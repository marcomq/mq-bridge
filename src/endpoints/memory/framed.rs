//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Shared length-delimited framing for the IPC transports.
//!
//! Both the Unix socket and Windows named pipe transports speak the same wire
//! format: a 4-byte big-endian payload length followed by a MessagePack encoded
//! `Vec<CanonicalMessage>`. `LengthDelimitedCodec` defaults to exactly that
//! layout, so this is wire-compatible with the previous hand-rolled framing.
//!
//! Framing lives here rather than in each transport because `Framed` owns the
//! partial-frame buffer. A hand-rolled `read_exact` pair is not cancel safe: a
//! `select!` that drops the read future mid-frame consumes bytes and desyncs the
//! stream permanently. Routes cancel `receive_batch` on shutdown, so that path
//! was reachable.

use crate::CanonicalMessage;
use anyhow::{anyhow, Result};
use bytes::{Bytes, BytesMut};
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::warn;

/// Maximum size of a single serialized batch frame (100 MB).
const MAX_FRAME_BYTES: usize = 100 * 1024 * 1024;

/// Size of the length prefix, in bytes.
const LENGTH_PREFIX_BYTES: usize = 4;

/// How long a send may block before it is reported as stalled.
///
/// Socket buffers are small (8 KiB by default on macOS), so a batch that
/// outgrows one only completes once the consumer drains it. That is correct
/// backpressure, but an unexplained indefinite stall is not actionable, so we
/// log once and keep waiting.
const STALLED_SEND_WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// An IO handle wrapped in the length-delimited codec.
///
/// `maybe_partial` shadows the codec's private decode state. `LengthDelimitedCodec`
/// consumes the length prefix as soon as it has read one, so while it is mid-frame the
/// read buffer starts at payload bytes rather than at a prefix — which [`buffered_frames`]
/// cannot tell apart from a frame boundary. The flag is set pessimistically before every
/// read and cleared only once a frame has been decoded (which leaves the codec back at a
/// boundary), so a cancelled read stays pessimistic.
pub(crate) struct FramedIo<S> {
    inner: Framed<S, LengthDelimitedCodec>,
    maybe_partial: bool,
}

/// Wrap an IO handle in the codec used by every IPC transport.
pub(crate) fn wrap<S>(io: S) -> FramedIo<S>
where
    S: AsyncRead + AsyncWrite,
{
    FramedIo {
        inner: LengthDelimitedCodec::builder()
            .length_field_type::<u32>()
            .max_frame_length(MAX_FRAME_BYTES)
            .new_framed(io),
        maybe_partial: false,
    }
}

/// Encode and send a batch as a single frame. Returns the encoded byte count.
///
/// Blocks while the peer is not draining — that is the intended backpressure.
/// `peer` only labels the diagnostic logged if the send stalls.
pub(crate) async fn send_batch<S>(
    framed: &mut FramedIo<S>,
    messages: &[CanonicalMessage],
    peer: &str,
) -> Result<usize>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let data = rmp_serde::to_vec(messages)?;
    if data.len() > MAX_FRAME_BYTES {
        return Err(anyhow!(
            "Encoded batch is {} bytes, exceeding the {} byte frame limit",
            data.len(),
            MAX_FRAME_BYTES
        ));
    }
    let len = data.len();

    let mut send = std::pin::pin!(framed.inner.send(Bytes::from(data)));
    tokio::select! {
        result = &mut send => result?,
        _ = tokio::time::sleep(STALLED_SEND_WARN_AFTER) => {
            warn!(
                peer,
                bytes = len,
                count = messages.len(),
                stalled_for_secs = STALLED_SEND_WARN_AFTER.as_secs(),
                "IPC send is blocked: the batch is larger than the socket buffer and the \
                 consumer is not draining it. Check that the consumer side is running and \
                 reading. Still waiting."
            );
            send.await?
        }
    }

    Ok(len)
}

