//! Alternative ZeroMQ backend built on omq.rs (`omq-tokio`), behind the
//! `zeromq-omq` feature. It reuses the shared framing/format codec
//! (`super::codec`) and the same [`ZeroMqConfig`] surface as the zmq.rs backend,
//! covering PUSH/PULL, PUB/SUB and REQ/REP. Selected from config via
//! `backend: omq` (or the default `try_omq`) on a `zeromq` endpoint — see
//! [`ZeroMqBackend`](crate::models::ZeroMqBackend).
//!
//! omq's `Socket` is a cheaply-cloneable actor handle whose `send`/`recv` take
//! `&self` and apply HWM backpressure internally, so unlike the zmq.rs backend
//! this needs no spawn-a-task + channel plumbing. REQ/REP still has to serialise
//! its exchanges by hand, because ZMTP requires strict send/recv alternation.

use super::codec;
use crate::models::{ZeroMqConfig, ZeroMqFormat, ZeroMqSocketType};
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_trait::async_trait;
use omq_tokio::{Endpoint, Message, Options, Socket, SocketType};
use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{error, trace};

fn parse_endpoint(url: &str) -> anyhow::Result<Endpoint> {
    url.parse::<Endpoint>()
        .map_err(|e| anyhow!("invalid ZeroMQ endpoint {url:?}: {e}"))
}

/// Collect every frame of a received omq `Message` into a plain `Vec<Bytes>` for
/// the shared codec.
///
/// Fails rather than skipping a missing part: `RawFramed` decoding is positional, so a
/// silently dropped frame would shift every frame after it.
fn message_frames(msg: &Message) -> anyhow::Result<Vec<bytes::Bytes>> {
    (0..msg.len())
        .map(|i| {
            msg.part_bytes(i)
                .ok_or_else(|| anyhow!("ZeroMQ message part {i} of {} is missing", msg.len()))
        })
        .collect()
}

/// Build an omq `Message` from codec frames (always at least one).
fn frames_to_message(frames: Vec<bytes::Bytes>) -> Message {
    Message::multipart(frames)
}

// ----------------------------------------------------------------------------
// Publisher (PUSH / PUB / REQ)
// ----------------------------------------------------------------------------

/// A REQ socket plus what is needed to rebuild it.
struct ReqSocket {
    /// `None` after a timed-out exchange, which leaves the socket expecting a reply
    /// that will never be read. The replacement is built on the next request, and
    /// only once the old socket has been dropped — a bound REQ endpoint keeps the
    /// address claimed until then, so building first would fail with "address in use".
    socket: Option<Socket>,
    url: String,
    bind: bool,
}

impl ReqSocket {
    async fn connect(url: &str, bind: bool) -> anyhow::Result<Socket> {
        let socket = Socket::new(SocketType::Req, Options::default());
        let endpoint = parse_endpoint(url)?;
        if bind {
            socket.bind(endpoint).await?;
        } else {
            socket.connect(endpoint).await?;
        }
        Ok(socket)
    }

    /// One request/reply exchange, bounded so an unresponsive REP peer cannot
    /// stall the publisher indefinitely.
    async fn exchange(&mut self, msg: Message, timeout: Duration) -> anyhow::Result<Message> {
        if self.socket.is_none() {
            self.socket = Some(Self::connect(&self.url, self.bind).await?);
        }
        // Scope the borrow so the socket can be dropped in the timeout arm.
        let result = {
            let socket = self.socket.as_ref().expect("REQ socket was just ensured");
            let exchange = async move {
                socket.send(msg).await?;
                socket.recv().await
            };
            tokio::time::timeout(timeout, exchange).await
        };
        match result {
            Ok(Ok(message)) => Ok(message),
            Ok(Err(error)) => {
                self.socket = None;
                Err(anyhow!(error))
            }
            Err(_) => {
                self.socket = None;
                Err(anyhow!("ZeroMQ REQ/REP exchange timed out"))
            }
        }
    }
}

/// How the publisher drives its socket.
enum Sender {
    /// PUSH/PUB: fire-and-forget; omq applies HWM backpressure internally.
    OneWay(Socket),
    /// REQ: strictly alternating send→recv, so concurrent batches are serialised
    /// through the mutex rather than interleaving and desyncing the socket.
    Request(tokio::sync::Mutex<ReqSocket>),
}

pub struct ZeroMqOmqPublisher {
    sender: Sender,
    format: ZeroMqFormat,
    request_timeout: Duration,
}

