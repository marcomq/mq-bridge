//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use std::time::Duration;

/// Exponential poll backoff between a base and a max interval. When `max <= base` the delay stays
/// constant at `base` (backoff disabled); otherwise each idle poll doubles the delay up to `max`,
/// and `reset` returns to `base` after a non-empty poll.
pub(crate) struct PollBackoff {
    base: Duration,
    max: Duration,
    current: Duration,
}

impl PollBackoff {
    pub(crate) fn new(base: Duration, max: Option<Duration>) -> Self {
        let max = max.unwrap_or(base).max(base);
        Self {
            base,
            max,
            current: base,
        }
    }

    /// The delay to sleep for this idle poll, then grow toward `max` for the next one.
    pub(crate) fn idle_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(self.max);
        delay
    }

    /// Return to the base interval after a non-empty poll.
    pub(crate) fn reset(&mut self) {
        self.current = self.base;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_backoff_disabled_when_no_max() {
        let base = Duration::from_millis(100);
        let mut b = PollBackoff::new(base, None);
        assert_eq!(b.idle_delay(), base);
        assert_eq!(b.idle_delay(), base); // stays constant without a max
    }

    #[test]
    fn poll_backoff_doubles_up_to_max_and_resets() {
        let base = Duration::from_millis(100);
        let mut b = PollBackoff::new(base, Some(Duration::from_millis(500)));
        assert_eq!(b.idle_delay(), Duration::from_millis(100));
        assert_eq!(b.idle_delay(), Duration::from_millis(200));
        assert_eq!(b.idle_delay(), Duration::from_millis(400));
        assert_eq!(b.idle_delay(), Duration::from_millis(500)); // capped at max
        assert_eq!(b.idle_delay(), Duration::from_millis(500));
        b.reset();
        assert_eq!(b.idle_delay(), Duration::from_millis(100)); // back to base
    }
}
