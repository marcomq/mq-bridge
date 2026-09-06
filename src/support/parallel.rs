//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Splitting a batch of independent per-message work across cores.
//!
//! Two callers share the thresholds here: the file reader's record decoder, which owns
//! the worker pool below, and [`map_messages`], which spreads a middleware batch over
//! tokio's blocking pool. A sink that requires ordered publishing serializes every
//! `send_batch` call and a middleware wrapping it works inside that serialized region,
//! so route `concurrency` cannot help it — the batch itself is what gets split.
//!
//! Both routes concatenate results in chunk order, so a caller cannot tell a parallel
//! run from a sequential one: same messages, same order, same outcomes.

use crate::CanonicalMessage;

/// Batches below this stay on the caller's thread — splitting one costs more in thread
/// wake-ups than the work it saves.
///
/// Set from `csv_to_json_bench`'s `csv_batch_decode` on an 8-core M1: at 64 records a
/// split still lost to a sequential decode (2.35 vs 2.69 Melem/s) and only turned a
/// profit from 256 up (3.09). Decode is the cheapest per-record work that takes this
/// path, so a threshold that pays there pays for the heavier middleware work too.
pub(crate) const MIN_PARALLEL_BATCH: usize = 256;

/// Smallest slice worth handing to another thread.
pub(crate) const MIN_CHUNK: usize = 32;

/// Cores available to this process, resolved once.
pub(crate) fn parallelism() -> usize {
    static CORES: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CORES.get_or_init(|| {
        std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1)
    })
}

/// How many pieces a batch of `len` items is worth splitting into.
pub(crate) fn chunk_count(len: usize) -> usize {
    if len < MIN_PARALLEL_BATCH {
        return 1;
    }
    parallelism().min(len.div_ceil(MIN_CHUNK)).max(1)
}

/// A fixed set of worker threads, started once and reused for every batch.
///
/// Spawning a thread costs ~30µs here — enough that a per-batch `std::thread::scope`
/// loses to a plain sequential decode at any batch below a few hundred records. Workers
/// outlive the batches instead, so submitting one costs a channel push and a wake.
pub(crate) struct Pool {
    shared: std::sync::Arc<Shared>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

struct Shared {
    queue: std::sync::Mutex<std::collections::VecDeque<Job>>,
    ready: std::sync::Condvar,
}

impl Pool {
    fn new(workers: usize) -> Self {
        let shared = std::sync::Arc::new(Shared {
            queue: std::sync::Mutex::new(std::collections::VecDeque::new()),
            ready: std::sync::Condvar::new(),
        });
        for _ in 0..workers {
            let shared = std::sync::Arc::clone(&shared);
            std::thread::Builder::new()
                .name("mqb-decode".to_string())
                .spawn(move || loop {
                    let job = {
                        let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            match queue.pop_front() {
                                Some(job) => break job,
                                None => {
                                    queue =
                                        shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner())
                                }
                            }
                        }
                    };
                    job();
                })
                .expect("decode worker thread");
        }
        Self { shared }
    }

    /// Queues `job` for the next free worker.
    pub(crate) fn submit(&self, job: Job) {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(job);
        self.shared.ready.notify_one();
    }
}

/// The process-wide decode pool, started on first use.
///
/// One pool rather than one per reader: several file routes would otherwise each hold a
/// full set of threads and oversubscribe the machine.
///
/// One worker short of the core count, because the thread submitting a batch keeps a
/// chunk for itself rather than blocking on the others.
pub(crate) fn pool() -> &'static Pool {
    static POOL: std::sync::OnceLock<Pool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| Pool::new(pool_workers()))
}

fn pool_workers() -> usize {
    parallelism().saturating_sub(1).max(1)
}

/// How many pieces the decode pool is worth splitting a batch of `len` records into.
///
/// One more than the pool has workers: the submitting thread keeps a chunk itself
/// rather than blocking on the others.
pub(crate) fn decode_chunk_count(len: usize) -> usize {
    chunk_count(len).min(pool_workers() + 1)
}

/// Applies `f` to every message, on the blocking pool in contiguous chunks when the batch
/// is large enough to pay for the split.
///
/// A panic inside `f` is resumed on the caller's thread, so it surfaces exactly as it
/// would have from a sequential loop.
pub(crate) async fn map_messages<T, F>(messages: Vec<CanonicalMessage>, f: F) -> Vec<T>
where
    T: Send + 'static,
    F: Fn(CanonicalMessage) -> T + Send + Sync + 'static,
{
    let chunks = chunk_count(messages.len());
    if chunks <= 1 {
        return messages.into_iter().map(f).collect();
    }

    let per_chunk = messages.len().div_ceil(chunks);
    let mut parts = messages.chunks(per_chunk).len();
    let mut remaining = messages;
    let mut tail = Vec::with_capacity(parts.saturating_sub(1));
    // Split off every chunk but the first, which this thread keeps: one fewer hand-off,
    // and the caller's thread would otherwise just block waiting.
    while parts > 1 {
        let at = remaining.len() - per_chunk.min(remaining.len());
        tail.push(remaining.split_off(at));
        parts -= 1;
    }
    tail.reverse();

    let f = std::sync::Arc::new(f);
    let handles: Vec<_> = tail
        .into_iter()
        .map(|part| {
            let f = std::sync::Arc::clone(&f);
            tokio::task::spawn_blocking(move || part.into_iter().map(|m| f(m)).collect::<Vec<T>>())
        })
        .collect();

    let mut out: Vec<T> = remaining.into_iter().map(|m| f(m)).collect();
    for handle in handles {
        match handle.await {
            Ok(part) => out.extend(part),
            // Started blocking work cannot be cancelled, so a panic is the usual case;
            // a chunk still queued when the runtime shuts down comes back cancelled, and
            // its messages are gone with it, so neither may return a short batch.
            Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
            Err(error) => panic!("batch chunk was cancelled by runtime shutdown: {error}"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_batches_are_not_split() {
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(MIN_PARALLEL_BATCH - 1), 1);
        assert!(chunk_count(4096) >= 1);
        // Never more chunks than there is work to fill them with.
        assert!(chunk_count(MIN_PARALLEL_BATCH) <= MIN_PARALLEL_BATCH / MIN_CHUNK);
    }

    fn batch(n: usize) -> Vec<CanonicalMessage> {
        (0..n)
            .map(|i| CanonicalMessage::new(i.to_string().into_bytes(), None))
            .collect()
    }

    /// Order is the whole contract: a parallel run must be indistinguishable from a
    /// sequential one, at every size around the split thresholds.
    #[tokio::test]
    async fn results_come_back_in_input_order_at_every_size() {
        for n in [0, 1, 31, 63, 64, 65, 127, 128, 1000, 1024, 4097] {
            let out = map_messages(batch(n), |message| {
                String::from_utf8(message.payload.to_vec()).unwrap()
            })
            .await;
            let expected: Vec<String> = (0..n).map(|i| i.to_string()).collect();
            assert_eq!(out, expected, "batch of {n} came back out of order");
        }
    }
}
