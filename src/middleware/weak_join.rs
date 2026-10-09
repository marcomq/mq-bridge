use crate::models::{WeakJoinAck, WeakJoinMiddleware, WeakJoinTimeout};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, ReceivedBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use serde_json::Value;
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// A buffered member's place in the source batch it came from.
#[derive(Clone, Copy)]
struct Slot {
    seq: u64,
    idx: usize,
}

struct Group {
    started: Instant,
    messages: Vec<CanonicalMessage>,
    slots: Vec<Slot>,
}

struct JoinState {
    pending: HashMap<String, Group>,
    ready_buffer: VecDeque<(CanonicalMessage, Vec<Slot>)>,
}

struct InnerBatch {
    commit: Option<BatchCommitFunc>,
    dispositions: Vec<MessageDisposition>,
    remaining: usize,
}

/// Holds each source batch's commit until every member has been settled by its joined
/// message. On a source needing ordered commits only the settled prefix is committed.
struct AckTracker {
    ordered: bool,
    base: u64,
    batches: VecDeque<InnerBatch>,
}

type DueCommits = Vec<(BatchCommitFunc, Vec<MessageDisposition>)>;

impl AckTracker {
    fn register(&mut self, commit: BatchCommitFunc, len: usize) -> u64 {
        let seq = self.base + self.batches.len() as u64;
        self.batches.push_back(InnerBatch {
            commit: Some(commit),
            dispositions: vec![MessageDisposition::Ack; len],
            remaining: len,
        });
        seq
    }

    fn settle(&mut self, slot: Slot, disposition: MessageDisposition, due: &mut DueCommits) {
        let Some(batch) = self.batches.get_mut((slot.seq - self.base) as usize) else {
            return;
        };
        batch.dispositions[slot.idx] = disposition;
        batch.remaining -= 1;
        if !self.ordered && batch.remaining == 0 {
            if let Some(commit) = batch.commit.take() {
                due.push((commit, std::mem::take(&mut batch.dispositions)));
            }
        }
    }

    /// Pops the settled prefix one batch at a time, so batches behind a failed commit stay tracked.
    fn pop_due(&mut self) -> Option<(BatchCommitFunc, Vec<MessageDisposition>)> {
        while self.batches.front().is_some_and(|b| b.remaining == 0) {
            let batch = self.batches.pop_front().expect("front checked");
            self.base += 1;
            if let Some(commit) = batch.commit {
                return Some((commit, batch.dispositions));
            }
        }
        None
    }
}

