//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details

use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::traits::{
    BatchCommitFunc, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    ReceivedBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{debug, info, trace, warn};

#[derive(Debug, Clone)]
pub struct RetentionPolicy {
    /// Maximum age of an event before it is eligible for removal.
    pub max_age: Option<Duration>,
    /// Maximum number of events to keep in the store.
    pub max_count: Option<usize>,
    /// Time after which a subscriber is considered inactive and ignored for GC purposes.
    pub subscriber_timeout: Duration,
    /// Minimum interval between Garbage Collection runs.
    pub gc_interval: Duration,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            max_age: Some(Duration::from_secs(3600 * 24)), // 24 hours
            max_count: Some(100_000),
            subscriber_timeout: Duration::from_secs(300), // 5 minutes
            gc_interval: Duration::from_secs(1),
        }
    }
}

/// A map to hold event stores for the duration of the bridge setup.
static RUNTIME_EVENT_STORES: Lazy<Mutex<HashMap<String, Arc<EventStore>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// Gets a shared `EventStore` for a given topic, creating it if it doesn't exist.
pub fn get_or_create_event_store(topic: &str) -> Arc<EventStore> {
    let mut stores = RUNTIME_EVENT_STORES.lock().unwrap();
    stores
        .entry(topic.to_string())
        .or_insert_with(|| {
            info!(topic = %topic, "Creating new runtime event store");
            Arc::new(EventStore::new(Default::default()))
        })
        .clone()
}

/// Checks if an EventStore exists for the given topic.
pub fn event_store_exists(topic: &str) -> bool {
    let stores = RUNTIME_EVENT_STORES.lock().unwrap();
    stores.contains_key(topic)
}

#[derive(Debug)]
struct SubscriberState {
    last_ack_offset: AtomicU64,
    last_seen: Mutex<SystemTime>,
}

#[derive(Debug, Clone)]
pub struct StoredEvent {
    pub message: CanonicalMessage,
    pub offset: u64,
    pub stored_at: u64,
}

/// A robust, in-memory event store.
/// Supports append-only storage, subscriber tracking, and garbage collection.
pub struct EventStore {
    events: RwLock<VecDeque<StoredEvent>>,
    subscribers: RwLock<HashMap<String, Arc<SubscriberState>>>,
    next_offset: AtomicU64,
    base_offset: AtomicU64,
    retention_policy: RetentionPolicy,
    offset_update: watch::Sender<u64>,
    dropped_events: AtomicU64,
    last_gc: RwLock<SystemTime>,
    on_drop: Option<Box<dyn Fn(Vec<StoredEvent>) + Send + Sync>>,
}

impl std::fmt::Debug for EventStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventStore")
            .field("next_offset", &self.next_offset)
            .field("base_offset", &self.base_offset)
            .field("retention_policy", &self.retention_policy)
            .finish_non_exhaustive()
    }
}

impl EventStore {
    pub fn new(retention_policy: RetentionPolicy) -> Self {
        let (offset_update, _) = watch::channel(0);
        Self {
            events: RwLock::new(VecDeque::new()),
            subscribers: RwLock::new(HashMap::new()),
            next_offset: AtomicU64::new(1), // Start offsets at 1
            base_offset: AtomicU64::new(1),
            retention_policy,
            offset_update,
            dropped_events: AtomicU64::new(0),
            last_gc: RwLock::new(SystemTime::now()),
            on_drop: None,
        }
    }

    /// Appends an event to the store.
    /// Assigns an offset and stored_at timestamp to the message metadata.
    pub async fn append(&self, event: CanonicalMessage) -> u64 {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let mut events = self.events.write().unwrap();
        let offset = self.next_offset.fetch_add(1, Ordering::SeqCst);
        let stored = StoredEvent {
            message: event,
            offset,
            stored_at: timestamp,
        };

        events.push_back(stored);

        let mut removed = Vec::new();
        let mut remove_count = 0;
        let mut total_dropped = 0;
        let mut max_count = 0;

        // Enforce max_count immediately
        if let Some(max) = self.retention_policy.max_count {
            max_count = max;
            if events.len() > max {
                remove_count = events.len() - max;
                removed = events.drain(0..remove_count).collect();
                self.base_offset
                    .fetch_add(remove_count as u64, Ordering::SeqCst);
                total_dropped = self
                    .dropped_events
                    .fetch_add(remove_count as u64, Ordering::SeqCst)
                    + remove_count as u64;
            }
        }
        drop(events);

        if remove_count > 0 {
            if let Some(cb) = &self.on_drop {
                cb(removed);
            }
            warn!(
                "Retention policy enforced (max_count={}): dropped {} events (total dropped: {}). Slow subscribers may miss events.",
                max_count, remove_count, total_dropped
            );
        }

        trace!("Appended event offset {}", offset);
        // Notify waiting subscribers
        let _ = self.offset_update.send(offset);

        offset
    }

