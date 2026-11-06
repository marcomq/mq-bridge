//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Committing in source order: sequencer, commit router, response mapping.

use super::*;

pub(super) type SequencerItem = (
    Vec<MessageDisposition>,
    BatchCommitFunc,
    tokio::sync::oneshot::Sender<anyhow::Result<()>>,
);

/// Closes the work channel when its worker exits, however it exits.
pub(super) struct CloseOnDrop<T>(pub(super) async_channel::Receiver<T>);

/// Hands the right to call `send_batch` from one batch to the next when the sink
/// declares [`MessagePublisher::requires_ordered_publish`]. `prev` resolves once the
/// preceding batch's send returned (or its worker went away); dropping `_release` lets
/// the batch behind it in. Only the `send_batch` call is serialised — per-batch prep,
/// disposition mapping and commits still run concurrently across the worker pool.
pub(super) struct OrderTicket {
    pub(super) prev: Option<tokio::sync::oneshot::Receiver<()>>,
    pub(super) _release: tokio::sync::oneshot::Sender<()>,
}

/// Routes batch commits either through the ordered sequencer (cumulative-ack
/// transports) or concurrently under a semaphore (individual-ack transports),
/// chosen once per route from the consumer's `commit_requires_order()`.
pub(super) enum CommitRouter {
    Ordered {
        seq_tx: Sender<(u64, SequencerItem)>,
        handle: JoinHandle<()>,
        /// Batches read but not yet committed. See [`CommitRouter::read_semaphore`].
        in_flight: Arc<tokio::sync::Semaphore>,
        capacity: usize,
    },
    Unordered {
        semaphore: Arc<tokio::sync::Semaphore>,
        capacity: usize,
    },
}

impl<T> Drop for CloseOnDrop<T> {
    fn drop(&mut self) {
        self.0.close();
    }
}

pub(super) fn spawn_sequencer(
    buffer_size: usize,
) -> (Sender<(u64, SequencerItem)>, JoinHandle<()>) {
    let (seq_tx, seq_rx) = bounded::<(u64, SequencerItem)>(buffer_size);
    let sequencer_handle = tokio::spawn(async move {
        let mut buffer: BTreeMap<u64, SequencerItem> = BTreeMap::new();
        let mut next_seq = 0u64;

        loop {
            // If we have the next item in sequence, execute its commit directly.
            // Using a plain await (no select!) here is essential: if we raced a recv
            // against the commit future, a recv win would drop the commit future and
            // the notify sender, leaving the caller permanently blocked while next_seq
            // stays unadvanced — a deadlock.
            if let Some((dispositions, commit_func, notify)) = buffer.remove(&next_seq) {
                let result = commit_func(dispositions).await;
                let _ = notify.send(result);
                next_seq += 1;
                // Yield to allow other tasks to run, preventing busy-loop when buffer has many messages
                tokio::task::yield_now().await;
                continue;
            }

            // Wait for the next item from any worker.
            match seq_rx.recv().await {
                Ok((seq, item)) => {
                    if seq < next_seq {
                        let (_, _, notify) = item;
                        trace!(
                            seq,
                            next_seq,
                            "Sequencer received late item (seq < next_seq)"
                        );
                        let _ = notify.send(Err(anyhow::anyhow!(
                            "Sequencer received late item (seq {} < next_seq {})",
                            seq,
                            next_seq
                        )));
                    } else {
                        buffer.insert(seq, item);
                    }
                }
                Err(_) => {
                    // seq_tx was dropped — drain and notify any remaining buffered commits.
                    for (_, (_, _, notify)) in buffer {
                        let _ = notify.send(Err(anyhow::anyhow!("Sequencer is shutting down")));
                    }
                    break;
                }
            }
        }
    });
    (seq_tx, sequencer_handle)
}

pub(super) fn wrap_commit(
    commit: BatchCommitFunc,
    seq: u64,
    seq_tx: Sender<(u64, SequencerItem)>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
) -> BatchCommitFunc {
    Box::new(move |dispositions| {
        Box::pin(async move {
            // Held until the commit has run, or released when it is dropped unrun.
            let _permit = permit;
            let (notify_tx, notify_rx) = tokio::sync::oneshot::channel();
            if seq_tx
                .send((seq, (dispositions, commit, notify_tx)))
                .await
                .is_ok()
            {
                match notify_rx.await {
                    Ok(res) => res,
                    Err(_) => Err(anyhow::anyhow!(
                        "Sequencer dropped the commit channel unexpectedly"
                    )),
                }
            } else {
                Err(anyhow::anyhow!(
                    "Failed to send commit to sequencer, route is likely shutting down"
                ))
            }
        })
    })
}