/// Settles members and runs the source commits that became due, oldest first. The tracker
/// stays locked while they run so concurrent settles cannot reorder them.
async fn settle_and_commit(
    tracker: &Mutex<AckTracker>,
    settled: impl IntoIterator<Item = (Slot, MessageDisposition)>,
) -> anyhow::Result<()> {
    let mut tracker = tracker.lock().await;
    let mut due = Vec::new();
    for (slot, disposition) in settled {
        tracker.settle(slot, disposition, &mut due);
    }
    while let Some((commit, dispositions)) = tracker.pop_due() {
        if tracker.ordered {
            commit(dispositions).await?;
        } else {
            due.push((commit, dispositions));
        }
    }
    // Unordered commits are independent, so one failure must not drop the rest.
    let mut first_err = None;
    for (commit, dispositions) in due {
        if let Err(e) = commit(dispositions).await {
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}

type PendingReceive = BoxFuture<'static, Result<ReceivedBatch, ConsumerError>>;

pub struct WeakJoinConsumer {
    inner: Arc<Mutex<Box<dyn MessageConsumer>>>,
    /// The source's in-flight receive, which holds `inner` locked. Kept across calls: a
    /// group timeout must not cancel it, as a source may lose what it had already read.
    pending_receive: std::sync::Mutex<Option<PendingReceive>>,
    commit_requires_order: bool,
    /// False while `exit_on_empty` could not reach `inner` because a receive was in flight.
    exit_on_empty_forwarded: bool,
    config: WeakJoinMiddleware,
    state: Arc<Mutex<JoinState>>,
    /// `None` under `ack: on_receive`, where sources are acked as they arrive.
    tracker: Option<Arc<Mutex<AckTracker>>>,
    /// Drain flag, tracked locally: on drain we must flush buffered pending groups before
    /// exposing an empty batch, so exit only happens once every group is drained.
    exit_on_empty: bool,
}

impl WeakJoinConsumer {
    pub fn new(inner: Box<dyn MessageConsumer>, config: &WeakJoinMiddleware) -> Self {
        if config.branch_by.is_none() && !config.required.is_empty() {
            tracing::warn!(
                "weak_join: 'required' is set but 'branch_by' is not; 'required' only applies in branch mode and will be ignored in count mode."
            );
        }
        let commit_requires_order = inner.commit_requires_order();
        let tracker = (config.ack == WeakJoinAck::OnJoin).then(|| {
            Arc::new(Mutex::new(AckTracker {
                ordered: commit_requires_order,
                base: 0,
                batches: VecDeque::new(),
            }))
        });
        Self {
            inner: Arc::new(Mutex::new(inner)),
            pending_receive: std::sync::Mutex::new(None),
            commit_requires_order,
            exit_on_empty_forwarded: true,
            config: config.clone(),
            state: Arc::new(Mutex::new(JoinState {
                pending: HashMap::new(),
                ready_buffer: VecDeque::new(),
            })),
            tracker,
            exit_on_empty: false,
        }
    }

    /// Forwards the drain flag unless a receive is in flight; `receive_batch` retries then.
    fn forward_exit_on_empty(&mut self) {
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.set_exit_on_empty(self.exit_on_empty);
            self.exit_on_empty_forwarded = true;
        }
    }

    /// Locks the source for a hook. Hooks run between receives, so a receive still pending
    /// is cancelled: it would hold the lock forever.
    async fn inner_for_hook(&self) -> tokio::sync::MutexGuard<'_, Box<dyn MessageConsumer>> {
        self.pending_receive
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        self.inner.lock().await
    }

    /// Dispatches to branch-keyed or count-array joining depending on config.
    fn emit_join(&self, key: &str, messages: &[CanonicalMessage]) -> CanonicalMessage {
        if self.config.branch_by.is_some() {
            self.try_join_branches(key, messages)
        } else {
            self.try_join(key, messages)
        }
    }

    /// Count mode: merge payloads into a JSON array (arrival order).
    fn try_join(&self, key: &str, messages: &[CanonicalMessage]) -> CanonicalMessage {
        let payloads: Vec<Value> = messages.iter().map(payload_to_value).collect();
        let merged_payload = serde_json::to_vec(&payloads).unwrap_or_default();
        self.finalize(key, merged_payload, messages.first())
    }

    /// Branch mode: merge payloads into an object keyed by branch name.
    /// The first message seen for a branch wins; later duplicates are ignored (idempotent).
    fn try_join_branches(&self, key: &str, messages: &[CanonicalMessage]) -> CanonicalMessage {
        let branch_by = self.config.branch_by.as_deref().unwrap_or_default();
        let mut obj = serde_json::Map::new();
        let mut present: Vec<String> = Vec::new();
        for m in messages {
            // Skip messages without a branch label rather than collapsing them
            // under a shared "unknown" key.
            let branch = match m.metadata.get(branch_by) {
                Some(b) => b.clone(),
                None => continue,
            };
            if obj.contains_key(&branch) {
                continue;
            }
            present.push(branch.clone());
            obj.insert(branch, payload_to_value(m));
        }
        let complete = self.is_complete(messages);
        let merged_payload = serde_json::to_vec(&Value::Object(obj)).unwrap_or_default();
        let mut new_msg = self.finalize(key, merged_payload, messages.first());
        new_msg
            .metadata
            .insert("join_branches".to_string(), present.join(","));
        new_msg
            .metadata
            .insert("join_complete".to_string(), complete.to_string());
        new_msg
    }

    /// Builds the joined message: fresh id, metadata inherited from the first source, group key stamped.
    fn finalize(
        &self,
        key: &str,
        payload: Vec<u8>,
        first: Option<&CanonicalMessage>,
    ) -> CanonicalMessage {
        let mut new_msg =
            CanonicalMessage::new(payload, Some(fast_uuid_v7::gen_id_with_sub_ms_4()));
        if let Some(first) = first {
            new_msg.metadata = first.metadata.clone();
        }
        new_msg
            .metadata
            .insert(self.config.group_by.clone(), key.to_string());
        new_msg
    }

    /// Whether a pending group satisfies its fire condition.
    /// Count mode: `expected_count` messages. Branch mode: all `required` branches present,
    /// or (if `required` is empty) `expected_count` distinct branches.
    fn is_complete(&self, messages: &[CanonicalMessage]) -> bool {
        match self.config.branch_by.as_deref() {
            None => messages.len() >= self.config.expected_count,
            Some(branch_by) => {
                let distinct: HashSet<&str> = messages
                    .iter()
                    .filter_map(|m| m.metadata.get(branch_by).map(String::as_str))
                    .collect();
                if self.config.required.is_empty() {
                    distinct.len() >= self.config.expected_count
                } else {
                    self.config
                        .required
                        .iter()
                        .all(|b| distinct.contains(b.as_str()))
                }
            }
        }
    }

    /// Emits a finished group, or under `on_timeout: discard` settles its members as dropped.
    fn close_group(
        &self,
        key: &str,
        group: Group,
        ready: &mut Vec<(CanonicalMessage, Vec<Slot>)>,
        discarded: &mut Vec<Slot>,
    ) {
        if self.config.on_timeout == WeakJoinTimeout::Discard {
            discarded.extend(group.slots);
        } else {
            ready.push((self.emit_join(key, &group.messages), group.slots));
        }
    }

    fn check_timeouts(
        &self,
        state: &mut JoinState,
        ready: &mut Vec<(CanonicalMessage, Vec<Slot>)>,
        discarded: &mut Vec<Slot>,
    ) {
        let now = Instant::now();
        let timeout = Duration::from_millis(self.config.timeout_ms);
        let expired: Vec<String> = state
            .pending
            .iter()
            .filter(|(_, g)| now.duration_since(g.started) >= timeout)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            let group = state.pending.remove(&key).expect("key just listed");
            self.close_group(&key, group, ready, discarded);
        }
    }

    /// Drains every remaining pending group, used when the upstream is exhausted (drain
    /// mode) so no buffered group is stranded when the empty batch propagates up. Honours
    /// `on_timeout`: incomplete groups are emitted unless configured to discard.
    fn flush_all_pending(
        &self,
        state: &mut JoinState,
        ready: &mut Vec<(CanonicalMessage, Vec<Slot>)>,
        discarded: &mut Vec<Slot>,
    ) {
        for (key, group) in std::mem::take(&mut state.pending) {
            self.close_group(&key, group, ready, discarded);
        }
    }

    /// Acks the members of discarded groups at once: they were dropped on purpose.
    async fn ack_discarded(&self, discarded: Vec<Slot>) -> Result<(), ConsumerError> {
        match &self.tracker {
            Some(tracker) if !discarded.is_empty() => settle_and_commit(
                tracker,
                discarded.into_iter().map(|s| (s, MessageDisposition::Ack)),
            )
            .await
            .map_err(ConsumerError::Connection),
            _ => Ok(()),
        }
    }

    /// Joins stay in `ready_buffer` until the discarded members are acked, so a failed
    /// ack loses none of them.
    async fn ack_then_take_ready(
        &self,
        discarded: Vec<Slot>,
        max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        self.ack_discarded(discarded).await?;
        let mut state = self.state.lock().await;
        Ok(self.take_ready(&mut state, max_messages))
    }

    /// Returns up to `max_messages` ready joins; the rest wait in `ready_buffer`. Their
    /// commit settles every member slot with the joined message's disposition.
    fn take_ready(&self, state: &mut JoinState, max_messages: usize) -> ReceivedBatch {
        let count = state.ready_buffer.len().min(max_messages);
        let (messages, slots): (Vec<_>, Vec<_>) = state.ready_buffer.drain(..count).unzip();
        let commit: BatchCommitFunc = match self.tracker.clone() {
            Some(tracker) => Box::new(move |dispositions: Vec<MessageDisposition>| {
                Box::pin(async move {
                    // A joined message with no disposition leaves its members unsettled, so
                    // the source redelivers them rather than acking unprocessed input. A
                    // reply goes to every member, since each one is waiting on the join.
                    let settled = slots
                        .into_iter()
                        .zip(dispositions)
                        .flat_map(|(slots, d)| slots.into_iter().map(move |s| (s, d.clone())));
                    settle_and_commit(&tracker, settled).await
                })
            }),
            None => Box::new(|_| Box::pin(async { Ok(()) })),
        };
        ReceivedBatch { messages, commit }
    }
}