/// Await the next frame and decode it.
///
/// Cancel safe: partial frames stay buffered inside `framed`, so a dropped
/// future loses nothing and the next call resumes mid-frame.
pub(crate) async fn recv_batch<S>(framed: &mut FramedIo<S>) -> Result<Vec<CanonicalMessage>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    framed.maybe_partial = true;
    match framed.inner.next().await {
        Some(frame) => {
            let frame = frame?;
            framed.maybe_partial = false;
            decode(&frame)
        }
        // Clean EOF. Surfaced as an io error so callers' disconnect detection
        // treats it like any other peer drop.
        None => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()),
    }
}

/// Return the next frame if one can be produced without blocking.
///
/// Polls with a no-op waker, so a `Pending` read simply yields `None`; the next
/// real poll re-registers interest with the IO driver.
pub(crate) fn try_recv_batch<S>(framed: &mut FramedIo<S>) -> Result<Option<Vec<CanonicalMessage>>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    framed.maybe_partial = true;
    match framed.inner.next().now_or_never() {
        Some(Some(frame)) => {
            let frame = frame?;
            framed.maybe_partial = false;
            decode(&frame).map(Some)
        }
        Some(None) => Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into()),
        None => Ok(None),
    }
}

/// Count whole frames already sitting in the read buffer.
///
/// These are decodable without touching the socket. Walks the length prefixes
/// without decoding payloads.
///
/// A lower bound: while the codec may be mid-frame the buffer does not start at a length
/// prefix, so nothing can be counted and this reports 0.
pub(crate) fn buffered_frames<S>(framed: &FramedIo<S>) -> usize {
    if framed.maybe_partial {
        return 0;
    }
    let buf: &BytesMut = framed.inner.read_buffer();
    let mut offset = 0usize;
    let mut frames = 0usize;

    while buf.len() >= offset + LENGTH_PREFIX_BYTES {
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        prefix.copy_from_slice(&buf[offset..offset + LENGTH_PREFIX_BYTES]);
        let payload = u32::from_be_bytes(prefix) as usize;

        let end = match offset
            .checked_add(LENGTH_PREFIX_BYTES)
            .and_then(|o| o.checked_add(payload))
        {
            Some(end) => end,
            None => break,
        };
        if buf.len() < end {
            break;
        }

        offset = end;
        frames += 1;
    }

    frames
}