impl ZeroMqOmqPublisher {
    pub async fn new(config: &ZeroMqConfig) -> anyhow::Result<Self> {
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Push);
        let sender = match socket_type {
            ZeroMqSocketType::Req => Sender::Request(tokio::sync::Mutex::new(ReqSocket {
                socket: Some(ReqSocket::connect(&config.url, config.bind).await?),
                url: config.url.clone(),
                bind: config.bind,
            })),
            other => {
                let omq_type = match other {
                    ZeroMqSocketType::Push => SocketType::Push,
                    ZeroMqSocketType::Pub => SocketType::Pub,
                    other => {
                        return Err(anyhow!(
                            "socket type {other:?} is not supported by a publisher \
                             (use Push, Pub or Req)"
                        ))
                    }
                };
                let socket = Socket::new(omq_type, Options::default());
                let endpoint = parse_endpoint(&config.url)?;
                if config.bind {
                    socket.bind(endpoint).await?;
                } else {
                    socket.connect(endpoint).await?;
                }
                Sender::OneWay(socket)
            }
        };

        Ok(Self {
            sender,
            format: config.format.clone(),
            request_timeout: Duration::from_millis(config.request_timeout_ms.unwrap_or(30_000)),
        })
    }

    fn expects_reply(&self) -> bool {
        matches!(self.sender, Sender::Request(_))
    }

    /// Send one already-framed message, mapping omq errors to a retryable failure.
    async fn send_message(&self, msg: Message) -> Result<(), PublisherError> {
        match &self.sender {
            Sender::OneWay(socket) => socket
                .send(msg)
                .await
                .map_err(|e| PublisherError::Retryable(anyhow!(e))),
            // Only reached if a REQ publisher takes the non-reply path, which it never does.
            Sender::Request(_) => Err(PublisherError::NonRetryable(anyhow!(
                "REQ socket cannot send without awaiting a reply"
            ))),
        }
    }

    /// Send one framed request and decode the reply into canonical messages.
    async fn request(&self, msg: Message) -> Result<Vec<CanonicalMessage>, PublisherError> {
        let Sender::Request(req) = &self.sender else {
            return Err(PublisherError::NonRetryable(anyhow!(
                "socket is not a REQ socket"
            )));
        };
        let reply = req
            .lock()
            .await
            .exchange(msg, self.request_timeout)
            .await
            .map_err(PublisherError::Retryable)?;
        let frames = message_frames(&reply).map_err(PublisherError::NonRetryable)?;
        // The REP side always answers with a JSON array of canonical messages
        // (see the commit path), whatever `format` the request used. Replies are
        // never SUB traffic either, so no topic cursor applies.
        codec::decode_frames(frames, false, &ZeroMqFormat::Json)
            .map_err(PublisherError::NonRetryable)
    }
}

#[async_trait]
impl MessagePublisher for ZeroMqOmqPublisher {
    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(
            count = messages.len(),
            "Publishing batch via omq ZeroMQ backend"
        );

        if matches!(self.format, ZeroMqFormat::Json) {
            // Whole batch is serialized into one JSON frame, mirroring zmq.rs.
            for message in &mut messages {
                message.strip_source_metadata();
            }
            let payload = serde_json::to_vec(&messages)
                .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
            let msg = Message::single(bytes::Bytes::from(payload));
            if self.expects_reply() {
                // One exchange for the whole batch, so one reply covers it.
                let responses = self.request(msg).await?;
                return Ok(SentBatch::Partial {
                    responses: Some(responses),
                    failed: Vec::new(),
                });
            }
            self.send_message(msg).await?;
            return Ok(SentBatch::Ack);
        }

        // Raw / RawFramed: one ZMQ message per canonical message so a single
        // failure only re-sends the offending message.
        let mut failed = Vec::new();
        let mut responses = Vec::new();
        let expects_reply = self.expects_reply();
        for mut message in messages {
            let frames = match codec::encode_frames(&mut message, &self.format) {
                Ok(f) => f,
                Err(e) => {
                    failed.push((message, e));
                    continue;
                }
            };
            let msg = frames_to_message(frames);
            if expects_reply {
                match self.request(msg).await {
                    Ok(decoded) => responses.extend(decoded),
                    Err(e) => failed.push((message, e)),
                }
            } else if let Err(e) = self.send_message(msg).await {
                failed.push((message, e));
            }
        }

