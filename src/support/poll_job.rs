//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Waiting for a job a remote system runs asynchronously: an indexing task, a
//! bulk import, a query that answers with a job id.

use std::future::Future;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};

/// How often a job is asked for its state, when a slow one is reported, and
/// when it is given up.
#[derive(Debug, Clone, Copy)]
pub struct PollSchedule {
    /// Delay before the second poll; it doubles after every poll.
    pub first: Duration,
    /// Upper bound of the delay.
    pub max: Duration,
    /// How long the job may run between two calls of the `slow` callback.
    pub notice_every: Duration,
    /// How long the job may run before `poll_until` gives up.
    pub timeout: Duration,
}

impl Default for PollSchedule {
    fn default() -> Self {
        Self {
            first: Duration::from_millis(10),
            max: Duration::from_millis(250),
            notice_every: Duration::from_secs(60),
            timeout: Duration::from_secs(300),
        }
    }
}

/// `None` once `deadline` has passed. A timeout too large to add has no deadline.
async fn before<F: Future>(deadline: Option<Instant>, future: F) -> Option<F::Output> {
    match deadline {
        Some(deadline) => timeout_at(deadline, future).await.ok(),
        None => Some(future.await),
    }
}

/// Calls `poll` until it answers `Some`, sleeping between calls as `schedule`
/// says, and answers `None` once `schedule.timeout` has passed; a poll still
/// running then is dropped. `slow` is called with the time waited each
/// `notice_every`, so the caller can log it. Return `Some(Err(..))` from `poll`
/// to give up, and `None` for a failed poll that is worth repeating.
pub async fn poll_until<T, Fut>(
    schedule: PollSchedule,
    mut slow: impl FnMut(Duration),
    mut poll: impl FnMut() -> Fut,
) -> Option<T>
where
    Fut: Future<Output = Option<T>>,
{
    let started = Instant::now();
    let deadline = started.checked_add(schedule.timeout);
    let mut notice_at = schedule.notice_every;
    let mut delay = schedule.first;
    loop {
        if let Some(done) = before(deadline, poll()).await? {
            return Some(done);
        }
        let waited = started.elapsed();
        if waited >= notice_at {
            slow(waited);
            notice_at += schedule.notice_every;
        }
        before(deadline, tokio::time::sleep(delay)).await?;
        delay = (delay * 2).min(schedule.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_job_is_reported_and_awaited_within_the_deadline() {
        let schedule = PollSchedule {
            first: Duration::from_millis(1),
            max: Duration::from_millis(5),
            notice_every: Duration::from_millis(10),
            timeout: Duration::from_secs(30),
        };
        let mut polls = 0;
        let mut notices = 0;
        let done = poll_until(
            schedule,
            |_| notices += 1,
            || {
                polls += 1;
                let finished = polls >= 12;
                async move { finished.then_some("succeeded") }
            },
        )
        .await;
        assert_eq!(done, Some("succeeded"));
        assert_eq!(polls, 12);
        assert!(notices >= 1, "a job slower than `notice_every` is reported");
    }

    #[tokio::test]
    async fn a_job_that_never_ends_is_given_up_at_the_deadline() {
        let schedule = PollSchedule {
            first: Duration::from_millis(1),
            max: Duration::from_millis(5),
            notice_every: Duration::from_secs(60),
            timeout: Duration::from_millis(40),
        };
        let mut polls = 0;
        let started = Instant::now();
        let done: Option<()> = poll_until(
            schedule,
            |_| {},
            || {
                polls += 1;
                async { None }
            },
        )
        .await;
        assert_eq!(done, None);
        assert!(polls >= 2, "polled {polls} times");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_poll_that_hangs_is_cut_off_at_the_deadline() {
        let schedule = PollSchedule {
            timeout: Duration::from_millis(20),
            ..PollSchedule::default()
        };
        let done: Option<()> =
            poll_until(schedule, |_| {}, std::future::pending::<Option<()>>).await;
        assert_eq!(done, None);
    }
}
