//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Embedded Bridge gRPC server (`server_mode`): hosts the `Bridge` service plus
//! reflection, and feeds published messages into the route as a consumer.

use super::dynamic::reject_unsupported_call_metadata;
use super::proto;
use super::{bridge_to_canonical, canonical_to_bridge, parse_addr};
use crate::models::GrpcConfig;
use crate::traits::{ConsumerError, MessageConsumer, MessageDisposition};
use anyhow::Result;
use async_trait::async_trait;
use proto::{BridgeMessage, SubscribeRequest};
use std::any::Any;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::Server as TonicServer;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};
use tonic::{Request, Response, Status};
use tracing::{error, info, trace, warn};

/// `publish_batch` messages that may be dispatched but not yet answered. Bounds how far
/// the dispatch task can run ahead of the commits it is waiting on.
const PUBLISH_BATCH_INFLIGHT: usize = 1024;
/// Unacknowledged messages retained per subscriber, and subscribers retained per route.
pub(super) const MAX_PENDING_PER_CONSUMER: usize = 1024;
pub(super) const MAX_PENDING_CONSUMERS: usize = 64;

pub(super) fn publish_response_for_disposition(
    id: String,
    disposition: MessageDisposition,
) -> proto::PublishResponse {
    match disposition {
        MessageDisposition::Reply(message) => {
            let mut reply = canonical_to_bridge(message, None);
            reply.id = id;
            proto::PublishResponse {
                result: Some(proto::publish_response::Result::Reply(reply)),
            }
        }
        MessageDisposition::Ack => proto::PublishResponse {
            result: Some(proto::publish_response::Result::Ack(proto::Ack {
                id,
                status: proto::ack::Status::Ack as i32,
                reason: String::new(),
                metadata: Default::default(),
            })),
        },
        MessageDisposition::Nack => proto::PublishResponse {
            result: Some(proto::publish_response::Result::Ack(proto::Ack {
                id,
                status: proto::ack::Status::Nack as i32,
                reason: "Downstream processing failed".to_string(),
                metadata: Default::default(),
            })),
        },
    }
}

// ── Embedded gRPC server (server_mode) ────────────────────────────────────────

pub(super) struct ServerModeConsumer {
    pub(super) route_id: u64,
    pub(super) shared_server: Arc<SharedGrpcServer>,
    pub(super) bound_addr: std::net::SocketAddr,
    // One receive channel per shard; publishes are spread round-robin across the
    // shards so many concurrent producers don't all contend on one channel.
    pub(super) rxs: Vec<mpsc::Receiver<InboundDelivery>>,
    // Round-robin cursor for the next shard to drain first, so none starves.
    pub(super) drain_start: usize,
    /// Drain mode: only then does an idle first-message poll time out into an empty batch.
    pub(super) exit_on_empty: bool,
}

pub(super) const REFLECTION_V1_PREFIX: &str = "/grpc.reflection.v1.ServerReflection/";
pub(super) const REFLECTION_V1ALPHA_PREFIX: &str = "/grpc.reflection.v1alpha.ServerReflection/";

/// Sends one gRPC path prefix to `matched` and everything else to `fallback`, so hosting
/// reflection alongside the Bridge service does not pull in tonic's axum-backed router.
///
/// Every generated tonic service shares one response type and already returns a boxed
/// future, so this dispatches by delegation alone — no wrapping, no extra allocation.
/// Nest it to add further services.
#[derive(Clone)]
pub(super) struct PrefixRouter<F, M> {
    pub(super) fallback: F,
    pub(super) prefix: &'static str,
    pub(super) matched: M,
}

impl<F, M, B> tonic::codegen::Service<tonic::codegen::http::Request<B>> for PrefixRouter<F, M>
where
    F: tonic::codegen::Service<tonic::codegen::http::Request<B>>,
    M: tonic::codegen::Service<
        tonic::codegen::http::Request<B>,
        Response = F::Response,
        Error = F::Error,
        Future = F::Future,
    >,
{
    type Response = F::Response;
    type Error = F::Error;
    type Future = F::Future;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        match self.fallback.poll_ready(cx) {
            std::task::Poll::Ready(Ok(())) => self.matched.poll_ready(cx),
            other => other,
        }
    }

    fn call(&mut self, request: tonic::codegen::http::Request<B>) -> Self::Future {
        // Unmatched paths go to the fallback, whose generated service answers anything it
        // does not recognise with UNIMPLEMENTED.
        if request.uri().path().starts_with(self.prefix) {
            self.matched.call(request)
        } else {
            self.fallback.call(request)
        }
    }
}