fn decode(frame: &[u8]) -> Result<Vec<CanonicalMessage>> {
    rmp_serde::from_slice(frame)
        .map_err(|e| anyhow!("Failed to decode IPC batch ({} bytes): {}", frame.len(), e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn roundtrip_preserves_batch() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = wrap(a);
        let mut reader = wrap(b);

        let sent = vec![
            CanonicalMessage::from_vec(b"one"),
            CanonicalMessage::from_vec(b"two"),
        ];
        send_batch(&mut writer, &sent, "test").await.unwrap();

        let received = recv_batch(&mut reader).await.unwrap();
        assert_eq!(received.len(), 2);
        assert_eq!(received[0].payload.as_ref(), b"one");
        assert_eq!(received[1].payload.as_ref(), b"two");
    }

    #[tokio::test]
    async fn try_recv_returns_none_when_idle() {
        let (a, b) = duplex(64 * 1024);
        let _writer = wrap(a);
        let mut reader = wrap(b);

        assert!(try_recv_batch(&mut reader).unwrap().is_none());
    }

    #[tokio::test]
    async fn try_recv_yields_a_buffered_frame() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = wrap(a);
        let mut reader = wrap(b);

        send_batch(&mut writer, &[CanonicalMessage::from_vec(b"ready")], "test")
            .await
            .unwrap();

        let received = try_recv_batch(&mut reader)
            .unwrap()
            .expect("a frame is already on the wire");
        assert_eq!(received[0].payload.as_ref(), b"ready");
    }

    #[tokio::test]
    async fn clean_eof_surfaces_as_unexpected_eof() {
        let (a, b) = duplex(64 * 1024);
        drop(a);
        let mut reader = wrap(b);

        let err = recv_batch(&mut reader).await.unwrap_err();
        let io_err = err
            .downcast_ref::<std::io::Error>()
            .expect("EOF should surface as an io error");
        assert_eq!(io_err.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    /// The regression this module exists for: dropping a read future mid-frame
    /// must not consume bytes. Previously `read_exact` swallowed the partial
    /// frame and desynced the stream for good.
    #[tokio::test]
    async fn cancelled_read_resumes_without_desync() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = wrap(a);
        let mut reader = wrap(b);

        let messages = vec![CanonicalMessage::from_vec(b"survives-cancellation")];
        let encoded = rmp_serde::to_vec(&messages).unwrap();

        // Write the length prefix and a slice of the payload, then stop.
        {
            use tokio::io::AsyncWriteExt;
            let io = writer.inner.get_mut();
            io.write_all(&(encoded.len() as u32).to_be_bytes())
                .await
                .unwrap();
            io.write_all(&encoded[..4]).await.unwrap();
            io.flush().await.unwrap();
        }

        // Cancel a read that can only see the partial frame.
        tokio::select! {
            _ = recv_batch(&mut reader) => panic!("frame is incomplete, read must not finish"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
        }

        // Deliver the rest; the buffered prefix must still be intact.
        {
            use tokio::io::AsyncWriteExt;
            let io = writer.inner.get_mut();
            io.write_all(&encoded[4..]).await.unwrap();
            io.flush().await.unwrap();
        }

        let received = recv_batch(&mut reader).await.unwrap();
        assert_eq!(received[0].payload.as_ref(), b"survives-cancellation");
    }

    #[tokio::test]
    async fn buffered_frames_counts_whole_frames_only() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = wrap(a);
        let mut reader = wrap(b);

        send_batch(&mut writer, &[CanonicalMessage::from_vec(b"a")], "test")
            .await
            .unwrap();
        send_batch(&mut writer, &[CanonicalMessage::from_vec(b"b")], "test")
            .await
            .unwrap();

        // Pull one frame; that read also buffers whatever else has arrived.
        let _ = recv_batch(&mut reader).await.unwrap();
        assert_eq!(buffered_frames(&reader), 1);

        let _ = recv_batch(&mut reader).await.unwrap();
        assert_eq!(buffered_frames(&reader), 0);
    }

    /// A half-arrived frame leaves the codec holding a consumed length prefix, so the
    /// read buffer starts at payload bytes. Those must never be counted as frames.
    #[tokio::test]
    async fn buffered_frames_ignores_a_partial_frame() {
        let (a, b) = duplex(64 * 1024);
        let mut writer = wrap(a);
        let mut reader = wrap(b);

        // A payload whose leading bytes would themselves read as a plausible length
        // prefix if mistaken for the start of a frame.
        let messages = vec![CanonicalMessage::from_vec([0u8; 64])];
        let encoded = rmp_serde::to_vec(&messages).unwrap();

        {
            use tokio::io::AsyncWriteExt;
            let io = writer.inner.get_mut();
            io.write_all(&(encoded.len() as u32).to_be_bytes())
                .await
                .unwrap();
            io.write_all(&encoded[..8]).await.unwrap();
            io.flush().await.unwrap();
        }

        // Drive a read so the codec consumes the prefix and parks mid-frame.
        assert!(try_recv_batch(&mut reader).unwrap().is_none());
        assert_eq!(buffered_frames(&reader), 0);

        // Once the frame completes, counting resumes.
        {
            use tokio::io::AsyncWriteExt;
            let io = writer.inner.get_mut();
            io.write_all(&encoded[8..]).await.unwrap();
            io.flush().await.unwrap();
        }
        let received = recv_batch(&mut reader).await.unwrap();
        assert_eq!(received[0].payload.len(), 64);
        assert_eq!(buffered_frames(&reader), 0);
    }
}