impl CommitRouter {
    /// `sending` is how many batches the runner can hold before their commit is queued.
    pub(super) fn new(ordered: bool, commit_concurrency_limit: usize, sending: usize) -> Self {
        let commit_concurrency_limit = commit_concurrency_limit.max(1);
        if ordered {
            let (seq_tx, handle) = spawn_sequencer(commit_concurrency_limit);
            let in_flight = commit_concurrency_limit.saturating_add(sending);
            CommitRouter::Ordered {
                seq_tx,
                handle,
                in_flight: Arc::new(tokio::sync::Semaphore::new(in_flight)),
                capacity: in_flight,
            }
        } else {
            CommitRouter::Unordered {
                semaphore: Arc::new(tokio::sync::Semaphore::new(commit_concurrency_limit)),
                capacity: commit_concurrency_limit,
            }
        }
    }

    /// Starts a [`FlushGate`] when the publisher defers its commits to a flush. It flushes
    /// once half the commit slots are held, well before the reader would block on them.
    pub(super) fn flush_gate(
        &self,
        publisher: &Arc<dyn MessagePublisher>,
        err_tx: &Sender<anyhow::Error>,
    ) -> Option<FlushGate> {
        let interval = publisher.deferred_commit_interval()?;
        let (capacity, sequencer, read_permits) = match self {
            CommitRouter::Ordered {
                seq_tx,
                capacity,
                in_flight,
                ..
            } => (*capacity, Some(seq_tx.clone()), Some(Arc::clone(in_flight))),
            CommitRouter::Unordered { capacity, .. } => (*capacity, None, None),
        };
        Some(FlushGate::spawn(
            Arc::clone(publisher),
            interval,
            capacity / 2,
            sequencer,
            read_permits,
            err_tx.clone(),
        ))
    }

    /// Wraps a batch commit for its dispatch mode. `seq` and `permit` are only used by
    /// the ordered path; they are ignored when commits run concurrently.
    pub(super) fn wrap(
        &self,
        commit: BatchCommitFunc,
        seq: u64,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> BatchCommitFunc {
        match self {
            CommitRouter::Ordered { seq_tx, .. } => {
                wrap_commit(commit, seq, seq_tx.clone(), permit)
            }
            // Unordered commits need no wrapping: the dispatcher acquires a
            // `commit_concurrency_limit` permit *before* spawning each commit
            // (see `dispatch_semaphore`), so backpressure applies at queue time
            // instead of letting blocked commit tasks pile up unbounded.
            CommitRouter::Unordered { .. } => commit,
        }
    }

    /// Semaphore that bounds how many unordered commits may be queued/in-flight at
    /// once. The dispatcher acquires a permit before spawning a commit task and
    /// holds it for the task's lifetime. `None` for the ordered path, which is
    /// bounded where batches are read (see `read_semaphore`).
    pub(super) fn dispatch_semaphore(&self) -> Option<Arc<tokio::sync::Semaphore>> {
        match self {
            CommitRouter::Ordered { .. } => None,
            CommitRouter::Unordered { semaphore, .. } => Some(Arc::clone(semaphore)),
        }
    }

    /// Semaphore the runner takes a permit from before it reads a batch whose commit is
    /// ordered; the permit travels with the commit. Taking it after the send instead could
    /// deadlock: later batches would hold every permit while waiting for an earlier one.
    pub(super) fn read_semaphore(&self) -> Option<Arc<tokio::sync::Semaphore>> {
        match self {
            CommitRouter::Ordered { in_flight, .. } => Some(Arc::clone(in_flight)),
            CommitRouter::Unordered { .. } => None,
        }
    }

    /// Gives up on the commit sequence after a gap: the sequencer still commits what is in
    /// order, then fails every commit behind the gap instead of waiting on it.
    pub(super) fn abandon(&self) {
        if let CommitRouter::Ordered { seq_tx, .. } = self {
            seq_tx.close();
        }
    }

    /// Tears down the sequencer (if any). Call only after all workers and their
    /// in-flight commit tasks have drained, so the sequencer's senders are gone.
    pub(super) async fn shutdown(self) {
        if let CommitRouter::Ordered { seq_tx, handle, .. } = self {
            drop(seq_tx);
            let _ = handle.await;
        }
    }
}

/// Acquires a commit-dispatch permit before a commit task is spawned, so the
/// caller blocks (applies backpressure) once `commit_concurrency_limit` commits
/// are already queued rather than spawning unbounded tasks. The returned permit is
/// held for the spawned task's lifetime. `None` (no semaphore, or a closed one
/// during shutdown) means the commit runs unbounded.
pub(super) async fn acquire_commit_permit(
    semaphore: Option<&Arc<tokio::sync::Semaphore>>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    match semaphore {
        Some(sem) => Arc::clone(sem).acquire_owned().await.ok(),
        None => None,
    }
}

/// Reads the next batch, first waiting for room to commit it when commits are ordered.
/// A permit left in `permit` by a cancelled or empty read is reused.
pub(super) async fn receive_with_permit(
    consumer: &mut dyn MessageConsumer,
    batch_size: usize,
    semaphore: Option<&Arc<tokio::sync::Semaphore>>,
    permit: &mut Option<tokio::sync::OwnedSemaphorePermit>,
    flush_gate: Option<&FlushGate>,
) -> Result<crate::traits::ReceivedBatch, ConsumerError> {
    if permit.is_none() {
        if let Some(sem) = semaphore {
            *permit = Arc::clone(sem).try_acquire_owned().ok();
            if permit.is_none() {
                // Out of room: what is held at the gate may be what the reader waits on.
                if let Some(gate) = flush_gate {
                    gate.nudge();
                }
                *permit = acquire_commit_permit(semaphore).await;
            }
        }
    }
    consumer.receive_batch(batch_size).await
}

pub(super) fn warn_reply_missing(id: u128) {
    warn!("Message {id:032x} expected a reply (reply_to set), but the output only acknowledged it. Committing it without a reply.");
}

pub(super) fn map_responses_to_dispositions(
    message_ids: &[u128],
    responses: Option<Vec<crate::CanonicalMessage>>,
    failed: &[(crate::CanonicalMessage, PublisherError)],
    request_ids: &std::collections::HashSet<u128>,
    has_dlq_middleware: bool,
) -> Vec<MessageDisposition> {
    let len = message_ids.len();
    if responses.is_none() && failed.is_empty() && (len == 0 || request_ids.is_empty()) {
        return vec![MessageDisposition::Ack; len];
    }

    // Fast path for single message batches (very common for high-concurrency low-batch setups)
    if len == 1 {
        let id = message_ids[0];
        if let Some((_, error)) = failed.first() {
            return vec![
                if matches!(error, PublisherError::NonRetryable(_)) && !has_dlq_middleware {
                    MessageDisposition::Ack
                } else {
                    MessageDisposition::Nack
                },
            ];
        }
        if let Some(mut resps) = responses {
            if let Some(resp) = resps.pop() {
                return vec![MessageDisposition::Reply(resp)];
            }
        }
        if request_ids.contains(&id) {
            warn_reply_missing(id);
        }
        return vec![MessageDisposition::Ack];
    }

    let mut dispositions = Vec::with_capacity(len);
    // Build failed_ids manually to avoid collect() overhead
    let mut failed_ids = std::collections::HashMap::with_capacity(failed.len());
    for (message, error) in failed {
        failed_ids.insert(message.message_id, error);
    }

    // Create a map from message_id to response message for efficient lookup.
    let mut response_map: std::collections::HashMap<u128, crate::CanonicalMessage> = responses
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.message_id, r))
        .collect();

    for id in message_ids {
        if let Some(error) = failed_ids.get(id) {
            dispositions.push(
                if matches!(error, PublisherError::NonRetryable(_)) && !has_dlq_middleware {
                    MessageDisposition::Ack
                } else {
                    MessageDisposition::Nack
                },
            );
        } else if let Some(resp) = response_map.remove(id) {
            // If a response exists for this specific ID, use it.
            dispositions.push(MessageDisposition::Reply(resp));
        } else {
            // The sink accepted it. A nack would redeliver a request it would only ack again.
            if request_ids.contains(id) {
                warn_reply_missing(*id);
            }
            dispositions.push(MessageDisposition::Ack);
        }
    }
    dispositions
}