/// Tonic service implementation that fans incoming messages into a subscriber
/// broadcast stream and a reliable internal queue for the server-mode consumer.
pub(super) struct BridgeService {
    pub(super) router: Arc<SharedGrpcRouter>,
    /// How long to wait for the consuming route to commit a published message before
    /// answering NACK. `None` (no `timeout_ms`) waits indefinitely, so a route that never
    /// commits blocks the publisher — set `timeout_ms` to bound it.
    pub(super) commit_timeout: Option<Duration>,
}

/// Wait for the route's disposition, treating an expired `commit_timeout` or a dropped
/// sender as a NACK: either way the message was not confirmed committed.
async fn await_disposition(
    receipt: oneshot::Receiver<MessageDisposition>,
    commit_timeout: Option<Duration>,
) -> MessageDisposition {
    match commit_timeout {
        Some(limit) => match tokio::time::timeout(limit, receipt).await {
            Ok(disposition) => disposition.unwrap_or(MessageDisposition::Nack),
            Err(_) => {
                warn!(
                    ?limit,
                    "gRPC publish timed out waiting for the route to commit"
                );
                MessageDisposition::Nack
            }
        },
        None => receipt.await.unwrap_or(MessageDisposition::Nack),
    }
}

pub(super) struct SharedGrpcRouter {
    // RwLock (not Mutex): `dispatch` only reads the table, so concurrent publishes
    // no longer serialize against each other on the lock.
    pub(super) routes: RwLock<HashMap<u64, SharedGrpcRoute>>,
}

#[derive(Clone)]
pub(super) struct SharedGrpcRoute {
    pub(super) topic: String,
    // Sharded senders; `cursor` round-robins publishes across them. `cursor` is
    // shared (Arc) so all clones of this route advance the same counter.
    pub(super) txs: Vec<mpsc::Sender<InboundDelivery>>,
    pub(super) cursor: Arc<AtomicUsize>,
    pub(super) broadcast_tx: broadcast::Sender<BridgeMessage>,
    pub(super) subscriber_pending: Arc<Mutex<SubscriberPending>>,
    /// `consumer_id`s with a live subscribe stream, so a duplicate is rejected rather than
    /// silently sharing the first one's retention set.
    pub(super) active_subscribers: Arc<Mutex<HashSet<String>>>,
}

pub(super) struct InboundDelivery {
    pub(super) message: BridgeMessage,
    pub(super) completion: oneshot::Sender<MessageDisposition>,
}

/// A `publish_batch` message that has been dispatched and is waiting for its response.
enum Pending {
    /// Resolves to the disposition the consuming route committed.
    Receipt(oneshot::Receiver<MessageDisposition>),
    /// Never reached a consumer; answered with this reason.
    Nack(&'static str),
}

/// Unacknowledged messages retained for one subscriber, so a consumer reconnecting with
/// the same `consumer_id` is redelivered them.
///
/// Both caps are hard: retention is a redelivery aid for a running server, not durable
/// storage, and a subscriber that never acks would otherwise grow the server without
/// bound. `unacked` is authoritative — `queue` keeps arrival order and may hold already
/// acked entries until it is compacted, which keeps every operation O(1) amortized.
#[derive(Default)]
pub(super) struct PendingMessages {
    queue: VecDeque<BridgeMessage>,
    unacked: HashSet<String>,
}

impl PendingMessages {
    pub(super) fn retain(&mut self, msg: &BridgeMessage) {
        if !self.unacked.insert(msg.id.clone()) {
            return;
        }
        if self.queue.len() >= MAX_PENDING_PER_CONSUMER {
            self.queue.retain(|held| self.unacked.contains(&held.id));
        }
        if self.queue.len() >= MAX_PENDING_PER_CONSUMER {
            if let Some(dropped) = self.queue.pop_front() {
                warn!(
                    msg_id = %dropped.id,
                    "gRPC subscriber holds too many unacknowledged messages, dropping the oldest"
                );
                self.unacked.remove(&dropped.id);
            }
        }
        self.queue.push_back(msg.clone());
    }