    /// Appends a batch of events to the store.
    pub async fn append_batch(&self, messages: Vec<CanonicalMessage>) -> u64 {
        if messages.is_empty() {
            return self.next_offset.load(Ordering::SeqCst).saturating_sub(1);
        }

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let count = messages.len() as u64;

        let mut events = self.events.write().unwrap();
        let start_offset = self.next_offset.fetch_add(count, Ordering::SeqCst);

        let stored_events = messages
            .into_iter()
            .enumerate()
            .map(|(i, event)| StoredEvent {
                message: event,
                offset: start_offset + i as u64,
                stored_at: timestamp,
            });

        events.extend(stored_events);

        let last_offset = start_offset + count - 1;

        let mut removed = Vec::new();
        let mut remove_count = 0;
        let mut total_dropped = 0;
        let mut max_count = 0;

        // Enforce max_count immediately
        if let Some(max) = self.retention_policy.max_count {
            max_count = max;
            if events.len() > max {
                remove_count = events.len() - max;
                removed = events.drain(0..remove_count).collect();
                self.base_offset
                    .fetch_add(remove_count as u64, Ordering::SeqCst);
                total_dropped = self
                    .dropped_events
                    .fetch_add(remove_count as u64, Ordering::SeqCst)
                    + remove_count as u64;
            }
        }
        drop(events);

        if remove_count > 0 {
            if let Some(cb) = &self.on_drop {
                cb(removed);
            }
            warn!(
                "Retention policy enforced (max_count={}): dropped {} events (total dropped: {}). Slow subscribers may miss events.",
                max_count, remove_count, total_dropped
            );
        }

        trace!("Appended batch up to offset {}", last_offset);
        // Notify waiting subscribers
        let _ = self.offset_update.send(last_offset);

        last_offset
    }

    pub fn with_drop_callback(
        mut self,
        callback: impl Fn(Vec<StoredEvent>) + Send + Sync + 'static,
    ) -> Self {
        self.on_drop = Some(Box::new(callback));
        self
    }

    /// Registers a subscriber or updates its liveness.
    pub async fn register_subscriber(&self, subscriber_id: String) {
        // Fast path: try to update existing subscriber with a read lock
        {
            let subs = self.subscribers.read().unwrap();
            if let Some(state) = subs.get(&subscriber_id) {
                if let Ok(mut last_seen) = state.last_seen.lock() {
                    *last_seen = SystemTime::now();
                }
                return;
            }
        }

        // Slow path: insert new subscriber with a write lock
        let mut subs = self.subscribers.write().unwrap();
        subs.entry(subscriber_id).or_insert_with(|| {
            Arc::new(SubscriberState {
                last_ack_offset: AtomicU64::new(0),
                last_seen: Mutex::new(SystemTime::now()),
            })
        });
    }

    /// Acknowledges processing of events up to `offset` for a subscriber.
    /// Triggers Garbage Collection.
    pub async fn ack(&self, subscriber_id: &str, offset: u64) {
        {
            let subs = self.subscribers.read().unwrap();
            if let Some(s) = subs.get(subscriber_id) {
                s.last_ack_offset.fetch_max(offset, Ordering::SeqCst);
                if let Ok(mut last_seen) = s.last_seen.lock() {
                    *last_seen = SystemTime::now();
                }
            }
        }

        trace!("Subscriber {} acked offset {}", subscriber_id, offset);

        let now = SystemTime::now();
        let should_gc = {
            let last = self.last_gc.read().unwrap();
            now.duration_since(*last).unwrap_or(Duration::ZERO) >= self.retention_policy.gc_interval
        };

        if should_gc {
            let perform_gc = {
                let mut last = self.last_gc.write().unwrap();
                if now.duration_since(*last).unwrap_or(Duration::ZERO)
                    >= self.retention_policy.gc_interval
                {
                    *last = now;
                    true
                } else {
                    false
                }
            };

            if perform_gc {
                self.run_gc().await;
            }
        }
    }

