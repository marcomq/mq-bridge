//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge
//
//! Thread-local gzip/zstd/lz4 codec pools shared by the HTTP endpoint and the batch codecs.
//!
//! Building an encoder costs far more than compressing a small payload: deflate state is
//! ~350 KiB in ~9 allocations (the 32 KiB window, two 64 KiB hash chains, the 64 KiB
//! literal buffer and an 83 KiB output buffer) and zstd's context is comparable. At
//! message rates that churn dominates the compression itself and pushes glibc into
//! trimming the arena back to the OS between messages. The state is reset and reused per
//! thread instead; the buffers stay resident for the life of the thread.

use std::cell::RefCell;
use std::thread::LocalKey;

/// gzip member header: magic, deflate, no flags/mtime, `xfl` level hint, OS unknown.
const fn gzip_header(xfl: u8) -> [u8; 10] {
    [0x1f, 0x8b, 0x08, 0, 0, 0, 0, 0, xfl, 0xff]
}

/// gzips `data` into a single member using `pool`'s reused deflate state.
/// `flate2`'s `GzEncoder` owns its compressor and cannot be reset, so the member is
/// framed here around a raw-deflate [`flate2::Compress`].
fn gzip_member(
    pool: &'static LocalKey<RefCell<flate2::Compress>>,
    xfl: u8,
    data: &[u8],
) -> std::io::Result<Vec<u8>> {
    pool.with(|encoder| {
        let mut encoder = encoder.borrow_mut();
        encoder.reset();

        let mut out = Vec::with_capacity(data.len() / 2 + 64);
        out.extend_from_slice(&gzip_header(xfl));
        loop {
            // `compress_vec` never grows `out`, so hand it spare capacity each round.
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(64));
            }
            let consumed = encoder.total_in() as usize;
            let status =
                encoder.compress_vec(&data[consumed..], &mut out, flate2::FlushCompress::Finish)?;
            match status {
                flate2::Status::StreamEnd => break,
                flate2::Status::Ok | flate2::Status::BufError => {
                    out.reserve(out.capacity().max(64))
                }
            }
        }

        let mut crc = flate2::Crc::new();
        crc.update(data);
        out.extend_from_slice(&crc.sum().to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        Ok(out)
    })
}

/// The level HTTP responses are compressed at. Measured on the HttpArena JSON payloads
/// (4–8 KB): level 4 is 24% smaller than level 1 for 1.9x the CPU, and captures 99% of
/// level 6's size gain at 79% of its cost. Levels 5–9 buy under 1% more for 7–21% more CPU.
#[cfg(feature = "http")]
const HTTP_GZIP_LEVEL: u32 = 4;

/// gzip at [`HTTP_GZIP_LEVEL`] — the level the HTTP endpoint compresses response bodies at.
#[cfg(feature = "http")]
pub(crate) fn gzip_http(data: &[u8]) -> std::io::Result<Vec<u8>> {
    thread_local! {
        static POOL: RefCell<flate2::Compress> = RefCell::new(flate2::Compress::new(
            flate2::Compression::new(HTTP_GZIP_LEVEL),
            false,
        ));
    }
    gzip_member(&POOL, 0, data)
}

/// gzip at `Compression::default()` — the level the batch codecs write members at.
/// Byte-identical to what `GzEncoder` produced at the same level.
#[cfg(feature = "compression")]
pub(crate) fn gzip_default(data: &[u8]) -> std::io::Result<Vec<u8>> {
    thread_local! {
        static POOL: RefCell<flate2::Compress> =
            RefCell::new(flate2::Compress::new(flate2::Compression::default(), false));
    }
    gzip_member(&POOL, 0, data)
}

/// zstd-compresses `data` into one frame with this thread's reused context.
/// `zstd::stream::encode_all` allocates and drops a fresh context per call.
pub(crate) fn zstd_pooled(data: &[u8], level: i32) -> std::io::Result<Vec<u8>> {
    thread_local! {
        static POOL: RefCell<Option<(i32, zstd::bulk::Compressor<'static>)>> =
            const { RefCell::new(None) };
    }
    POOL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let (cached_level, encoder) = match slot.as_mut() {
            Some(entry) => entry,
            None => slot.insert((level, zstd::bulk::Compressor::new(level)?)),
        };
        // Re-levelling only touches a context parameter, so a mixed-level thread still
        // reuses the same allocations.
        if *cached_level != level {
            encoder.set_compression_level(level)?;
            *cached_level = level;
        }
        encoder.compress(data)
    })
}