    pub(super) fn is_unacked(&self, msg_id: &str) -> bool {
        self.unacked.contains(msg_id)
    }

    /// `true` if the id was still awaiting acknowledgement.
    pub(super) fn acknowledge(&mut self, msg_id: &str) -> bool {
        self.unacked.remove(msg_id)
    }

    pub(super) fn replay(&self) -> Vec<BridgeMessage> {
        self.queue
            .iter()
            .filter(|msg| self.unacked.contains(&msg.id))
            .cloned()
            .collect()
    }
}

/// Per-subscriber retention for one route, capped in both dimensions.
#[derive(Default)]
pub(super) struct SubscriberPending {
    by_consumer: HashMap<String, PendingMessages>,
    /// Insertion order of `by_consumer`, so the oldest subscriber can be evicted. A
    /// consumer that never reconnects (the default id is per-instance) would otherwise
    /// leave its entry behind forever.
    order: VecDeque<String>,
}

impl SubscriberPending {
    pub(super) fn entry(&mut self, consumer_id: &str) -> &mut PendingMessages {
        if !self.by_consumer.contains_key(consumer_id) {
            if self.order.len() >= MAX_PENDING_CONSUMERS {
                if let Some(evicted) = self.order.pop_front() {
                    warn!(
                        consumer_id = %evicted,
                        "gRPC subscriber retention is full, dropping the oldest subscriber"
                    );
                    self.by_consumer.remove(&evicted);
                }
            }
            self.order.push_back(consumer_id.to_string());
            self.by_consumer
                .insert(consumer_id.to_string(), PendingMessages::default());
        }
        self.by_consumer
            .get_mut(consumer_id)
            .expect("entry was just inserted")
    }

    pub(super) fn get(&self, consumer_id: &str) -> Option<&PendingMessages> {
        self.by_consumer.get(consumer_id)
    }

    fn get_mut(&mut self, consumer_id: &str) -> Option<&mut PendingMessages> {
        self.by_consumer.get_mut(consumer_id)
    }

    fn remove(&mut self, consumer_id: &str) {
        self.by_consumer.remove(consumer_id);
        self.order.retain(|held| held != consumer_id);
    }
}

/// Holds a `consumer_id` for the lifetime of one subscribe stream, so a second stream
/// cannot claim the same id while the first is live. Released on drop, however the
/// stream's task ends.
struct SubscriptionClaim {
    consumer_id: String,
    active: Arc<Mutex<HashSet<String>>>,
    /// Set for a server-generated id, whose retention is worthless once the stream ends
    /// because no client can ever reconnect under it.
    drop_pending: Option<Arc<Mutex<SubscriberPending>>>,
}

impl SubscriptionClaim {
    fn acquire(route: &SharedGrpcRoute, consumer_id: String, ephemeral: bool) -> Option<Self> {
        let mut active = route.active_subscribers.lock().ok()?;
        if !active.insert(consumer_id.clone()) {
            return None;
        }
        drop(active);
        Some(Self {
            consumer_id,
            active: route.active_subscribers.clone(),
            drop_pending: ephemeral.then(|| route.subscriber_pending.clone()),
        })
    }
}

impl Drop for SubscriptionClaim {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.consumer_id);
        }
        if let Some(pending) = &self.drop_pending {
            if let Ok(mut pending) = pending.lock() {
                pending.remove(&self.consumer_id);
            }
        }
    }
}

