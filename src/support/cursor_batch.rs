//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The read position of a cursor-column source and the batches handed out from it.

use crate::traits::MessageDisposition;
use std::sync::{Arc, Mutex};

/// Where the next page starts. A nack rewinds `last` and starts a new `epoch`; batches read
/// in an older one are read again, so their commits confirm nothing.
pub(crate) struct CursorPosition<C> {
    pub last: Option<C>,
    pub epoch: u64,
}

/// A batch handed out and not settled yet. Dropped without a commit, its rows are read again.
pub(crate) struct Unsettled<C> {
    pub position: Arc<Mutex<CursorPosition<C>>>,
    /// The cursor before this batch.
    pub resume_from: Option<C>,
    pub epoch: u64,
    pub settled: bool,
}

impl<C: Clone> Unsettled<C> {
    /// Settles the batch whose rows end at `cursors` and returns the cursor up to which the
    /// leading acked rows are confirmed. Rows after it are read again on the next poll.
    /// `None` when nothing is confirmed, or the batch was read before an earlier nack.
    pub fn settle(&mut self, cursors: &[C], dispositions: &[MessageDisposition]) -> Option<C> {
        self.settled = true;
        let acked = dispositions
            .iter()
            .take(cursors.len())
            .take_while(|d| matches!(d, MessageDisposition::Ack | MessageDisposition::Reply(_)))
            .count();
        let boundary = match acked {
            0 => self.resume_from.take(),
            n => Some(cursors[n - 1].clone()),
        };
        let mut position = self.position.lock().unwrap();
        if position.epoch != self.epoch {
            return None;
        }
        if acked < cursors.len() {
            position.last = boundary.clone();
            position.epoch += 1;
        }
        boundary
    }
}

impl<C> Drop for Unsettled<C> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut position = self.position.lock().unwrap();
        if position.epoch == self.epoch {
            position.last = self.resume_from.take();
            position.epoch += 1;
        }
    }
}
