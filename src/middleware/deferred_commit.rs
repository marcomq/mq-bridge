//! Commit handling for middleware that drops messages out of a batch.
//!
//! A consumer middleware that filters a batch down to nothing still has to
//! acknowledge what it dropped. On a source with cumulative acks, doing that
//! straight away jumps ahead of batches the route is still writing — the route's
//! ordered sequencer only sees the commits `receive_batch` returns — and a crash
//! in that window loses them. So on those sources the emptied batch's commit is
//! held and runs from inside the next retained batch's commit, which the
//! sequencer does order. While no retained batch is uncommitted there is nothing
//! to jump ahead of, so the commit runs at once and a middleware that drops
//! everything still advances the source.
//!
//! Shared by the `filter` and `deduplication` middlewares.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::traits::{BatchCommitFunc, MessageDisposition};

/// How many emptied-batch commits may be held before the oldest are released.
///
/// A middleware that drops everything for a long stretch would otherwise hold
/// one commit per batch read, for the whole read.
const MAX_DEFERRED_COMMITS: usize = 1024;

/// Commits for batches this middleware emptied, held back on sources that need
/// ordered commits.
///
/// Bounded at [`MAX_DEFERRED_COMMITS`], dropping the oldest plain ack; an entry that
/// carries a reply is never dropped, its requester is waiting for it. Releasing a held
/// commit without running it is the same at-least-once outcome as ending a drain
/// with commits still held — those messages are re-read and re-dropped — which is
/// why the bound costs correctness nothing. Keeping the newest is what makes it
/// cheap: on a cumulative-ack source that commit subsumes every one released
/// before it.
///
/// Behind a `Mutex` only to stay `Sync`, which `MessageConsumer` requires: a
/// boxed `FnOnce` is `Send` but not `Sync`. It is only ever reached through
/// `&mut self`, so `get_mut` suffices and nothing ever blocks.
#[derive(Default)]
pub(crate) struct DeferredCommits {
    held: Mutex<VecDeque<(BatchCommitFunc, Emptied)>>,
    in_flight: Arc<AtomicUsize>,
}