#[cfg(test)]
pub(super) fn test_map_responses_to_dispositions_logic() {
    use crate::{traits::PublisherError, CanonicalMessage};
    use anyhow::anyhow;

    let ids = vec![1, 2, 3, 4];

    let mut resp1 = CanonicalMessage::from("resp1");
    resp1.message_id = 1;
    let mut resp4 = CanonicalMessage::from("resp4");
    resp4.message_id = 4;

    let responses = Some(vec![
        resp1, // Corresponds to id 1
        resp4, // Corresponds to id 4
    ]);

    let mut msg2 = CanonicalMessage::from("msg2");
    msg2.message_id = 2;
    let failed = vec![(msg2, PublisherError::NonRetryable(anyhow!("failed")))];

    let mut request_ids = std::collections::HashSet::new();
    request_ids.insert(3); // id 3 expects a reply but won't get one
    let dispositions = map_responses_to_dispositions(&ids, responses, &failed, &request_ids, false);

    assert_eq!(dispositions.len(), 4);
    assert!(matches!(dispositions[0], MessageDisposition::Reply(_))); // from responses
    assert!(matches!(dispositions[1], MessageDisposition::Ack)); // permanent failure dropped
    assert!(matches!(dispositions[2], MessageDisposition::Ack)); // accepted, no reply
    assert!(matches!(dispositions[3], MessageDisposition::Reply(_))); // from responses

    let mut dlq_msg = CanonicalMessage::from("msg2");
    dlq_msg.message_id = 2;
    let dlq_failed = vec![(
        dlq_msg,
        PublisherError::NonRetryable(anyhow!("DLQ send failed")),
    )];
    let dlq_dispositions =
        map_responses_to_dispositions(&ids, None, &dlq_failed, &request_ids, true);
    assert!(matches!(dlq_dispositions[1], MessageDisposition::Nack));
}