/// Deserializes a message payload as JSON, falling back to a lossy UTF-8 string.
fn payload_to_value(m: &CanonicalMessage) -> Value {
    match serde_json::from_slice(&m.payload) {
        Ok(v) => v,
        Err(_) => Value::String(String::from_utf8_lossy(&m.payload).to_string()),
    }
}

#[async_trait]
impl MessageConsumer for WeakJoinConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        // Track the flag locally so receive_batch can flush pending groups before exposing
        // an empty batch, and still forward it so the inner source actually reports drained.
        self.exit_on_empty = exit_on_empty;
        self.exit_on_empty_forwarded = false;
        self.forward_exit_on_empty();
    }

    fn commit_requires_order(&self) -> bool {
        self.commit_requires_order
    }
    fn on_connect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let inner = self.inner_for_hook().await;
            let hook = inner.on_connect_hook();
            match hook {
                Some(hook) => hook.await,
                None => Ok(()),
            }
        }))
    }

    fn on_disconnect_hook(&self) -> Option<BoxFuture<'_, anyhow::Result<()>>> {
        Some(Box::pin(async move {
            let inner = self.inner_for_hook().await;
            let hook = inner.on_disconnect_hook();
            match hook {
                Some(hook) => hook.await,
                None => Ok(()),
            }
        }))
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let mut state = self.state.lock().await;

        if !state.ready_buffer.is_empty() {
            return Ok(self.take_ready(&mut state, max_messages));
        }

        let now = Instant::now();
        let timeout_duration = Duration::from_millis(self.config.timeout_ms);
        let next_timeout = state
            .pending
            .values()
            .map(|g| g.started + timeout_duration)
            .min()
            .unwrap_or(now + Duration::from_secs(3600));

        let sleep_duration = next_timeout.saturating_duration_since(now);
        drop(state);

        if !self.exit_on_empty_forwarded {
            self.forward_exit_on_empty();
        }
        let pending = self
            .pending_receive
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let received = {
            let receive = pending.get_or_insert_with(|| {
                let inner = Arc::clone(&self.inner);
                Box::pin(async move { inner.lock_owned().await.receive_batch(max_messages).await })
            });
            tokio::select! {
                res = receive => Some(res),
                _ = tokio::time::sleep(sleep_duration) => None,
            }
        };

        let mut ready = Vec::new();
        let mut discarded = Vec::new();
        match received {
            Some(res) => {
                *pending = None;
                let batch = res?;
                let count = batch.messages.len();
                let seq = match &self.tracker {
                    Some(tracker) if count > 0 => {
                        Some(tracker.lock().await.register(batch.commit, count))
                    }
                    Some(_) => None,
                    None => {
                        if count > 0 {
                            (batch.commit)(vec![MessageDisposition::Ack; count])
                                .await
                                .map_err(ConsumerError::Connection)?;
                        }
                        None
                    }
                };

                let mut state = self.state.lock().await;
                // Flush expired groups before admitting new messages, so a
                // fresh message for an expired key starts a new group rather
                // than joining a stale one.
                self.check_timeouts(&mut state, &mut ready, &mut discarded);

                // An empty upstream batch under exit_on_empty means the source is
                // drained: flush every remaining pending group so none is stranded,
                // and only then let an empty batch propagate up to end the route.
                if count == 0 && self.exit_on_empty {
                    self.flush_all_pending(&mut state, &mut ready, &mut discarded);
                }

                let now = Instant::now();
                for (idx, msg) in batch.messages.into_iter().enumerate() {
                    let key = msg
                        .metadata
                        .get(&self.config.group_by)
                        .cloned()
                        .unwrap_or_else(|| "default".to_string());
                    let group = state.pending.entry(key.clone()).or_insert_with(|| Group {
                        started: now,
                        messages: Vec::new(),
                        slots: Vec::new(),
                    });
                    group.messages.push(msg);
                    if let Some(seq) = seq {
                        group.slots.push(Slot { seq, idx });
                    }

                    if self.is_complete(&group.messages) {
                        let group = state.pending.remove(&key).expect("entry just used");
                        ready.push((self.emit_join(&key, &group.messages), group.slots));
                    }
                }

                state.ready_buffer.extend(ready);
                drop(state);
                self.ack_then_take_ready(discarded, max_messages).await
            }
            None => {
                let mut state = self.state.lock().await;
                self.check_timeouts(&mut state, &mut ready, &mut discarded);
                state.ready_buffer.extend(ready);
                drop(state);
                self.ack_then_take_ready(discarded, max_messages).await
            }
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::endpoints::memory::MemoryConsumer;
    use crate::CanonicalMessage;
    use serde_json::json;

    #[tokio::test]
    async fn test_weak_join_grouping() {
        let config = WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 2,
            timeout_ms: 1000,
            branch_by: None,
            required: Vec::new(),
            on_timeout: WeakJoinTimeout::Fire,
            ack: WeakJoinAck::OnJoin,
        };

        let mem_consumer = MemoryConsumer::new_local("join_test", 10);
        let channel = mem_consumer.channel();

        // Send 2 messages with group_id "A"
        let msg1 = CanonicalMessage::from_json(json!({"val": 1}))
            .unwrap()
            .with_metadata_kv("group_id", "A");
        let msg2 = CanonicalMessage::from_json(json!({"val": 2}))
            .unwrap()
            .with_metadata_kv("group_id", "A");

        channel.send_message(msg1).await.unwrap();
        channel.send_message(msg2).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        let batch = join_consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);

        let joined = &batch.messages[0];
        let payload: Vec<Value> = serde_json::from_slice(&joined.payload).unwrap();
        assert_eq!(payload.len(), 2);
        assert_eq!(payload[0]["val"], 1);
        assert_eq!(payload[1]["val"], 2);
        assert_eq!(
            joined.metadata.get("group_id").map(|s| s.as_str()),
            Some("A")
        );
    }

    #[tokio::test]
    async fn test_weak_join_timeout() {
        let config = WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 3,
            timeout_ms: 100,
            branch_by: None,
            required: Vec::new(),
            on_timeout: WeakJoinTimeout::Fire,
            ack: WeakJoinAck::OnJoin,
        };

        let mem_consumer = MemoryConsumer::new_local("join_timeout_test", 10);
        let channel = mem_consumer.channel();

        // Send 1 message with group_id "B" (less than expected 3)
        let msg1 = CanonicalMessage::from_json(json!({"val": 1}))
            .unwrap()
            .with_metadata_kv("group_id", "B");

        channel.send_message(msg1).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        // First call will pick up the message but return empty batch because count < expected
        let batch1 = join_consumer.receive_batch(10).await.unwrap();
        assert!(batch1.messages.is_empty());

        // Wait for timeout to expire
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Second call should trigger timeout logic and return the partial batch
        let batch2 = join_consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch2.messages.len(), 1);

        let joined = &batch2.messages[0];
        let payload: Vec<Value> = serde_json::from_slice(&joined.payload).unwrap();
        assert_eq!(payload.len(), 1);
        assert_eq!(payload[0]["val"], 1);
    }

    #[tokio::test]
    async fn test_weak_join_branches_all_required() {
        let config = WeakJoinMiddleware {
            group_by: "correlation_id".to_string(),
            expected_count: 0,
            timeout_ms: 1000,
            branch_by: Some("branch".to_string()),
            required: vec!["postgres".to_string(), "features".to_string()],
            on_timeout: WeakJoinTimeout::Fire,
            ack: WeakJoinAck::OnJoin,
        };

        let mem_consumer = MemoryConsumer::new_local("join_branch_test", 10);
        let channel = mem_consumer.channel();

        let pg = CanonicalMessage::from_json(json!({"id": 42}))
            .unwrap()
            .with_metadata_kv("correlation_id", "order-1")
            .with_metadata_kv("branch", "postgres");
        let feat = CanonicalMessage::from_json(json!({"score": 0.9}))
            .unwrap()
            .with_metadata_kv("correlation_id", "order-1")
            .with_metadata_kv("branch", "features");

        channel.send_message(pg).await.unwrap();
        channel.send_message(feat).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        let batch = join_consumer.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);

        let joined = &batch.messages[0];
        let payload: Value = serde_json::from_slice(&joined.payload).unwrap();
        // Branch-keyed object, not a positional array.
        assert_eq!(payload["postgres"]["id"], 42);
        assert_eq!(payload["features"]["score"], 0.9);
        assert_eq!(
            joined.metadata.get("join_complete").map(|s| s.as_str()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn test_weak_join_branch_incomplete_does_not_fire() {
        let config = WeakJoinMiddleware {
            group_by: "correlation_id".to_string(),
            expected_count: 0,
            timeout_ms: 1000,
            branch_by: Some("branch".to_string()),
            required: vec!["postgres".to_string(), "features".to_string()],
            on_timeout: WeakJoinTimeout::Fire,
            ack: WeakJoinAck::OnJoin,
        };

        let mem_consumer = MemoryConsumer::new_local("join_branch_incomplete", 10);
        let channel = mem_consumer.channel();

        // Two messages from the SAME branch must NOT satisfy a two-branch join.
        let pg1 = CanonicalMessage::from_json(json!({"id": 1}))
            .unwrap()
            .with_metadata_kv("correlation_id", "order-2")
            .with_metadata_kv("branch", "postgres");
        let pg2 = CanonicalMessage::from_json(json!({"id": 2}))
            .unwrap()
            .with_metadata_kv("correlation_id", "order-2")
            .with_metadata_kv("branch", "postgres");

        channel.send_message(pg1).await.unwrap();
        channel.send_message(pg2).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        let batch = join_consumer.receive_batch(10).await.unwrap();
        assert!(batch.messages.is_empty());
    }

    #[tokio::test]
    async fn test_weak_join_branch_timeout_discard() {
        let config = WeakJoinMiddleware {
            group_by: "correlation_id".to_string(),
            expected_count: 0,
            timeout_ms: 100,
            branch_by: Some("branch".to_string()),
            required: vec!["postgres".to_string(), "features".to_string()],
            on_timeout: WeakJoinTimeout::Discard,
            ack: WeakJoinAck::OnJoin,
        };

        let mem_consumer = MemoryConsumer::new_local("join_branch_discard", 10);
        let channel = mem_consumer.channel();

        let pg = CanonicalMessage::from_json(json!({"id": 7}))
            .unwrap()
            .with_metadata_kv("correlation_id", "order-3")
            .with_metadata_kv("branch", "postgres");
        channel.send_message(pg).await.unwrap();

        let mut join_consumer = WeakJoinConsumer::new(Box::new(mem_consumer), &config);

        let batch1 = join_consumer.receive_batch(10).await.unwrap();
        assert!(batch1.messages.is_empty());

        tokio::time::sleep(Duration::from_millis(150)).await;

        // Incomplete group is dropped on timeout, not emitted as a partial.
        let batch2 = join_consumer.receive_batch(10).await.unwrap();
        assert!(batch2.messages.is_empty());
    }

    type CommitLog = Arc<std::sync::Mutex<Vec<(usize, Vec<&'static str>)>>>;

    /// Hands out scripted batches and records each batch's commit as (batch index, dispositions).
    struct ScriptedSource {
        batches: VecDeque<Vec<CanonicalMessage>>,
        next: usize,
        ordered: bool,
        log: CommitLog,
        fail: Option<usize>,
    }

    impl ScriptedSource {
        fn new(batches: Vec<Vec<CanonicalMessage>>, ordered: bool) -> (Self, CommitLog) {
            let log = CommitLog::default();
            let source = Self {
                batches: batches.into(),
                next: 0,
                ordered,
                log: log.clone(),
                fail: None,
            };
            (source, log)
        }
    }

    #[async_trait]
    impl MessageConsumer for ScriptedSource {
        fn commit_requires_order(&self) -> bool {
            self.ordered
        }

        async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let messages = self.batches.pop_front().unwrap_or_default();
            let id = self.next;
            self.next += 1;
            let log = self.log.clone();
            let fail = self.fail == Some(id);
            Ok(ReceivedBatch {
                messages,
                commit: Box::new(move |dispositions| {
                    Box::pin(async move {
                        if fail {
                            anyhow::bail!("commit {id} failed");
                        }
                        let named = dispositions
                            .iter()
                            .map(|d| match d {
                                MessageDisposition::Nack => "nack",
                                MessageDisposition::Reply(_) => "reply",
                                MessageDisposition::Ack => "ack",
                            })
                            .collect();
                        log.lock().unwrap().push((id, named));
                        Ok(())
                    })
                }),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn member(group: &str) -> CanonicalMessage {
        CanonicalMessage::from_json(json!({"g": group}))
            .unwrap()
            .with_metadata_kv("group_id", group)
    }

    fn pair_config(
        ack: WeakJoinAck,
        on_timeout: WeakJoinTimeout,
        timeout_ms: u64,
    ) -> WeakJoinMiddleware {
        WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 2,
            timeout_ms,
            branch_by: None,
            required: Vec::new(),
            on_timeout,
            ack,
        }
    }

    fn logged(log: &CommitLog) -> Vec<(usize, Vec<&'static str>)> {
        log.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn members_are_acked_only_once_the_join_commits() {
        let (source, log) = ScriptedSource::new(vec![vec![member("A"), member("A")]], false);
        let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Fire, 1000);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        let batch = join.receive_batch(10).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert!(
            logged(&log).is_empty(),
            "must not ack before the join is committed"
        );

        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        assert_eq!(logged(&log), vec![(0, vec!["ack", "ack"])]);
    }

    #[tokio::test]
    async fn a_nacked_join_nacks_its_members() {
        let (source, log) = ScriptedSource::new(vec![vec![member("A"), member("A")]], false);
        let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Fire, 1000);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        let batch = join.receive_batch(10).await.unwrap();
        (batch.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();
        assert_eq!(logged(&log), vec![(0, vec!["nack", "nack"])]);
    }

    #[tokio::test]
    async fn a_joined_reply_reaches_every_member() {
        let (source, log) = ScriptedSource::new(vec![vec![member("A"), member("A")]], false);
        let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Fire, 1000);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        let batch = join.receive_batch(10).await.unwrap();
        (batch.commit)(vec![MessageDisposition::Reply(CanonicalMessage::from(
            "ok",
        ))])
        .await
        .unwrap();
        assert_eq!(logged(&log), vec![(0, vec!["reply", "reply"])]);
    }

    /// Group B completes before the older group A. An ordered source must still commit its
    /// batches in the order it produced them; an unordered one commits B's batch at once.
    #[tokio::test]
    async fn source_batches_commit_in_order_only_when_the_source_needs_it() {
        for ordered in [true, false] {
            let (source, log) = ScriptedSource::new(
                vec![
                    vec![member("A")],
                    vec![member("B"), member("B")],
                    vec![member("A")],
                ],
                ordered,
            );
            let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Fire, 10_000);
            let mut join = WeakJoinConsumer::new(Box::new(source), &config);

            assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
            let b = join.receive_batch(10).await.unwrap();
            (b.commit)(vec![MessageDisposition::Ack]).await.unwrap();
            if ordered {
                assert!(logged(&log).is_empty(), "batch 1 must wait for batch 0");
            } else {
                assert_eq!(logged(&log), vec![(1, vec!["ack", "ack"])]);
            }

            let a = join.receive_batch(10).await.unwrap();
            (a.commit)(vec![MessageDisposition::Ack]).await.unwrap();
            let order: Vec<usize> = logged(&log).into_iter().map(|(id, _)| id).collect();
            if ordered {
                assert_eq!(order, vec![0, 1, 2]);
            } else {
                assert_eq!(order, vec![1, 0, 2]);
            }
        }
    }

    /// Three source batches become due at once and the oldest commit fails: the later two
    /// must stay tracked and commit, in order, on the next settle.
    #[tokio::test]
    async fn a_failed_ordered_commit_keeps_later_batches() {
        let (mut source, log) = ScriptedSource::new(
            vec![
                vec![member("A")],
                vec![member("B")],
                vec![member("A"), member("B")],
                vec![member("C"), member("C")],
            ],
            true,
        );
        source.fail = Some(0);
        let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Fire, 10_000);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
        let ab = join.receive_batch(10).await.unwrap();
        assert_eq!(ab.messages.len(), 2);
        assert!((ab.commit)(vec![MessageDisposition::Ack; 2]).await.is_err());
        assert!(logged(&log).is_empty());

        let c = join.receive_batch(10).await.unwrap();
        (c.commit)(vec![MessageDisposition::Ack]).await.unwrap();
        let order: Vec<usize> = logged(&log).into_iter().map(|(id, _)| id).collect();
        assert_eq!(order, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn a_discarded_group_is_acked_on_timeout() {
        let (source, log) = ScriptedSource::new(vec![vec![member("A")]], true);
        let config = pair_config(WeakJoinAck::OnJoin, WeakJoinTimeout::Discard, 50);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
        assert!(logged(&log).is_empty());
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
        assert_eq!(logged(&log), vec![(0, vec!["ack"])]);
    }

    #[tokio::test]
    async fn on_receive_acks_as_messages_arrive() {
        let (source, log) = ScriptedSource::new(vec![vec![member("A")]], true);
        let config = pair_config(WeakJoinAck::OnReceive, WeakJoinTimeout::Fire, 1000);
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);

        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());
        assert_eq!(logged(&log), vec![(0, vec!["ack"])]);
    }

    /// Takes a message, then waits before returning it: a cancelled receive loses it.
    struct SlowSource {
        queue: VecDeque<(CanonicalMessage, Duration)>,
    }

    #[async_trait]
    impl MessageConsumer for SlowSource {
        async fn receive_batch(&mut self, _max: usize) -> Result<ReceivedBatch, ConsumerError> {
            let Some((message, delay)) = self.queue.pop_front() else {
                return std::future::pending().await;
            };
            tokio::time::sleep(delay).await;
            Ok(ReceivedBatch {
                messages: vec![message],
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[tokio::test]
    async fn a_group_timeout_does_not_cancel_the_receive_in_flight() {
        let config = WeakJoinMiddleware {
            group_by: "group_id".to_string(),
            expected_count: 2,
            timeout_ms: 30,
            branch_by: None,
            required: Vec::new(),
            on_timeout: WeakJoinTimeout::Fire,
            ack: WeakJoinAck::OnJoin,
        };
        let message = |val: u64| {
            CanonicalMessage::from_json(json!({ "val": val }))
                .unwrap()
                .with_metadata_kv("group_id", "A")
        };
        // The first receive returns at once and opens a group; the second outlasts its timeout.
        let source = SlowSource {
            queue: VecDeque::from([
                (message(1), Duration::ZERO),
                (message(2), Duration::from_millis(120)),
            ]),
        };
        let mut join = WeakJoinConsumer::new(Box::new(source), &config);
        assert!(join.receive_batch(10).await.unwrap().messages.is_empty());

        let mut values = Vec::new();
        while values.len() < 2 {
            let batch = tokio::time::timeout(Duration::from_secs(5), join.receive_batch(10))
                .await
                .expect("the second message was lost with the cancelled receive")
                .unwrap();
            for joined in batch.messages {
                let payload: Vec<Value> = serde_json::from_slice(&joined.payload).unwrap();
                values.extend(payload.iter().map(|p| p["val"].as_u64().unwrap()));
            }
        }
        assert_eq!(values, vec![1, 2]);
    }
}