/// A retained delivery handed to the caller whose commit has not finished.
pub(crate) struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    /// Call once the delivery's commit succeeded. A commit that fails or never runs
    /// stays counted, which keeps later emptied batches from committing past it.
    pub(crate) fn settled(self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What a held commit settles its dropped messages with. Plain acks stay a count, so a
/// middleware that drops everything for a long stretch holds no per-message state.
pub(crate) enum Emptied {
    Acked(usize),
    Settled(Vec<MessageDisposition>),
}

impl Emptied {
    fn into_dispositions(self) -> Vec<MessageDisposition> {
        match self {
            Emptied::Acked(dropped) => vec![MessageDisposition::Ack; dropped],
            Emptied::Settled(dispositions) => dispositions,
        }
    }
}

impl DeferredCommits {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn queue(&mut self) -> &mut VecDeque<(BatchCommitFunc, Emptied)> {
        self.held
            .get_mut()
            .expect("held commits are only reached through &mut self, never locked")
    }

    /// Counts a retained delivery as uncommitted until [`InFlight::settled`].
    pub(crate) fn track(&self) -> InFlight {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        InFlight(Arc::clone(&self.in_flight))
    }

    /// Acknowledges a batch this middleware emptied.
    ///
    /// On a source needing ordered commits the commit is held while a retained
    /// batch is uncommitted; otherwise it runs now.
    #[cfg_attr(not(feature = "filter"), allow(dead_code))]
    pub(crate) async fn ack_emptied(
        &mut self,
        ordered: bool,
        commit: BatchCommitFunc,
        dropped: usize,
    ) -> anyhow::Result<()> {
        self.hold(ordered, commit, Emptied::Acked(dropped)).await
    }

    /// Like [`Self::ack_emptied`], for a batch whose dropped messages are not all plain acks
    /// — a deduplicated request answered with its stored reply.
    pub(crate) async fn settle_emptied(
        &mut self,
        ordered: bool,
        commit: BatchCommitFunc,
        dispositions: Vec<MessageDisposition>,
    ) -> anyhow::Result<()> {
        let emptied = if dispositions
            .iter()
            .all(|d| matches!(d, MessageDisposition::Ack))
        {
            Emptied::Acked(dispositions.len())
        } else {
            Emptied::Settled(dispositions)
        };
        self.hold(ordered, commit, emptied).await
    }

    async fn hold(
        &mut self,
        ordered: bool,
        commit: BatchCommitFunc,
        emptied: Emptied,
    ) -> anyhow::Result<()> {
        if !ordered {
            return commit(emptied.into_dispositions()).await;
        }
        let idle = self.in_flight.load(Ordering::SeqCst) == 0;
        let queue = self.queue();
        if !idle && queue.len() >= MAX_DEFERRED_COMMITS {
            let oldest_ack = queue
                .iter()
                .position(|(_, emptied)| matches!(emptied, Emptied::Acked(_)));
            if let Some(oldest_ack) = oldest_ack {
                queue.remove(oldest_ack);
            }
        }
        queue.push_back((commit, emptied));
        if idle {
            // Popped one at a time, so a cancelled caller loses at most the one running.
            while let Some((commit, emptied)) = self.queue().pop_front() {
                commit(emptied.into_dispositions()).await?;
            }
        }
        Ok(())
    }

    /// Hands the held commits to the caller, to be run from inside the commit of
    /// the next batch that did retain something. See [`run_all`].
    pub(crate) fn take(&mut self) -> VecDeque<(BatchCommitFunc, Emptied)> {
        std::mem::take(self.queue())
    }

    /// Hands held commits to lifecycle code that only has a shared consumer reference.
    pub(crate) fn take_shared(&self) -> VecDeque<(BatchCommitFunc, Emptied)> {
        std::mem::take(&mut *self.held.lock().expect("deferred commit lock poisoned"))
    }
}

/// Runs commits handed over by [`DeferredCommits::take`], oldest first.
///
/// Call this *before* the retained batch's own commit, so the acks stay in the
/// order the source produced them.
pub(crate) async fn run_all(held: VecDeque<(BatchCommitFunc, Emptied)>) -> anyhow::Result<()> {
    for (commit, emptied) in held {
        commit(emptied.into_dispositions()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn counting_commit(seen: Arc<AtomicUsize>) -> BatchCommitFunc {
        Box::new(move |dispositions| {
            Box::pin(async move {
                seen.fetch_add(dispositions.len(), Ordering::Relaxed);
                Ok(())
            })
        })
    }

    #[tokio::test]
    async fn an_unordered_source_is_acknowledged_immediately() {
        let seen = Arc::new(AtomicUsize::new(0));
        let mut deferred = DeferredCommits::new();
        deferred
            .ack_emptied(false, counting_commit(seen.clone()), 7)
            .await
            .unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 7);
        assert!(deferred.take().is_empty());
    }

    #[tokio::test]
    async fn an_ordered_source_holds_the_commit_until_it_is_taken() {
        let seen = Arc::new(AtomicUsize::new(0));
        let mut deferred = DeferredCommits::new();
        let _uncommitted = deferred.track();
        deferred
            .ack_emptied(true, counting_commit(seen.clone()), 4)
            .await
            .unwrap();
        assert_eq!(
            seen.load(Ordering::Relaxed),
            0,
            "must not ack ahead of the route"
        );

        run_all(deferred.take()).await.unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 4);
    }

    /// The bound keeps the newest commit, which on a cumulative-ack source
    /// subsumes the ones dropped before it.
    #[tokio::test]
    async fn held_commits_are_bounded_and_drop_the_oldest() {
        let seen = Arc::new(AtomicUsize::new(0));
        let executed = Arc::new(Mutex::new(Vec::new()));
        let mut deferred = DeferredCommits::new();
        let _uncommitted = deferred.track();
        for id in 0..MAX_DEFERRED_COMMITS + 10 {
            let seen = seen.clone();
            let executed = executed.clone();
            let commit: BatchCommitFunc = Box::new(move |dispositions| {
                Box::pin(async move {
                    seen.fetch_add(dispositions.len(), Ordering::Relaxed);
                    executed.lock().unwrap().push(id);
                    Ok(())
                })
            });
            deferred.ack_emptied(true, commit, 1).await.unwrap();
        }
        let held = deferred.take();
        assert_eq!(held.len(), MAX_DEFERRED_COMMITS);
        run_all(held).await.unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), MAX_DEFERRED_COMMITS);
        assert_eq!(
            *executed.lock().unwrap(),
            (10..MAX_DEFERRED_COMMITS + 10).collect::<Vec<_>>()
        );
    }

    /// With nothing retained and uncommitted, an ordered source advances batch by batch.
    #[tokio::test]
    async fn an_ordered_source_commits_at_once_while_nothing_is_uncommitted() {
        let seen = Arc::new(AtomicUsize::new(0));
        let mut deferred = DeferredCommits::new();
        deferred
            .ack_emptied(true, counting_commit(seen.clone()), 3)
            .await
            .unwrap();
        assert_eq!(seen.load(Ordering::Relaxed), 3);

        let retained = deferred.track();
        deferred
            .ack_emptied(true, counting_commit(seen.clone()), 2)
            .await
            .unwrap();
        assert_eq!(
            seen.load(Ordering::Relaxed),
            3,
            "held behind the retained batch"
        );

        retained.settled();
        deferred
            .ack_emptied(true, counting_commit(seen.clone()), 1)
            .await
            .unwrap();
        assert_eq!(
            seen.load(Ordering::Relaxed),
            6,
            "the held commit runs first"
        );
        assert!(deferred.take().is_empty());
    }

    #[tokio::test]
    async fn the_bound_never_drops_a_held_reply() {
        let seen = Arc::new(AtomicUsize::new(0));
        let mut deferred = DeferredCommits::new();
        let _uncommitted = deferred.track();
        let reply = || {
            vec![MessageDisposition::Reply(crate::CanonicalMessage::from(
                "stored",
            ))]
        };
        deferred
            .settle_emptied(true, counting_commit(seen.clone()), reply())
            .await
            .unwrap();
        for _ in 0..MAX_DEFERRED_COMMITS + 10 {
            deferred
                .ack_emptied(true, counting_commit(seen.clone()), 1)
                .await
                .unwrap();
        }
        let held = deferred.take();
        assert_eq!(held.len(), MAX_DEFERRED_COMMITS);
        assert!(matches!(held[0].1, Emptied::Settled(_)));
    }
}