        if expects_reply {
            return Ok(SentBatch::Partial {
                responses: Some(responses),
                failed,
            });
        }

        Ok(SentBatch::from_failures(failed))
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: true,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

// ----------------------------------------------------------------------------
// Consumer (PULL / SUB / REP)
// ----------------------------------------------------------------------------

/// Encode the replies for one REP exchange. Matches the zmq.rs wire format: a
/// single frame holding a JSON array of canonical messages.
fn encode_replies(replies: &mut [CanonicalMessage]) -> Message {
    for reply in replies.iter_mut() {
        reply.strip_source_metadata();
    }
    // An empty frame would be a decode error on the REQ side, so fall back to an
    // empty array: the peer still gets a well-formed reply and stops waiting.
    let payload = serde_json::to_vec(replies).unwrap_or_else(|e| {
        error!(error = %e, "Failed to encode ZeroMQ REP reply, answering with an empty array");
        b"[]".to_vec()
    });
    Message::single(bytes::Bytes::from(payload))
}

struct BufferedMessage {
    msg: CanonicalMessage,
    reply: Option<ReplyContext>,
}

/// One message's slot in the reply for the request it arrived in.
struct ReplyContext {
    state: Arc<Mutex<RepReplyState>>,
    index: usize,
}

/// Shared across every message decoded from one REP request: the reply goes out
/// once all of them have committed.
struct RepReplyState {
    socket: Socket,
    responses: Vec<Option<CanonicalMessage>>,
    pending: usize,
    /// Tells `fill_buffer` the reply is on the wire and REP may receive again.
    done: Option<oneshot::Sender<()>>,
}

pub struct ZeroMqOmqConsumer {
    socket: Socket,
    buffer: VecDeque<BufferedMessage>,
    // Only SUB sockets prepend a subscription-topic frame; for PULL/REP a leading
    // frame is payload, not a topic.
    is_sub: bool,
    is_rep: bool,
    format: ZeroMqFormat,
    /// Drain mode: only then does an idle recv return empty (see `drain_gated`).
    exit_on_empty: bool,
    /// REP only: REP alternates strictly, so the next recv waits for the previous
    /// request's reply to be sent.
    reply_gate: Option<oneshot::Receiver<()>>,
}

impl ZeroMqOmqConsumer {
    pub async fn new(config: &ZeroMqConfig) -> anyhow::Result<Self> {
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Pull);
        let is_sub = matches!(socket_type, ZeroMqSocketType::Sub);
        let is_rep = matches!(socket_type, ZeroMqSocketType::Rep);
        let omq_type = match socket_type {
            ZeroMqSocketType::Pull => SocketType::Pull,
            ZeroMqSocketType::Sub => SocketType::Sub,
            ZeroMqSocketType::Rep => SocketType::Rep,
            other => {
                return Err(anyhow!(
                    "socket type {other:?} is not supported by a consumer \
                     (use Pull, Sub or Rep)"
                ))
            }
        };

        let socket = Socket::new(omq_type, Options::default());
        // Subscribe before connecting so early PUB traffic isn't missed.
        if is_sub {
            let topic = config.topic.as_deref().unwrap_or("");
            socket
                .subscribe(bytes::Bytes::from(topic.to_owned()))
                .await?;
        }
        let endpoint = parse_endpoint(&config.url)?;
        if config.bind {
            socket.bind(endpoint).await?;
        } else {
            socket.connect(endpoint).await?;
        }

        Ok(Self {
            socket,
            buffer: VecDeque::new(),
            is_sub,
            is_rep,
            format: config.format.clone(),
            exit_on_empty: false,
            reply_gate: None,
        })
    }

    /// Answer a request nothing will ever commit, so the REQ peer isn't left
    /// blocking and REP is free to receive again.
    async fn send_empty_reply(&self) {
        if let Err(e) = self.socket.send(encode_replies(&mut [])).await {
            error!(error = %e, "Failed to send empty ZeroMQ REP reply");
        }
    }