    /// Returns events with offset > last_known_offset.
    ///
    /// If the requested events have already been GC'd (i.e. there's a gap between
    /// `last_known_offset + 1` and the current `base_offset`), returns `Err(ConsumerError::Gap)`.
    pub async fn get_events_since(
        &self,
        last_known_offset: u64,
        limit: usize,
    ) -> Result<Vec<StoredEvent>, crate::errors::ConsumerError> {
        let events = self.events.read().unwrap();
        let base = self.base_offset.load(Ordering::SeqCst);

        // We want events starting from offset: last_known_offset + 1
        let start_offset = last_known_offset + 1;

        // If the requested start_offset is older than base, the caller has a gap.
        if start_offset < base {
            return Err(crate::errors::ConsumerError::Gap {
                requested: start_offset,
                base,
            });
        }

        // Calculate index relative to the vector
        let start_index = if start_offset > base {
            (start_offset - base) as usize
        } else {
            0
        };

        if start_index >= events.len() {
            return Ok(Vec::new());
        }

        let count = std::cmp::min(limit, events.len() - start_index);
        let mut result = Vec::with_capacity(count);

        let (s1, s2) = events.as_slices();

        if start_index < s1.len() {
            let take_s1 = std::cmp::min(count, s1.len() - start_index);
            result.extend_from_slice(&s1[start_index..start_index + take_s1]);

            let remaining = count - take_s1;
            if remaining > 0 {
                result.extend_from_slice(&s2[0..remaining]);
            }
        } else {
            let s2_index = start_index - s1.len();
            result.extend_from_slice(&s2[s2_index..s2_index + count]);
        }

        Ok(result)
    }

    /// Waits for new events if none are currently available after last_known_offset.
    pub async fn wait_for_events(&self, last_known_offset: u64) {
        let mut rx = self.offset_update.subscribe();
        if *rx.borrow() > last_known_offset {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() > last_known_offset {
                return;
            }
        }
    }

    async fn run_gc(&self) {
        let subs = self.subscribers.read().unwrap();
        if subs.is_empty() {
            return;
        }

        let now = SystemTime::now();
        let timeout = self.retention_policy.subscriber_timeout;

        // Calculate min_ack only from active subscribers
        let active_acks: Vec<u64> = subs
            .values()
            .filter(|s| {
                let last_seen = s.last_seen.lock().unwrap();
                if let Ok(age) = now.duration_since(*last_seen) {
                    age < timeout
                } else {
                    false
                }
            })
            .map(|s| s.last_ack_offset.load(Ordering::SeqCst))
            .collect();

        drop(subs);

        // If no active subscribers, we default to 0 (don't remove based on ack)
        let min_ack = if active_acks.is_empty() {
            0
        } else {
            *active_acks.iter().min().unwrap()
        };

        let mut events = self.events.write().unwrap();
        let base = self.base_offset.load(Ordering::SeqCst);
        let max_age = self.retention_policy.max_age;
        let mut remove_count = 0;

        for (i, event) in events.iter().enumerate() {
            let mut remove = false;
            let offset = base + i as u64;

            // Condition 1: All active subscribers have acked this event
            if !active_acks.is_empty() && offset <= min_ack {
                remove = true;
            }

            // Condition 2: Max age exceeded (Secondary GC)
            if !remove {
                if let Some(max_age) = max_age {
                    let event_time = UNIX_EPOCH + Duration::from_millis(event.stored_at);
                    if let Ok(age) = now.duration_since(event_time) {
                        if age > max_age {
                            remove = true;
                        }
                    }
                }
            }

            if remove {
                remove_count += 1;
            } else {
                break;
            }
        }

        let mut removed = Vec::new();
        if remove_count > 0 {
            debug!("GC removing {} events", remove_count);
            removed = events.drain(0..remove_count).collect();
            self.base_offset
                .fetch_add(remove_count as u64, Ordering::SeqCst);
        }
        drop(events);

        if remove_count > 0 {
            if let Some(cb) = &self.on_drop {
                cb(removed);
            }
        }
    }

    pub fn consumer(self: &Arc<Self>, subscriber_id: String) -> EventStoreConsumer {
        EventStoreConsumer {
            store: self.clone(),
            subscriber_id,
            last_offset: Arc::new(AtomicU64::new(0)),
        }
    }
}

#[derive(Debug)]
pub struct EventStoreConsumer {
    store: Arc<EventStore>,
    subscriber_id: String,
    last_offset: Arc<AtomicU64>,
}