pub(super) struct SharedGrpcServer {
    pub(super) router: Arc<SharedGrpcRouter>,
    pub(super) handle: tokio::task::JoinHandle<()>,
    pub(super) bound_addr: std::net::SocketAddr,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct GrpcServerKey {
    listen_addr: String,
    tls: crate::models::TlsConfig,
    request_timeout_ms: Option<u64>,
    initial_stream_window_size: Option<u32>,
    initial_connection_window_size: Option<u32>,
    concurrency_limit_per_connection: Option<usize>,
    http2_keepalive_interval_ms: Option<u64>,
    http2_keepalive_timeout_ms: Option<u64>,
    max_decoding_message_size: Option<usize>,
}

static GRPC_SERVER_REGISTRY: OnceLock<Mutex<HashMap<GrpcServerKey, Arc<SharedGrpcServer>>>> =
    OnceLock::new();
pub(super) static GRPC_ROUTE_ID: AtomicU64 = AtomicU64::new(1);

fn grpc_server_registry() -> &'static Mutex<HashMap<GrpcServerKey, Arc<SharedGrpcServer>>> {
    GRPC_SERVER_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn normalize_grpc_topic(topic: Option<&str>) -> String {
    topic
        .map(str::trim)
        .filter(|topic| !topic.is_empty())
        .unwrap_or("default")
        .to_string()
}

impl SharedGrpcRouter {
    pub(super) fn new() -> Self {
        Self {
            routes: RwLock::new(HashMap::new()),
        }
    }
}

fn bridge_message_topic(msg: &BridgeMessage) -> String {
    normalize_grpc_topic(msg.metadata.get("mq_bridge.topic").map(String::as_str))
}

impl SharedGrpcRouter {
    pub(super) fn register_route(
        &self,
        route_id: u64,
        topic: String,
        txs: Vec<mpsc::Sender<InboundDelivery>>,
    ) -> Result<()> {
        let mut routes = self
            .routes
            .write()
            .map_err(|_| anyhow::anyhow!("gRPC route registry lock poisoned"))?;
        if routes.values().any(|route| route.topic == topic) {
            return Err(anyhow::anyhow!(
                "Conflicting gRPC consumer registration for topic '{}'",
                topic
            ));
        }
        let (broadcast_tx, _) = broadcast::channel(1024);
        routes.insert(
            route_id,
            SharedGrpcRoute {
                topic,
                txs,
                cursor: Arc::new(AtomicUsize::new(0)),
                broadcast_tx,
                subscriber_pending: Arc::new(Mutex::new(SubscriberPending::default())),
                active_subscribers: Arc::new(Mutex::new(HashSet::new())),
            },
        );
        Ok(())
    }

    fn unregister_route(&self, route_id: u64) -> bool {
        let Ok(mut routes) = self.routes.write() else {
            return false;
        };
        routes.remove(&route_id);
        routes.is_empty()
    }

    fn route_for_topic(&self, topic: &str) -> Option<SharedGrpcRoute> {
        let Ok(routes) = self.routes.read() else {
            return None;
        };
        routes.values().find(|route| route.topic == topic).cloned()
    }

    pub(super) async fn dispatch(
        &self,
        mut msg: BridgeMessage,
    ) -> Result<oneshot::Receiver<MessageDisposition>> {
        if msg.id.is_empty() {
            msg.id = fast_uuid_v7::gen_id().to_string();
        }
        let topic = bridge_message_topic(&msg);
        let route = self
            .route_for_topic(&topic)
            .ok_or_else(|| anyhow::anyhow!("No route for topic '{}'", topic))?;
        {
            let active = route
                .active_subscribers
                .lock()
                .map_err(|_| anyhow::anyhow!("gRPC active subscriber lock poisoned"))?;
            if !active.is_empty() {
                let mut pending = route
                    .subscriber_pending
                    .lock()
                    .map_err(|_| anyhow::anyhow!("gRPC subscriber retention lock poisoned"))?;
                for consumer_id in active.iter() {
                    pending.entry(consumer_id).retain(&msg);
                }
            }
        }
        // Only clone for the broadcast stream when someone is actually subscribed.
        if route.broadcast_tx.receiver_count() > 0 {
            let _ = route.broadcast_tx.send(msg.clone());
        }
        let shard = route.cursor.fetch_add(1, Ordering::Relaxed) % route.txs.len();
        let (completion, receipt) = oneshot::channel();
        route.txs[shard]
            .send(InboundDelivery {
                message: msg,
                completion,
            })
            .await
            .map_err(|_| anyhow::anyhow!("No active gRPC consumer for topic '{}'", topic))?;
        Ok(receipt)
    }
}

#[tonic::async_trait]
impl proto::bridge_server::Bridge for BridgeService {
    async fn publish(
        &self,
        request: Request<BridgeMessage>,
    ) -> Result<Response<proto::PublishResponse>, Status> {
        let msg = request.into_inner();
        let msg_id = msg.id.clone();
        let topic = bridge_message_topic(&msg);
        trace!(msg_id = %msg_id, topic = %topic, "BridgeService::publish received message");
        let receipt = match self.router.dispatch(msg).await {
            Ok(receipt) => receipt,
            Err(_) => {
                warn!(msg_id = %msg_id, topic = %topic, "BridgeService::publish failed: internal server queue is closed");
                return Ok(Response::new(proto::PublishResponse {
                    result: Some(proto::publish_response::Result::Ack(proto::Ack {
                        id: msg_id,
                        status: 1, // NACK
                        reason: "Internal queue closed".to_string(),
                        metadata: Default::default(),
                    })),
                }));
            }
        };
        let disposition = await_disposition(receipt, self.commit_timeout).await;
        Ok(Response::new(publish_response_for_disposition(
            msg_id,
            disposition,
        )))
    }