/// Largest payload the lz4 pool serves. `BlockSize::Auto` picks the block size from the
/// first payload an encoder sees and keeps it, so the pool pins 64 KiB blocks — the size
/// `Auto` would have chosen here — and anything larger takes a fresh encoder rather than
/// leaving 256 KiB or 4 MiB buffers resident on every worker thread to save a setup cost
/// the compression itself already dwarfs.
const LZ4_POOLED_MAX: usize = 64 * 1024;

/// lz4-compresses `data` into one frame, reusing this thread's encoder for small payloads.
/// A fresh `FrameEncoder` costs its 16 KiB match table plus block-sized input and output
/// buffers.
pub(crate) fn lz4_pooled(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use lz4_flex::frame::{BlockSize, FrameEncoder, FrameInfo};
    use std::io::Write as _;

    fn new_encoder() -> FrameEncoder<Vec<u8>> {
        FrameEncoder::with_frame_info(FrameInfo::new().block_size(BlockSize::Max64KB), Vec::new())
    }

    // An encoder that has already closed a frame writes no header for an empty payload,
    // so that case stays off the pool as well.
    if data.is_empty() || data.len() > LZ4_POOLED_MAX {
        let mut encoder = FrameEncoder::new(Vec::with_capacity(data.len() / 2 + 64));
        encoder.write_all(data)?;
        return Ok(encoder.finish()?);
    }

    thread_local! {
        static POOL: RefCell<Option<FrameEncoder<Vec<u8>>>> = const { RefCell::new(None) };
    }
    POOL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let encoder = match slot.as_mut() {
            Some(encoder) => encoder,
            None => slot.insert(new_encoder()),
        };
        encoder.get_mut().reserve(data.len() / 2 + 64);

        // A failure mid-frame leaves the frame open and the output half-written, which would
        // corrupt the next member; drop the encoder so the next call starts clean.
        let framed = (|| {
            encoder.write_all(data)?;
            encoder.try_finish()?;
            Ok::<_, std::io::Error>(std::mem::take(encoder.get_mut()))
        })();
        if framed.is_err() {
            *slot = None;
        }
        framed
    })
}

/// Decompresses concatenated zstd frames with this thread's reused context, stopping
/// once `limit` is passed. `zstd::stream::read::Decoder` builds a fresh context and its
/// own output buffer per call. The result may run one byte past `limit`, which is how
/// the caller tells a payload that merely reached the limit from one that exceeds it.
#[cfg(feature = "compression")]
pub(crate) fn zstd_pooled_decompress(data: &[u8], limit: Option<u64>) -> std::io::Result<Vec<u8>> {
    use zstd::stream::raw::{Decoder, InBuffer, Operation, OutBuffer};

    if data.is_empty() {
        return Ok(Vec::new());
    }
    let cap = limit.map_or(usize::MAX, |limit| {
        usize::try_from(limit.saturating_add(1)).unwrap_or(usize::MAX)
    });

    thread_local! {
        static POOL: RefCell<Option<Decoder<'static>>> = const { RefCell::new(None) };
    }
    POOL.with(|slot| {
        let mut slot = slot.borrow_mut();
        let decoder = match slot.as_mut() {
            Some(decoder) => decoder,
            None => slot.insert(Decoder::new()?),
        };
        // The previous call may have stopped mid-frame, on its limit or on an error.
        decoder.reinit()?;

        let mut out = Vec::with_capacity(
            data.len()
                .saturating_mul(4)
                .clamp(4096, 256 * 1024)
                .min(cap),
        );
        let mut consumed = 0;
        loop {
            if out.len() == out.capacity() {
                let target = out.capacity().saturating_mul(2).min(cap);
                out.reserve(target - out.len());
            }
            let filled = out.len();
            // `run` decodes into the spare capacity and extends the length as it fills.
            let (hint, read) = {
                let mut input = InBuffer::around(&data[consumed..]);
                let mut output = OutBuffer::around_pos(&mut out, filled);
                let hint = decoder.run(&mut input, &mut output)?;
                (hint, input.pos())
            };
            consumed += read;

            if hint == 0 {
                // Frame complete; a following member decodes with a fresh session.
                if consumed == data.len() {
                    return Ok(out);
                }
                decoder.reinit()?;
            } else if read == 0 && out.len() == filled {
                // Nothing read and nothing drained while the frame is still open: the
                // input is cut short. Returning here is also what rules out a spin.
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "incomplete frame",
                ));
            }
            // Checked last so the loop never re-enters with a full buffer it cannot grow.
            // `reserve` rounds capacity up and the decoder fills whatever it is handed,
            // so trim back to the one byte past `limit` the caller expects.
            if out.len() >= cap {
                out.truncate(cap);
                return Ok(out);
            }
        }
    })
}
