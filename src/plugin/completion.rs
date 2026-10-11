//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Awaits the non-blocking ABI 1.2 calls without tying up a thread.
//!
//! The out-parameters of a call live in a heap block handed to the plugin as
//! the completion's `ctx`. The callback moves them into a oneshot; if the host
//! stopped waiting, it releases them on the host's blocking pool instead, never
//! on the plugin's thread.

use std::ffi::c_void;

use anyhow::anyhow;
use tokio::sync::oneshot;

use crate::support::plugin_abi::{MqbCompletion, MqbStatus, MQB_OK};

/// The out-parameters of one call, plus whatever must outlive it.
pub(super) trait Slots: Send + 'static {
    /// Releases what the plugin handed back to a caller that is gone.
    fn abandon(self);
}

struct Pending<S: Slots> {
    host: Option<tokio::runtime::Handle>,
    done: oneshot::Sender<(MqbStatus, Completed<S>)>,
    slots: S,
}

/// Slots on their way to the waiter. Abandoned when nobody takes them: the waiter may be
/// gone already, or be cancelled between the completion and its next poll.
struct Completed<S: Slots> {
    host: Option<tokio::runtime::Handle>,
    slots: Option<S>,
}

impl<S: Slots> Completed<S> {
    fn take(mut self) -> S {
        self.slots.take().expect("slots are taken once")
    }
}

impl<S: Slots> Drop for Completed<S> {
    fn drop(&mut self) {
        let Some(slots) = self.slots.take() else {
            return;
        };
        match self.host.take() {
            Some(host) => drop(host.spawn_blocking(move || slots.abandon())),
            None => slots.abandon(),
        }
    }
}

unsafe extern "C" fn complete<S: Slots>(ctx: *mut c_void, status: MqbStatus) {
    let pending = unsafe { Box::from_raw(ctx.cast::<Pending<S>>()) };
    let Pending { host, done, slots } = *pending;
    let completed = Completed {
        host,
        slots: Some(slots),
    };
    // A failed send hands the value back and drops it here, which abandons it.
    let _ = done.send((status, completed));
}

/// A started call; [`Started::finish`] awaits its completion.
pub(super) struct Started<S: Slots> {
    failed: Option<(MqbStatus, S)>,
    finished: oneshot::Receiver<(MqbStatus, Completed<S>)>,
}

impl<S: Slots> Started<S> {
    /// The slots back, if the starting call refused with `status`; no await needed.
    pub(super) fn refused_with(&mut self, status: MqbStatus) -> Option<S> {
        match self.failed.take() {
            Some((refused, slots)) if refused == status => Some(slots),
            other => {
                self.failed = other;
                None
            }
        }
    }

    pub(super) async fn finish(self) -> anyhow::Result<(MqbStatus, S)> {
        match self.failed {
            Some(failed) => Ok(failed),
            None => self
                .finished
                .await
                .map(|(status, completed)| (status, completed.take()))
                .map_err(|_| anyhow!("endpoint plugin dropped a call without completing it")),
        }
    }
}

/// Starts a call now.
///
/// `start` gets a raw pointer into the heap block, not a reference: the plugin
/// may complete, and move the slots out, before `start` returns.
pub(super) fn call<S: Slots>(
    slots: S,
    start: impl FnOnce(*mut S, MqbCompletion) -> MqbStatus,
) -> Started<S> {
    let (done, finished) = oneshot::channel();
    let pending = Box::into_raw(Box::new(Pending {
        host: tokio::runtime::Handle::try_current().ok(),
        done,
        slots,
    }));
    let completion = MqbCompletion {
        callback: complete::<S>,
        ctx: pending.cast(),
    };
    let status = start(
        unsafe { std::ptr::addr_of_mut!((*pending).slots) },
        completion,
    );
    // Any other status means the callback never runs, so the block is ours again.
    let failed = (status != MQB_OK).then(|| (status, unsafe { Box::from_raw(pending) }.slots));
    Started { failed, finished }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::plugin_abi::MQB_ERR_RETRYABLE;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    struct Probe {
        value: u32,
        abandoned: Arc<AtomicBool>,
    }

    impl Slots for Probe {
        fn abandon(self) {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }

    fn probe() -> (Probe, Arc<AtomicBool>) {
        let abandoned = Arc::new(AtomicBool::new(false));
        let slots = Probe {
            value: 0,
            abandoned: Arc::clone(&abandoned),
        };
        (slots, abandoned)
    }

    #[tokio::test]
    async fn a_call_may_complete_before_it_returns() {
        let (slots, _) = probe();
        let started = call(slots, |slots, completion| unsafe {
            (*slots).value = 7;
            (completion.callback)(completion.ctx, MQB_OK);
            MQB_OK
        });
        let (status, slots) = started.finish().await.unwrap();
        assert_eq!((status, slots.value), (MQB_OK, 7));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_completes_from_another_thread() {
        let (slots, _) = probe();
        let started = call(slots, |slots, completion| {
            let slots = slots as usize;
            let (callback, ctx) = (completion.callback, completion.ctx as usize);
            std::thread::spawn(move || unsafe {
                (*(slots as *mut Probe)).value = 9;
                callback(ctx as *mut c_void, MQB_ERR_RETRYABLE);
            });
            MQB_OK
        });
        let (status, slots) = started.finish().await.unwrap();
        assert_eq!((status, slots.value), (MQB_ERR_RETRYABLE, 9));
    }

    #[tokio::test]
    async fn a_refused_call_returns_its_slots_without_a_callback() {
        let (slots, abandoned) = probe();
        let started = call(slots, |_, _| MQB_ERR_RETRYABLE);
        let (status, _) = started.finish().await.unwrap();
        assert_eq!(status, MQB_ERR_RETRYABLE);
        assert!(!abandoned.load(Ordering::SeqCst));
    }

    /// The waiter is still there when the plugin completes, and is cancelled before it reads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_completed_but_never_read_is_abandoned() {
        let (slots, abandoned) = probe();
        let started = call(slots, |_, completion| unsafe {
            (completion.callback)(completion.ctx, MQB_OK);
            MQB_OK
        });
        drop(started);
        for _ in 0..200 {
            if abandoned.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the unread result was never released");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_result_nobody_awaits_is_abandoned() {
        let (slots, abandoned) = probe();
        let mut later = None;
        let started = call(slots, |_, completion| {
            later = Some(completion);
            MQB_OK
        });
        drop(started);
        let completion = later.unwrap();
        unsafe { (completion.callback)(completion.ctx, MQB_OK) };
        for _ in 0..200 {
            if abandoned.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("the abandoned result was never released");
    }
}