#[async_trait]
impl MessageConsumer for EventStoreConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.store
            .register_subscriber(self.subscriber_id.clone())
            .await;

        let last_offset_val = self.last_offset.load(Ordering::SeqCst);

        let stored_events = match self
            .store
            .get_events_since(last_offset_val, max_messages)
            .await
        {
            Ok(ev) => ev,
            Err(e) => return Err(e),
        };

        let stored_events = if stored_events.is_empty() {
            // Wait for new events
            self.store.wait_for_events(last_offset_val).await;
            match self
                .store
                .get_events_since(last_offset_val, max_messages)
                .await
            {
                Ok(ev) => ev,
                Err(e) => return Err(e),
            }
        } else {
            stored_events
        };

        let mut new_offset = last_offset_val;
        if let Some(last) = stored_events.last() {
            new_offset = last.offset;
        }

        // Advance offset immediately to support async commit pipelines (like Route)
        self.last_offset.store(new_offset, Ordering::SeqCst);

        // Convert to CanonicalMessage for the consumer
        let events: Vec<CanonicalMessage> = stored_events.into_iter().map(|e| e.message).collect();
        trace!(count = events.len(), subscriber_id = %self.subscriber_id, message_ids = ?LazyMessageIds(&events), "Received batch of events from store");

        let store = self.store.clone();
        let subscriber_id = self.subscriber_id.clone();
        let last_offset_arc = self.last_offset.clone();

        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            let store = store.clone();
            let subscriber_id = subscriber_id.clone();
            let last_offset_arc = last_offset_arc.clone();
            Box::pin(async move {
                if dispositions
                    .iter()
                    .any(|d| matches!(d, MessageDisposition::Nack))
                {
                    last_offset_arc.fetch_min(last_offset_val, Ordering::SeqCst);
                } else {
                    store.ack(&subscriber_id, new_offset).await;
                }
                Ok(())
            })
        });

        Ok(ReceivedBatch {
            messages: events,
            commit,
        })
    }

    async fn status(&self) -> EndpointStatus {
        let last = self.last_offset.load(Ordering::SeqCst);
        let next = self.store.next_offset.load(Ordering::SeqCst);
        // Offsets are 1-based. next_offset is the ID of the *next* event to be written.
        let pending = next.saturating_sub(last).saturating_sub(1) as usize;

        EndpointStatus {
            healthy: true,
            target: self.subscriber_id.clone(),
            pending: Some(pending),
            details: serde_json::json!({
                "mode": "event_store",
                "current_offset": last,
                "head_offset": next.saturating_sub(1)
            }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ConsumerError;
    use std::sync::Arc;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn test_event_store_lifecycle() {
        let store = Arc::new(EventStore::new(RetentionPolicy::default()));

        // Append
        let id1 = store.append(CanonicalMessage::from("msg1")).await;
        assert_eq!(id1, 1);
        let id2 = store.append(CanonicalMessage::from("msg2")).await;
        assert_eq!(id2, 2);

        // Retrieve
        let events = store.get_events_since(0, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].offset, 1);
        assert_eq!(events[1].offset, 2);

        // Retrieve partial
        let events = store.get_events_since(1, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].offset, 2);
    }

    #[tokio::test]
    async fn test_retention_policy_max_count() {
        let policy = RetentionPolicy {
            max_count: Some(2),
            ..Default::default()
        };
        let store = EventStore::new(policy);

        store.append(CanonicalMessage::from("1")).await;
        store.append(CanonicalMessage::from("2")).await;
        store.append(CanonicalMessage::from("3")).await;

        // Should have dropped "1" (offset 1). Base offset should be 2.
        // get_events_since(0) asks for offset 1.
        let res = store.get_events_since(0, 10).await;
        match res {
            Err(ConsumerError::Gap { requested, base }) => {
                assert_eq!(requested, 1);
                assert_eq!(base, 2);
            }
            _ => panic!("Expected Gap error"),
        }

        let events = store.get_events_since(1, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].message.get_payload_str(), "2");
        assert_eq!(events[1].message.get_payload_str(), "3");
    }

    #[tokio::test]
    async fn test_subscriber_ack_gc() {
        let policy = RetentionPolicy {
            max_count: None,
            gc_interval: Duration::from_millis(10), // Fast GC
            subscriber_timeout: Duration::from_secs(60),
            max_age: None,
        };
        let store = Arc::new(EventStore::new(policy));

        store.append(CanonicalMessage::from("1")).await; // Offset 1
        store.append(CanonicalMessage::from("2")).await; // Offset 2

        // Register subscriber
        store.register_subscriber("subA".into()).await;

        // Ack offset 1
        store.ack("subA", 1).await;

        // Force GC (store logic checks interval)
        // We need to wait a bit to satisfy the duration check
        sleep(Duration::from_millis(20)).await;
        store.run_gc().await;

        // Offset 1 should be removed
        let res = store.get_events_since(0, 10).await;
        match res {
            Err(ConsumerError::Gap { requested, base }) => {
                assert_eq!(requested, 1);
                assert_eq!(base, 2);
            }
            _ => panic!("Expected Gap error, got {:?}", res),
        }

        let events = store.get_events_since(1, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].offset, 2);
    }

    #[tokio::test]
    async fn test_append_batch() {
        let store = Arc::new(EventStore::new(RetentionPolicy::default()));
        let messages = vec![
            CanonicalMessage::from("batch1"),
            CanonicalMessage::from("batch2"),
        ];
        let last_offset = store.append_batch(messages).await;
        assert_eq!(last_offset, 2);

        let events = store.get_events_since(0, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].offset, 1);
        assert_eq!(events[1].offset, 2);
    }

    #[tokio::test]
    async fn test_retention_policy_max_age() {
        let policy = RetentionPolicy {
            max_age: Some(Duration::from_millis(50)),
            gc_interval: Duration::from_millis(10),
            max_count: None,
            ..Default::default()
        };
        let store = Arc::new(EventStore::new(policy));
        store.register_subscriber("subA".into()).await;

        store.append(CanonicalMessage::from("1")).await;
        sleep(Duration::from_millis(60)).await;
        store.append(CanonicalMessage::from("2")).await;

        // Acking offset 0 for subA will not remove anything based on acks, but it will trigger GC.
        store.ack("subA", 0).await;
        sleep(Duration::from_millis(20)).await; // ensure gc_interval is met
        store.run_gc().await;

        // "1" should be removed due to max_age.
        let res = store.get_events_since(0, 10).await;
        match res {
            Err(ConsumerError::Gap { requested, base }) => {
                assert_eq!(requested, 1);
                assert_eq!(base, 2);
            }
            _ => panic!("Expected Gap error, got {:?}", res),
        }
    }

    #[tokio::test]
    async fn test_subscriber_timeout_gc() {
        let policy = RetentionPolicy {
            max_count: None,
            gc_interval: Duration::from_millis(10),
            subscriber_timeout: Duration::from_millis(50), // Short timeout
            max_age: None,
        };
        let store = Arc::new(EventStore::new(policy));

        store.append(CanonicalMessage::from("1")).await; // Offset 1
        store.append(CanonicalMessage::from("2")).await; // Offset 2

        // Register two subscribers
        store.register_subscriber("subA".into()).await; // Will time out
        store.register_subscriber("subB".into()).await; // Will stay active

        // Ack offset 1 for subA
        store.ack("subA", 1).await;

        // Wait for subA to time out
        sleep(Duration::from_millis(60)).await;

        // Ack offset 2 for subB
        store.ack("subB", 2).await;

        // Force GC. It should ignore subA's ack state (stuck at 1) and GC up to subB's ack (2).
        store.run_gc().await;

        // Both messages should be gone.
        let res = store.get_events_since(0, 10).await;
        match res {
            Err(ConsumerError::Gap { requested, base }) => {
                assert_eq!(requested, 1);
                assert_eq!(base, 3); // Base offset is next_offset (1-based)
            }
            _ => panic!("Expected Gap error, got {:?}", res),
        }
    }

    #[tokio::test]
    async fn test_event_store_consumer_waits_for_events() {
        let store = Arc::new(EventStore::new(RetentionPolicy::default()));
        let mut consumer = store.consumer("consumer1".to_string());

        let producer_task = tokio::spawn({
            let store = store.clone();
            async move {
                sleep(Duration::from_millis(50)).await;
                store.append(CanonicalMessage::from("event1")).await;
            }
        });

        // Consumer should wait for the event
        let batch = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch.messages.len(), 1);
        assert_eq!(batch.messages[0].get_payload_str(), "event1");

        // Ack the batch
        (batch.commit)(vec![MessageDisposition::Ack]).await.unwrap();

        producer_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_event_store_consumer_nack() {
        let store = Arc::new(EventStore::new(RetentionPolicy::default()));
        let mut consumer = store.consumer("consumer_nack".to_string());

        store.append(CanonicalMessage::from("event1")).await;

        // Receive first batch and Nack it
        let batch1 = consumer.receive_batch(1).await.unwrap();
        (batch1.commit)(vec![MessageDisposition::Nack])
            .await
            .unwrap();

        // Receive again, should get the same message
        let batch2 = consumer.receive_batch(1).await.unwrap();
        assert_eq!(batch2.messages[0].get_payload_str(), "event1");

        // Ack it this time
        (batch2.commit)(vec![MessageDisposition::Ack])
            .await
            .unwrap();
    }
}