    async fn fill_buffer(&mut self) -> Result<(), ConsumerError> {
        // REP alternates strictly, so the previous reply must be on the wire before
        // another request is received. A dropped sender means the batch was never
        // committed and nothing replied, so answer here to keep the socket usable.
        //
        // Awaited through `as_mut` and cleared only afterwards: taking it first would
        // lose the gate if `receive_batch` is cancelled here (the route cancels it on
        // shutdown), and the next call would then recv a new request before the previous
        // reply had gone out.
        if let Some(gate) = self.reply_gate.as_mut() {
            let unanswered = gate.await.is_err();
            self.reply_gate = None;
            if unanswered {
                self.send_empty_reply().await;
            }
        }

        // Drain mode: a brief idle timeout returns empty-handed so --drain can fire.
        let Some(res) = crate::traits::drain_gated(self.exit_on_empty, self.socket.recv()).await
        else {
            return Ok(());
        };
        // A closed socket is end-of-stream, not a connection fault, mirroring the
        // closed-channel handling in the zmq.rs backend.
        let msg = res.map_err(|e| match e {
            omq_tokio::Error::Closed => ConsumerError::EndOfStream,
            other => ConsumerError::Connection(anyhow!(other)),
        })?;
        let frames = message_frames(&msg).map_err(|e| ConsumerError::Connection(anyhow!(e)))?;
        let msgs = codec::decode_frames(frames, self.is_sub, &self.format)
            .map_err(|e| ConsumerError::Connection(anyhow!(e)))?;

        if !self.is_rep {
            self.buffer.extend(
                msgs.into_iter()
                    .map(|msg| BufferedMessage { msg, reply: None }),
            );
            return Ok(());
        }

        if msgs.is_empty() {
            // No decoded messages means no commit will ever fire to resolve the reply.
            self.send_empty_reply().await;
            return Ok(());
        }

        let (done_tx, done_rx) = oneshot::channel();
        self.reply_gate = Some(done_rx);
        let count = msgs.len();
        let state = Arc::new(Mutex::new(RepReplyState {
            socket: self.socket.clone(),
            responses: vec![None; count],
            pending: count,
            done: Some(done_tx),
        }));
        for (index, msg) in msgs.into_iter().enumerate() {
            self.buffer.push_back(BufferedMessage {
                msg,
                reply: Some(ReplyContext {
                    state: Arc::clone(&state),
                    index,
                }),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl MessageConsumer for ZeroMqOmqConsumer {
    // ZeroMQ has no broker-side ack, so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch::empty());
        }

        if self.buffer.is_empty() {
            self.fill_buffer().await?;
        }

        let mut messages = Vec::with_capacity(max_messages.min(self.buffer.len()));
        let mut contexts = Vec::with_capacity(max_messages.min(self.buffer.len()));
        while messages.len() < max_messages {
            match self.buffer.pop_front() {
                Some(buffered) => {
                    messages.push(buffered.msg);
                    contexts.push(buffered.reply);
                }
                None => break,
            }
        }

        trace!(
            count = messages.len(),
            "Received batch via omq ZeroMQ backend"
        );
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                if dispositions.len() != contexts.len() {
                    // Bailing drops `contexts`, which drops the reply state and lets
                    // `fill_buffer` answer the REQ peer instead of leaving it blocked.
                    anyhow::bail!(
                        "ZeroMQ REP batch commit length mismatch: dispositions={}, messages={}",
                        dispositions.len(),
                        contexts.len()
                    );
                }
                for (i, ctx) in contexts.into_iter().enumerate() {
                    let Some(ctx) = ctx else { continue };
                    let response = match dispositions.get(i) {
                        Some(MessageDisposition::Reply(r)) => Some(r.clone()),
                        _ => None,
                    };
                    // Take everything needed out of the lock; the socket send below
                    // must not happen while it is held.
                    let ready = {
                        let mut state = ctx.state.lock().unwrap();
                        state.responses[ctx.index] = response;
                        state.pending -= 1;
                        if state.pending == 0 {
                            let replies: Vec<CanonicalMessage> =
                                state.responses.iter().filter_map(|r| r.clone()).collect();
                            let socket = state.socket.clone();
                            state.done.take().map(|done| (socket, replies, done))
                        } else {
                            None
                        }
                    };
                    if let Some((socket, mut replies, done)) = ready {
                        if let Err(e) = socket.send(encode_replies(&mut replies)).await {
                            error!(error = %e, "Failed to send ZeroMQ REP reply");
                        }
                        let _ = done.send(());
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: true,
            pending: Some(self.buffer.len()),
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
    use crate::models::ZeroMqBackend;
    use crate::traits::MessageConsumer;
    use std::sync::atomic::{AtomicU16, Ordering};
    use tokio::time::Duration;

    #[test]
    fn config_selects_backend_and_defaults_to_try_omq() {
        // The YAML/JSON surface: `backend: omq` makes this backend a hard
        // requirement; absent means `try_omq`, which prefers it but falls back.
        let omq: ZeroMqConfig =
            serde_json::from_str(r#"{"url":"tcp://127.0.0.1:5555","backend":"omq"}"#).unwrap();
        assert_eq!(omq.backend, ZeroMqBackend::Omq);

        let zmq: ZeroMqConfig =
            serde_json::from_str(r#"{"url":"tcp://127.0.0.1:5555","backend":"zmq"}"#).unwrap();
        assert_eq!(zmq.backend, ZeroMqBackend::Zmq);

        let default: ZeroMqConfig =
            serde_json::from_str(r#"{"url":"tcp://127.0.0.1:5555"}"#).unwrap();
        assert_eq!(default.backend, ZeroMqBackend::TryOmq);

        // Both spellings of the fallback backend are accepted.
        for spelling in [r#""try_omq""#, r#""try-omq""#] {
            let cfg: ZeroMqConfig = serde_json::from_str(&format!(
                r#"{{"url":"tcp://127.0.0.1:5555","backend":{spelling}}}"#
            ))
            .unwrap();
            assert_eq!(cfg.backend, ZeroMqBackend::TryOmq);
        }
    }

    // Give each test its own port so parallel runs don't collide.
    static NEXT_PORT: AtomicU16 = AtomicU16::new(5620);
    fn next_url() -> String {
        let port = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        format!("tcp://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn omq_push_pull_round_trip() {
        let url = next_url();
        let consumer_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Pull),
            bind: true,
            ..Default::default()
        };
        let publisher_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Push),
            bind: false,
            ..Default::default()
        };

        let mut consumer = ZeroMqOmqConsumer::new(&consumer_config).await.unwrap();
        let publisher = ZeroMqOmqPublisher::new(&publisher_config).await.unwrap();

        let msg = CanonicalMessage::from("hello omq");
        publisher.send(msg).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();
        assert_eq!(received.message.get_payload_str(), "hello omq");
    }

    #[tokio::test]
    async fn omq_pub_sub_round_trip() {
        let url = next_url();
        let consumer_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Sub),
            bind: false,
            ..Default::default()
        };
        let publisher_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Pub),
            bind: true,
            ..Default::default()
        };

        // PUB binds first so the SUB has something to connect to.
        let publisher = ZeroMqOmqPublisher::new(&publisher_config).await.unwrap();
        let mut consumer = ZeroMqOmqConsumer::new(&consumer_config).await.unwrap();

        // Allow the subscription to propagate before publishing (PUB/SUB drops
        // messages sent before the subscription is registered).
        tokio::time::sleep(Duration::from_millis(200)).await;
        publisher
            .send(CanonicalMessage::from("hello sub"))
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for pub/sub message")
            .unwrap();
        assert_eq!(received.message.get_payload_str(), "hello sub");
    }

    /// REQ/REP: the request reaches the consumer and the consumer's reply
    /// disposition travels back to the publisher as the batch response.
    #[tokio::test]
    async fn omq_req_rep_round_trip() {
        let url = next_url();
        let rep_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Rep),
            bind: true,
            ..Default::default()
        };
        let req_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Req),
            bind: false,
            ..Default::default()
        };

        let mut consumer = ZeroMqOmqConsumer::new(&rep_config).await.unwrap();
        let publisher = ZeroMqOmqPublisher::new(&req_config).await.unwrap();

        // The REP side has to be driven concurrently: the publisher blocks on the
        // reply, which only exists once the consumer commits.
        let responder = tokio::spawn(async move {
            let batch = consumer.receive_batch(8).await.unwrap();
            assert_eq!(batch.messages.len(), 1);
            assert_eq!(batch.messages[0].get_payload_str(), "ping");
            (batch.commit)(vec![MessageDisposition::Reply(CanonicalMessage::from(
                "pong",
            ))])
            .await
            .unwrap();
        });

        let sent = tokio::time::timeout(
            Duration::from_secs(5),
            publisher.send_batch(vec![CanonicalMessage::from("ping")]),
        )
        .await
        .expect("Timed out waiting for the REQ/REP exchange")
        .unwrap();

        let SentBatch::Partial { responses, failed } = sent else {
            panic!("REQ publisher should report responses, got {sent:?}");
        };
        assert!(failed.is_empty(), "unexpected failures: {failed:?}");
        let responses = responses.expect("REQ publisher returns a response list");
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].get_payload_str(), "pong");

        responder.await.unwrap();
    }
}