    async fn acknowledge(
        &self,
        request: Request<proto::Ack>,
    ) -> Result<Response<proto::AckResponse>, Status> {
        let ack = request.into_inner();
        trace!(ack_id = %ack.id, "BridgeService::acknowledge received ack");
        // Without an id there is no retention set to resolve the ack against, so reporting
        // success would tell the caller its message was committed when nothing was tracked.
        let Some(consumer_id) = ack.metadata.get("mq_bridge.consumer_id") else {
            return Ok(Response::new(proto::AckResponse {
                success: false,
                error: "Ack is missing the mq_bridge.consumer_id metadata entry".to_string(),
            }));
        };
        let acked = ack.status == proto::ack::Status::Ack as i32;
        let mut found = false;
        if let Ok(routes) = self.router.routes.read() {
            for route in routes.values() {
                let Ok(mut pending) = route.subscriber_pending.lock() else {
                    continue;
                };
                let Some(messages) = pending.get_mut(consumer_id) else {
                    continue;
                };
                if !messages.is_unacked(&ack.id) {
                    continue;
                }
                // A NACK leaves the message pending so it is redelivered on reconnect.
                found = if acked {
                    messages.acknowledge(&ack.id)
                } else {
                    true
                };
                break;
            }
        }
        Ok(Response::new(proto::AckResponse {
            success: found,
            error: if found {
                String::new()
            } else {
                "Unknown consumer or message".to_string()
            },
        }))
    }

    type PublishBatchStream = ReceiverStream<Result<proto::PublishResponse, Status>>;

    async fn publish_batch(
        &self,
        request: Request<tonic::Streaming<BridgeMessage>>,
    ) -> Result<Response<Self::PublishBatchStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(32);
        let router = self.router.clone();

        // Dispatch and commit-wait run in separate tasks. Awaiting a receipt inline would
        // keep the next message from reaching the consumer until this one had committed,
        // serializing the stream and forcing every consumer batch to hold one message.
        let (pending_tx, mut pending_rx) =
            mpsc::channel::<(String, Pending)>(PUBLISH_BATCH_INFLIGHT);

        let commit_timeout = self.commit_timeout;
        tokio::spawn(async move {
            while let Some((msg_id, pending)) = pending_rx.recv().await {
                let resp = match pending {
                    Pending::Receipt(receipt) => publish_response_for_disposition(
                        msg_id,
                        await_disposition(receipt, commit_timeout).await,
                    ),
                    Pending::Nack(reason) => proto::PublishResponse {
                        result: Some(proto::publish_response::Result::Ack(proto::Ack {
                            id: msg_id,
                            status: proto::ack::Status::Nack as i32,
                            reason: reason.to_string(),
                            metadata: Default::default(),
                        })),
                    },
                };
                if tx.send(Ok(resp)).await.is_err() {
                    warn!("publish_batch: client stream closed, stopping responder task");
                    break;
                }
            }
            trace!("publish_batch responder task exiting");
        });

