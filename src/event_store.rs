//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details

use crate::traits::{
    BatchCommitFunc, ConsumerError, MessageConsumer, MessageDisposition, ReceivedBatch,
};
use crate::CanonicalMessage;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;
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
    notify: Arc<Notify>,
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
        Self {
            events: RwLock::new(VecDeque::new()),
            subscribers: RwLock::new(HashMap::new()),
            next_offset: AtomicU64::new(1), // Start offsets at 1
            base_offset: AtomicU64::new(1),
            retention_policy,
            notify: Arc::new(Notify::new()),
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

        let offset = self.next_offset.fetch_add(1, Ordering::SeqCst);
        let stored = StoredEvent {
            message: event,
            offset,
            stored_at: timestamp,
        };

        let mut events = self.events.write().unwrap();
        events.push_back(stored);

        // Enforce max_count immediately
        if let Some(max) = self.retention_policy.max_count {
            if events.len() > max {
                let remove_count = events.len() - max;
                let removed: Vec<StoredEvent> = events.drain(0..remove_count).collect();
                self.base_offset
                    .fetch_add(remove_count as u64, Ordering::SeqCst);
                let total_dropped = self
                    .dropped_events
                    .fetch_add(remove_count as u64, Ordering::SeqCst)
                    + remove_count as u64;

                if let Some(cb) = &self.on_drop {
                    cb(removed);
                }

                warn!(
                    "Retention policy enforced (max_count={}): dropped {} events (total dropped: {}). Slow subscribers may miss events.",
                    max, remove_count, total_dropped
                );
            }
        }
        drop(events);

        trace!("Appended event offset {}", offset);
        // Notify waiting subscribers
        self.notify.notify_waiters();

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
        let start_offset = self.next_offset.fetch_add(count, Ordering::SeqCst);

        let stored_events = messages
            .into_iter()
            .enumerate()
            .map(|(i, event)| StoredEvent {
                message: event,
                offset: start_offset + i as u64,
                stored_at: timestamp,
            });

        let mut events = self.events.write().unwrap();
        events.extend(stored_events);

        let last_offset = start_offset + count - 1;

        // Enforce max_count immediately
        if let Some(max) = self.retention_policy.max_count {
            if events.len() > max {
                let remove_count = events.len() - max;
                let removed: Vec<StoredEvent> = events.drain(0..remove_count).collect();
                self.base_offset
                    .fetch_add(remove_count as u64, Ordering::SeqCst);
                let total_dropped = self
                    .dropped_events
                    .fetch_add(remove_count as u64, Ordering::SeqCst)
                    + remove_count as u64;

                if let Some(cb) = &self.on_drop {
                    cb(removed);
                }

                warn!(
                    "Retention policy enforced (max_count={}): dropped {} events (total dropped: {}). Slow subscribers may miss events.",
                    max, remove_count, total_dropped
                );
            }
        }
        drop(events);

        trace!("Appended batch up to offset {}", last_offset);
        // Notify waiting subscribers
        self.notify.notify_waiters();

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
        {
            let events = self.events.read().unwrap();
            let base = self.base_offset.load(Ordering::SeqCst);
            let max_offset = base + events.len() as u64;
            if max_offset > last_known_offset + 1 {
                return;
            }
        }
        self.notify.notified().await;
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

        if remove_count > 0 {
            debug!("GC removing {} events", remove_count);
            let removed: Vec<StoredEvent> = events.drain(0..remove_count).collect();
            self.base_offset
                .fetch_add(remove_count as u64, Ordering::SeqCst);

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

        // Convert to CanonicalMessage for the consumer
        let events: Vec<CanonicalMessage> = stored_events.into_iter().map(|e| e.message).collect();

        let store = self.store.clone();
        let subscriber_id = self.subscriber_id.clone();
        let last_offset_arc = self.last_offset.clone();

        let commit: BatchCommitFunc = Box::new(move |dispositions| {
            Box::pin(async move {
                if !dispositions
                    .iter()
                    .any(|d| matches!(d, MessageDisposition::Nack))
                {
                    store.ack(&subscriber_id, new_offset).await;
                    last_offset_arc.store(new_offset, Ordering::SeqCst);
                }
                Ok(())
            })
        });

        Ok(ReceivedBatch {
            messages: events,
            commit,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