        tokio::spawn(async move {
            while let Ok(Some(msg)) = stream.message().await {
                let msg_id = msg.id.clone();
                let topic = bridge_message_topic(&msg);
                trace!(msg_id = %msg_id, topic = %topic, "BridgeService::publish_batch received message");
                let pending = match router.dispatch(msg).await {
                    Ok(receipt) => Pending::Receipt(receipt),
                    Err(_) => {
                        warn!(
                            "publish_batch: internal server queue closed, stopping dispatch task"
                        );
                        // Queued rather than sent directly, so this terminal NACK still
                        // arrives after the responses for the messages before it.
                        let _ = pending_tx
                            .send((msg_id, Pending::Nack("Internal queue closed")))
                            .await;
                        break;
                    }
                };
                if pending_tx.send((msg_id, pending)).await.is_err() {
                    break;
                }
            }
            trace!("publish_batch dispatch task exiting");
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    type SubscribeStream = ReceiverStream<Result<BridgeMessage, Status>>;

    async fn subscribe(
        &self,
        request: Request<SubscribeRequest>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let request = request.into_inner();
        let topic = normalize_grpc_topic(Some(request.topic.as_str()));
        // An id the server made up cannot be reconnected to, so its retention is dropped
        // when the stream ends instead of waiting to be evicted by the cap.
        let ephemeral = request.consumer_id.is_empty();
        let consumer_id = if ephemeral {
            fast_uuid_v7::gen_id().to_string()
        } else {
            request.consumer_id
        };
        let route = self
            .router
            .route_for_topic(&topic)
            .ok_or_else(|| Status::not_found(format!("No active gRPC topic '{}'", topic)))?;

        // One stream per id. Two live subscriptions sharing an id would both be fanned the
        // same broadcast messages but share one retention set, so whichever acked first
        // would remove the entry and the other's ack would come back rejected.
        let claim = SubscriptionClaim::acquire(&route, consumer_id.clone(), ephemeral).ok_or_else(|| {
            Status::already_exists(format!(
                "gRPC consumer_id '{consumer_id}' already has an active subscription on topic '{topic}'"
            ))
        })?;

        let mut rx = route.broadcast_tx.subscribe();
        let replay = route
            .subscriber_pending
            .lock()
            .ok()
            .and_then(|pending| pending.get(&consumer_id).map(PendingMessages::replay))
            .unwrap_or_default();
        let replayed_ids: HashSet<_> = replay.iter().map(|msg| msg.id.clone()).collect();
        let (tx_stream, rx_stream) = mpsc::channel(32);
        tokio::spawn(async move {
            // Releases the id, and an ephemeral consumer's retention with it, however this
            // task ends.
            let _claim = claim;
            for msg in replay {
                if tx_stream.send(Ok(msg)).await.is_err() {
                    return;
                }
            }
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        // A dispatch racing the replay snapshot is both retained and broadcast.
                        // The retained copy was already sent above, so do not send it twice.
                        if replayed_ids.contains(&msg.id) {
                            continue;
                        }
                        if tx_stream.send(Ok(msg)).await.is_err() {
                            warn!("subscribe: downstream consumer disconnected");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(
                            skipped,
                            "subscribe: subscriber lagged; closing stream for retained replay"
                        );
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx_stream)))
    }
}

impl ServerModeConsumer {
    pub(super) async fn new(config: &GrpcConfig, url: &str) -> Result<Self> {
        reject_unsupported_call_metadata(config, "server")?;
        let key = GrpcServerKey {
            listen_addr: parse_addr(url)?.to_string(),
            tls: config.tls.clone(),
            request_timeout_ms: config.request_timeout_ms.or(config.timeout_ms),
            initial_stream_window_size: config.initial_stream_window_size,
            initial_connection_window_size: config.initial_connection_window_size,
            concurrency_limit_per_connection: config.concurrency_limit_per_connection,
            http2_keepalive_interval_ms: config.http2_keepalive_interval_ms,
            http2_keepalive_timeout_ms: config.http2_keepalive_timeout_ms,
            max_decoding_message_size: config.max_decoding_message_size,
        };
        let topic = normalize_grpc_topic(config.topic.as_deref());
        // Total queue depth stays ~16k, split across shards to cut producer contention.
        let shard_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .clamp(1, 16);
        let per_shard = ((16 * 1024) / shard_count).max(1);
        let mut txs = Vec::with_capacity(shard_count);
        let mut rxs = Vec::with_capacity(shard_count);
        for _ in 0..shard_count {
            let (tx, rx) = mpsc::channel(per_shard);
            txs.push(tx);
            rxs.push(rx);
        }
        let route_id = GRPC_ROUTE_ID.fetch_add(1, Ordering::Relaxed);
        let shared_server =
            get_or_create_shared_grpc_server(config, &key, route_id, topic, txs).await?;

        Ok(Self {
            route_id,
            bound_addr: shared_server.bound_addr,
            shared_server,
            rxs,
            drain_start: 0,
            exit_on_empty: false,
        })
    }

    /// True while the embedded server task is still running.
    pub(super) fn server_is_running(&self) -> bool {
        !self.shared_server.handle.is_finished()
    }

    pub(super) fn bound_addr(&self) -> std::net::SocketAddr {
        self.bound_addr
    }
}

async fn get_or_create_shared_grpc_server(
    config: &GrpcConfig,
    key: &GrpcServerKey,
    route_id: u64,
    topic: String,
    txs: Vec<mpsc::Sender<InboundDelivery>>,
) -> Result<Arc<SharedGrpcServer>> {
    if let Ok(registry) = grpc_server_registry().lock() {
        for (existing_key, server) in registry.iter() {
            if existing_key.listen_addr != key.listen_addr {
                continue;
            }
            if existing_key == key {
                server
                    .router
                    .register_route(route_id, topic.clone(), txs.clone())?;
                return Ok(server.clone());
            }
            return Err(anyhow::anyhow!(
                "gRPC consumer {} is already registered with different server settings",
                key.listen_addr
            ));
        }
    }

    let addr = parse_addr(&key.listen_addr)?;
    let router = Arc::new(SharedGrpcRouter::new());
    let mut builder = TonicServer::builder();
    if let Some(v) = config.initial_stream_window_size {
        builder = builder.initial_stream_window_size(v);
    }
    if let Some(v) = config.initial_connection_window_size {
        builder = builder.initial_connection_window_size(v);
    }
    if let Some(v) = config.concurrency_limit_per_connection {
        builder = builder.concurrency_limit_per_connection(v);
    }
    if let Some(ms) = config.http2_keepalive_interval_ms {
        builder = builder.http2_keepalive_interval(Some(Duration::from_millis(ms)));
    }
    if let Some(ms) = config.http2_keepalive_timeout_ms {
        builder = builder.http2_keepalive_timeout(Some(Duration::from_millis(ms)));
    }
    if let Some(ms) = config.request_timeout_ms.or(config.timeout_ms) {
        builder = builder.timeout(Duration::from_millis(ms));
    }

    if config.tls.required {
        if !config.tls.is_tls_server_configured() {
            return Err(anyhow::anyhow!(
                "gRPC server TLS enabled but no cert/key provided in GrpcConfig"
            ));
        }
        let cert_path = config.tls.cert_file.as_ref().unwrap();
        let key_path = config.tls.key_file.as_ref().unwrap();
        let cert = tokio::fs::read(cert_path).await?;
        let key = tokio::fs::read(key_path).await?;
        let identity = Identity::from_pem(cert, key);

        let mut tls_config = ServerTlsConfig::new().identity(identity);
        if let Some(ca_path) = &config.tls.ca_file {
            let ca_pem = tokio::fs::read(ca_path).await?;
            let ca_cert = Certificate::from_pem(ca_pem);
            tls_config = tls_config.client_ca_root(ca_cert);
        }

        builder = builder.tls_config(tls_config)?;
    }

    let mut service = proto::bridge_server::BridgeServer::new(BridgeService {
        router: router.clone(),
        commit_timeout: config
            .request_timeout_ms
            .or(config.timeout_ms)
            .map(Duration::from_millis),
    });
    if let Some(max) = config.max_decoding_message_size {
        service = service.max_decoding_message_size(max);
    }

    // Bind the TCP listener first so we know the server port is bound and
    // listening before returning. This avoids races where the consumer
    // tries to connect before the server is ready.
    info!(addr = %addr, "Binding gRPC embedded server listener");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    info!(server_addr = %local, "gRPC embedded server listener bound");
    let incoming = TcpListenerStream::new(listener);

    // Both reflection versions: v1 for current tooling, v1alpha for older grpcurl/evans.
    let configure_reflection = || {
        tonic_reflection::server::Builder::configure()
            .register_encoded_file_descriptor_set(proto::FILE_DESCRIPTOR_SET)
    };
    let services = PrefixRouter {
        fallback: PrefixRouter {
            fallback: service,
            prefix: REFLECTION_V1_PREFIX,
            matched: configure_reflection().build_v1()?,
        },
        prefix: REFLECTION_V1ALPHA_PREFIX,
        matched: configure_reflection().build_v1alpha()?,
    };
    let handle = tokio::spawn(async move {
        info!(server_addr = %local, "gRPC embedded server starting to serve");
        if let Err(e) = builder.serve_with_incoming(services, incoming).await {
            error!(server_addr = %local, "gRPC server error: {:?}", e);
        }
        info!(server_addr = %local, "gRPC embedded server stopped");
    });

    let server = Arc::new(SharedGrpcServer {
        router,
        handle,
        bound_addr: local,
    });

    let mut registry = grpc_server_registry()
        .lock()
        .map_err(|_| anyhow::anyhow!("gRPC server registry lock poisoned"))?;
    for (existing_key, existing) in registry.iter() {
        if existing_key.listen_addr != key.listen_addr {
            continue;
        }
        if existing_key == key {
            server.handle.abort();
            existing
                .router
                .register_route(route_id, topic.clone(), txs.clone())?;
            return Ok(existing.clone());
        }
        server.handle.abort();
        return Err(anyhow::anyhow!(
            "gRPC consumer {} is already registered with different server settings",
            key.listen_addr
        ));
    }
    server.router.register_route(route_id, topic, txs)?;
    registry.insert(key.clone(), server.clone());
    Ok(server)
}

impl Drop for ServerModeConsumer {
    fn drop(&mut self) {
        let Ok(mut registry) = grpc_server_registry().lock() else {
            return;
        };
        let should_shutdown = self.shared_server.router.unregister_route(self.route_id);
        if !should_shutdown {
            return;
        }

        registry.retain(|_, server| !Arc::ptr_eq(server, &self.shared_server));
        self.shared_server.handle.abort();
    }
}

#[async_trait]
impl MessageConsumer for ServerModeConsumer {
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<crate::outcomes::ReceivedBatch, ConsumerError> {
        let max_messages = max_messages.max(1);
        let shard_count = self.rxs.len();
        let mut messages = Vec::with_capacity(max_messages);
        let mut completions = Vec::with_capacity(max_messages);
        'fill: loop {
            // Greedily sweep all shards for whatever is immediately available.
            let mut got_any = false;
            for offset in 0..shard_count {
                let idx = (self.drain_start + offset) % shard_count;
                if let Ok(delivery) = self.rxs[idx].try_recv() {
                    messages.push(bridge_to_canonical(delivery.message));
                    completions.push(delivery.completion);
                    got_any = true;
                    if messages.len() >= max_messages {
                        break 'fill;
                    }
                }
            }
            if got_any {
                self.drain_start = (self.drain_start + 1) % shard_count;
                continue;
            }

            // Everything currently buffered has been drained. Return a partial batch
            // immediately so publishers waiting for its commit are not held behind the
            // stream-oriented batching linger.
            if !messages.is_empty() {
                break;
            }

            // Nothing buffered yet: block for the first message. Polling every shard
            // registers our waker on each.
            let start = self.drain_start;
            let poll = std::future::poll_fn(|cx| {
                let mut all_closed = true;
                for offset in 0..shard_count {
                    let idx = (start + offset) % shard_count;
                    match self.rxs[idx].poll_recv(cx) {
                        std::task::Poll::Ready(Some(msg)) => {
                            self.drain_start = (idx + 1) % shard_count;
                            return std::task::Poll::Ready(Some(msg));
                        }
                        std::task::Poll::Ready(None) => {}
                        std::task::Poll::Pending => all_closed = false,
                    }
                }
                if all_closed {
                    std::task::Poll::Ready(None)
                } else {
                    std::task::Poll::Pending
                }
            });
            // Drain mode: a brief idle timeout on the first message yields an empty batch.
            let next = match crate::traits::drain_gated(self.exit_on_empty, poll).await {
                Some(value) => value,
                None => return Ok(crate::outcomes::ReceivedBatch::empty()),
            };
            match next {
                Some(delivery) => {
                    messages.push(bridge_to_canonical(delivery.message));
                    completions.push(delivery.completion);
                }
                None => break, // every shard closed
            }
        }
        if messages.is_empty() {
            Err(ConsumerError::EndOfStream)
        } else {
            let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
                Box::pin(async move {
                    if dispositions.len() != completions.len() {
                        anyhow::bail!(
                            "gRPC server batch commit length mismatch: dispositions={}, messages={}",
                            dispositions.len(),
                            completions.len()
                        );
                    }
                    for (completion, disposition) in completions.into_iter().zip(dispositions) {
                        let _ = completion.send(disposition);
                    }
                    Ok(())
                }) as futures::future::BoxFuture<'static, anyhow::Result<()>>
            });
            Ok(crate::outcomes::ReceivedBatch { messages, commit })
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
