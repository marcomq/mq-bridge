use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{ZeroMqConfig, ZeroMqFormat, ZeroMqSocketType};
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use futures::channel::mpsc;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::trace;
use zeromq::{Socket, SocketEvent, SocketOptions, SocketRecv, SocketSend, ZmqMessage};

use super::codec;

/// Pack codec frames (always at least one) into a `ZmqMessage`.
fn frames_to_zmq(frames: Vec<bytes::Bytes>) -> ZmqMessage {
    let mut it = frames.into_iter();
    let first = it.next().unwrap_or_default();
    let mut msg = ZmqMessage::from(first);
    for frame in it {
        msg.push_back(frame);
    }
    msg
}

/// A rebuilt socket waits this long for its peer, so a queued send is not held for long.
const REBUILD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

fn socket_options(rebuild: bool) -> SocketOptions {
    let mut options = SocketOptions::default();
    if rebuild {
        options.connect_timeout(REBUILD_CONNECT_TIMEOUT);
    }
    options
}

async fn attach<S: Socket>(mut socket: S, url: &str, bind: bool) -> zeromq::ZmqResult<S> {
    if bind {
        socket.bind(url).await?;
    } else {
        socket.connect(url).await?;
    }
    Ok(socket)
}

enum SenderSocket {
    Push(zeromq::PushSocket),
    /// With the monitor that reports a lost peer: a PUB send does not fail without one.
    Pub(zeromq::PubSocket, mpsc::Receiver<SocketEvent>),
    Req(zeromq::ReqSocket),
}

impl SenderSocket {
    async fn open(
        socket_type: &ZeroMqSocketType,
        url: &str,
        bind: bool,
        rebuild: bool,
    ) -> anyhow::Result<Self> {
        let options = socket_options(rebuild);
        Ok(match socket_type {
            ZeroMqSocketType::Push => {
                Self::Push(attach(zeromq::PushSocket::with_options(options), url, bind).await?)
            }
            ZeroMqSocketType::Pub => {
                let mut socket = zeromq::PubSocket::with_options(options);
                let monitor = socket.monitor();
                Self::Pub(attach(socket, url, bind).await?, monitor)
            }
            ZeroMqSocketType::Req => {
                Self::Req(attach(zeromq::ReqSocket::with_options(options), url, bind).await?)
            }
            other => {
                return Err(anyhow!(
                    "Unsupported socket type for publisher: {:?}",
                    other
                ))
            }
        })
    }

    /// Whether a peer went away since the last call. Only PUB has to ask.
    fn lost_a_peer(&mut self) -> bool {
        let Self::Pub(_, monitor) = self else {
            return false;
        };
        let mut lost = false;
        while let Ok(event) = monitor.try_recv() {
            lost |= matches!(event, SocketEvent::Disconnected(_));
        }
        lost
    }

    /// Frees a bound address, so that a replacement can bind it.
    async fn close(self) {
        match self {
            Self::Push(mut s) => drop(s.unbind_all().await),
            Self::Pub(mut s, _) => drop(s.unbind_all().await),
            Self::Req(mut s) => drop(s.unbind_all().await),
        }
    }
}

enum PublisherJob {
    Send(ZmqMessage, oneshot::Sender<zeromq::ZmqResult<()>>),
    Request(ZmqMessage, oneshot::Sender<zeromq::ZmqResult<ZmqMessage>>),
}

impl PublisherJob {
    fn fail(self, error: zeromq::ZmqError) {
        match self {
            Self::Send(_, ack_tx) => drop(ack_tx.send(Err(error))),
            Self::Request(_, reply_tx) => drop(reply_tx.send(Err(error))),
        }
    }
}

pub struct ZeroMqPublisher {
    tx: Sender<PublisherJob>,
    expects_reply: bool,
    format: ZeroMqFormat,
    topic: Option<bytes::Bytes>,
}

impl ZeroMqPublisher {
    pub async fn new(config: &ZeroMqConfig) -> anyhow::Result<Self> {
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Push);
        let socket = SenderSocket::open(&socket_type, &config.url, config.bind, false).await?;

        let buffer_size = config.internal_buffer_size.unwrap_or(128).max(1);
        let (tx, rx) = bounded::<PublisherJob>(buffer_size);
        let request_timeout = Duration::from_millis(config.request_timeout_ms.unwrap_or(30_000));
        let url = config.url.clone();
        let bind = config.bind;
        let task_socket_type = socket_type.clone();
        tokio::spawn(async move {
            // `None` while the socket has to be rebuilt: zmq.rs reconnects no PUSH, PUB or REQ.
            let mut socket = Some(socket);
            while let Ok(job) = rx.recv().await {
                if !bind && socket.as_mut().is_some_and(SenderSocket::lost_a_peer) {
                    socket = None;
                }
                let current = match &mut socket {
                    Some(current) => current,
                    None => match SenderSocket::open(&task_socket_type, &url, bind, true).await {
                        Ok(rebuilt) => socket.insert(rebuilt),
                        Err(e) => {
                            tracing::error!(error = %e, "Failed to rebuild the ZeroMQ socket; retrying on the next send");
                            job.fail(zeromq::ZmqError::Other(
                                "ZeroMQ socket reconstruction failed",
                            ));
                            continue;
                        }
                    },
                };
                let broken = match job {
                    PublisherJob::Send(msg, ack_tx) => {
                        let result = match current {
                            SenderSocket::Push(s) => s.send(msg).await,
                            SenderSocket::Pub(s, _) => s.send(msg).await,
                            SenderSocket::Req(_) => Err(zeromq::ZmqError::Other(
                                "Req socket received Send job, expected Request",
                            )),
                        };
                        // A bound socket is found again by its peers; a connecting one is not.
                        let broken = result.is_err() && !bind;
                        let _ = ack_tx.send(result);
                        broken
                    }
                    PublisherJob::Request(msg, reply_tx) => match current {
                        SenderSocket::Req(s) => {
                            // Bound, so an unresponsive REP peer cannot stall every queued job.
                            let exchange = async {
                                s.send(msg).await.map_err(|e| (e, false))?;
                                s.recv().await.map_err(|e| (e, true))
                            };
                            // REQ alternates strictly: once a request is out, a failed or
                            // cancelled exchange leaves the socket waiting for its reply.
                            let (result, broken) =
                                match tokio::time::timeout(request_timeout, exchange).await {
                                    Ok(Ok(reply)) => (Ok(reply), false),
                                    Ok(Err((e, sent))) => (Err(e), sent || !bind),
                                    Err(_) => (
                                        Err(zeromq::ZmqError::Other(
                                            "ZeroMQ REQ/REP exchange timed out",
                                        )),
                                        true,
                                    ),
                                };
                            let _ = reply_tx.send(result);
                            broken
                        }
                        _ => {
                            let _ = reply_tx.send(Err(zeromq::ZmqError::Other(
                                "Push/Pub socket received Request job, expected Send",
                            )));
                            false
                        }
                    },
                };
                if broken {
                    if let Some(old) = socket.take() {
                        old.close().await;
                    }
                    if bind {
                        // At once: peers connect to this address between two sends.
                        socket = SenderSocket::open(&task_socket_type, &url, bind, true)
                            .await
                            .ok();
                    }
                }
            }
        });

        Ok(Self {
            tx,
            expects_reply: matches!(socket_type, ZeroMqSocketType::Req),
            format: config.format.clone(),
            topic: codec::pub_topic(config),
        })
    }

    /// Build the wire frames for one message in `raw`/`raw_framed` mode, then pack
    /// them into a [`ZmqMessage`]. See [`codec::encode_frames`] for the
    /// per-format framing.
    fn frame_message(&self, message: &mut CanonicalMessage) -> Result<ZmqMessage, PublisherError> {
        let frames = codec::encode_frames(message, &self.format)?;
        Ok(frames_to_zmq(codec::with_topic(&self.topic, frames)))
    }

    /// Send each message as its own ZMQ message (one per message, not batched), used
    /// for `raw` and `raw_framed`. See [`frame_message`] for the per-format framing.
    async fn send_batch_raw(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        // Accumulate per-message outcomes so a single Retryable failure only re-sends
        // the offending message, not the whole batch (which would double-deliver the
        // ones that already succeeded).
        let mut failed = Vec::new();
        if self.expects_reply {
            let mut responses = Vec::new();
            for mut message in messages {
                let zmq_msg = match self.frame_message(&mut message) {
                    Ok(m) => m,
                    Err(e) => {
                        failed.push((message, e));
                        continue;
                    }
                };
                let (reply_tx, reply_rx) = oneshot::channel();
                if self
                    .tx
                    .send(PublisherJob::Request(zmq_msg, reply_tx))
                    .await
                    .is_err()
                {
                    failed.push((
                        message,
                        PublisherError::Retryable(anyhow!("ZeroMQ publisher task closed")),
                    ));
                    continue;
                }
                let response_zmq = match reply_rx.await {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => {
                        failed.push((message, PublisherError::Retryable(anyhow!(e))));
                        continue;
                    }
                    Err(_) => {
                        failed.push((
                            message,
                            PublisherError::Retryable(anyhow!("ZeroMQ reply channel closed")),
                        ));
                        continue;
                    }
                };
                // The REP side always answers with a JSON array of canonical messages
                // (see the commit path), whatever `format` the request used. Replies are
                // never SUB traffic either, so no topic cursor applies.
                match ZeroMqConsumer::decode_batch(response_zmq, false, &ZeroMqFormat::Json) {
                    Ok(decoded) => responses.extend(decoded),
                    Err(e) => failed.push((message, PublisherError::NonRetryable(anyhow!(e)))),
                }
            }
            Ok(SentBatch::Partial {
                responses: Some(responses),
                failed,
            })
        } else {
            for mut message in messages {
                let zmq_msg = match self.frame_message(&mut message) {
                    Ok(m) => m,
                    Err(e) => {
                        failed.push((message, e));
                        continue;
                    }
                };
                let (ack_tx, ack_rx) = oneshot::channel();
                if self
                    .tx
                    .send(PublisherJob::Send(zmq_msg, ack_tx))
                    .await
                    .is_err()
                {
                    failed.push((
                        message,
                        PublisherError::Retryable(anyhow!("ZeroMQ publisher task closed")),
                    ));
                    continue;
                }
                match ack_rx.await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => failed.push((message, PublisherError::Retryable(anyhow!(e)))),
                    Err(_) => failed.push((
                        message,
                        PublisherError::Retryable(anyhow!(
                            "ZeroMQ publisher task dropped ack channel"
                        )),
                    )),
                }
            }
            Ok(SentBatch::from_failures(failed))
        }
    }
}

#[async_trait]
impl MessagePublisher for ZeroMqPublisher {
    async fn send_batch(
        &self,
        mut messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch of ZeroMQ messages");
        if !matches!(self.format, ZeroMqFormat::Json) {
            return self.send_batch_raw(messages).await;
        }
        // Source/provenance keys are per-hop context and must not be forwarded.
        for message in &mut messages {
            message.strip_source_metadata();
        }
        let payload =
            serde_json::to_vec(&messages).map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
        let frames = codec::with_topic(&self.topic, vec![bytes::Bytes::from(payload)]);
        let zmq_msg = frames_to_zmq(frames);

        if self.expects_reply {
            let (reply_tx, reply_rx) = oneshot::channel();
            self.tx
                .send(PublisherJob::Request(zmq_msg, reply_tx))
                .await
                .map_err(|_| PublisherError::Retryable(anyhow!("ZeroMQ publisher task closed")))?;
            let response_zmq = reply_rx
                .await
                .map_err(|_| PublisherError::Retryable(anyhow!("ZeroMQ reply channel closed")))?
                .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
            // REQ/REP replies are never SUB traffic, so no topic cursor applies.
            let responses = ZeroMqConsumer::decode_batch(response_zmq, false, &ZeroMqFormat::Json)
                .map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
            Ok(SentBatch::Partial {
                responses: Some(responses),
                failed: vec![],
            })
        } else {
            let (ack_tx, ack_rx) = oneshot::channel();
            self.tx
                .send(PublisherJob::Send(zmq_msg, ack_tx))
                .await
                .map_err(|_| PublisherError::Retryable(anyhow!("ZeroMQ publisher task closed")))?;
            ack_rx
                .await
                .map_err(|_| {
                    PublisherError::Retryable(anyhow!("ZeroMQ publisher task dropped ack channel"))
                })?
                .map_err(|e| PublisherError::Retryable(anyhow!(e)))?;
            Ok(SentBatch::Ack)
        }
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: !self.tx.is_closed(),
            pending: Some(self.tx.len()),
            capacity: self.tx.capacity(),
            error: if self.tx.is_closed() {
                Some("Publisher task terminated".to_string())
            } else {
                None
            },
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The `zeromq` crate reconnects SUB sockets only.
fn stays_silent_after_peer_restart(socket_type: &ZeroMqSocketType, bind: bool) -> bool {
    !bind && matches!(socket_type, ZeroMqSocketType::Pull | ZeroMqSocketType::Rep)
}

enum ReceiverSocket {
    Pull(zeromq::PullSocket),
    Sub(zeromq::SubSocket),
    Rep(zeromq::RepSocket),
}

impl ReceiverSocket {
    async fn open(socket_type: &ZeroMqSocketType, config: &ZeroMqConfig) -> anyhow::Result<Self> {
        let (url, bind) = (config.url.as_str(), config.bind);
        if stays_silent_after_peer_restart(socket_type, bind) {
            tracing::warn!(
                url,
                "a connecting {socket_type:?} socket on the `zmq` backend does not reconnect: after its peer restarts it receives nothing until the route is restarted. Bind on this side or use `backend: omq`"
            );
        }
        Ok(match socket_type {
            ZeroMqSocketType::Pull => {
                Self::Pull(attach(zeromq::PullSocket::new(), url, bind).await?)
            }
            ZeroMqSocketType::Sub => {
                let mut socket = attach(zeromq::SubSocket::new(), url, bind).await?;
                socket
                    .subscribe(config.topic.as_deref().unwrap_or(""))
                    .await?;
                Self::Sub(socket)
            }
            ZeroMqSocketType::Rep => Self::Rep(attach(zeromq::RepSocket::new(), url, bind).await?),
            other => return Err(anyhow!("Unsupported socket type for consumer: {:?}", other)),
        })
    }

    async fn recv(&mut self) -> zeromq::ZmqResult<ZmqMessage> {
        match self {
            Self::Pull(s) => s.recv().await,
            Self::Sub(s) => s.recv().await,
            Self::Rep(s) => s.recv().await,
        }
    }

    /// Frees a bound address at once; a new consumer may want it.
    async fn close(self) {
        match self {
            Self::Pull(mut s) => drop(s.unbind_all().await),
            Self::Sub(mut s) => drop(s.unbind_all().await),
            Self::Rep(mut s) => drop(s.unbind_all().await),
        }
    }
}

#[derive(Debug)]
struct ConsumerItem {
    msg: ZmqMessage,
    reply_tx: Option<oneshot::Sender<ZmqMessage>>,
}

struct BufferedMessage {
    msg: CanonicalMessage,
    reply_context: Option<ReplyContext>,
}

#[derive(Clone)]
struct ReplyContext {
    state: Arc<Mutex<BatchReplyState>>,
    index: usize,
}

struct BatchReplyState {
    tx: Option<oneshot::Sender<ZmqMessage>>,
    responses: Vec<Option<CanonicalMessage>>,
    pending: usize,
}

pub struct ZeroMqConsumer {
    rx: Receiver<Result<ConsumerItem, ConsumerError>>,
    buffer: VecDeque<BufferedMessage>,
    // Only SUB sockets prepend a subscription-topic frame; for PULL/REP a leading
    // frame is payload, not a topic, so the cursor must not be attached there.
    is_sub: bool,
    format: ZeroMqFormat,
    /// Drain mode: only then does an idle recv return without filling (empty batch).
    exit_on_empty: bool,
}

impl ZeroMqConsumer {
    pub async fn new(config: &ZeroMqConfig) -> anyhow::Result<Self> {
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Pull);
        let is_sub = matches!(socket_type, ZeroMqSocketType::Sub);
        let format = config.format.clone();
        let mut socket = ReceiverSocket::open(&socket_type, config).await?;

        let buffer_size = config.internal_buffer_size.unwrap_or(128).max(1);
        let (tx, rx) = bounded::<Result<ConsumerItem, ConsumerError>>(buffer_size);
        tokio::spawn(async move {
            loop {
                // A dropped consumer ends the read: the socket would keep its address and
                // take one more message otherwise.
                let received = tokio::select! {
                    biased;
                    _ = tx.closed() => break,
                    received = socket.recv() => received,
                };
                let msg = match received {
                    Ok(msg) => msg,
                    Err(e) => {
                        if tx
                            .send(Err(ConsumerError::Connection(anyhow!(e))))
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let ReceiverSocket::Rep(rep) = &mut socket else {
                    let item = ConsumerItem {
                        msg,
                        reply_tx: None,
                    };
                    if tx.send(Ok(item)).await.is_err() {
                        break;
                    }
                    continue;
                };
                // REP: the reply to this request goes out before the next one is read.
                let (reply_tx, reply_rx) = oneshot::channel();
                let item = ConsumerItem {
                    msg,
                    reply_tx: Some(reply_tx),
                };
                if tx.send(Ok(item)).await.is_err() {
                    break;
                }
                // A request nobody settled is answered with an empty array, so the REQ
                // peer stops waiting and gets no made-up response.
                let reply = reply_rx
                    .await
                    .unwrap_or_else(|_| ZmqMessage::from(bytes::Bytes::from_static(b"[]")));
                // The failure belongs to the request delivered above, so it is logged
                // here and not handed to a later read.
                if let Err(e) = rep.send(reply).await {
                    tracing::error!("Failed to send ZeroMQ REP reply: {}", e);
                }
            }
            socket.close().await;
        });

        Ok(Self {
            rx,
            buffer: VecDeque::new(),
            is_sub,
            format,
            exit_on_empty: false,
        })
    }

    pub(crate) fn decode_batch(
        zmq_msg: ZmqMessage,
        is_sub: bool,
        format: &ZeroMqFormat,
    ) -> anyhow::Result<Vec<CanonicalMessage>> {
        codec::decode_frames(zmq_msg.into_vec(), is_sub, format)
    }

    async fn fill_buffer(&mut self) -> Result<(), ConsumerError> {
        // Drain mode: a brief idle timeout returns empty-handed so --drain can fire.
        let Some(item) = crate::traits::drain_gated(self.exit_on_empty, self.rx.recv()).await
        else {
            return Ok(());
        };
        let item = item.map_err(|_| ConsumerError::EndOfStream)??;
        let msgs = Self::decode_batch(item.msg, self.is_sub, &self.format)
            .map_err(|e| ConsumerError::Connection(anyhow!(e)))?;

        if let Some(tx) = item.reply_tx {
            let count = msgs.len();
            if count == 0 {
                // No decoded messages means no per-message commit will ever fire to
                // resolve the reply. Answer the pending REP request immediately (with an
                // empty JSON array, matching the commit path's wire format) so the REQ
                // peer isn't left blocking forever.
                let payload =
                    serde_json::to_vec(&Vec::<CanonicalMessage>::new()).unwrap_or_default();
                let _ = tx.send(ZmqMessage::from(bytes::Bytes::from(payload)));
                return Ok(());
            }
            let state = Arc::new(Mutex::new(BatchReplyState {
                tx: Some(tx),
                responses: vec![None; count],
                pending: count,
            }));

            for (i, msg) in msgs.into_iter().enumerate() {
                self.buffer.push_back(BufferedMessage {
                    msg,
                    reply_context: Some(ReplyContext {
                        state: state.clone(),
                        index: i,
                    }),
                });
            }
        } else {
            for msg in msgs {
                self.buffer.push_back(BufferedMessage {
                    msg,
                    reply_context: None,
                });
            }
        }
        Ok(())
    }
}
#[async_trait]
impl MessageConsumer for ZeroMqConsumer {
    // ZeroMQ has no broker-side ack (commit only routes per-message REQ/REP
    // replies or is a no-op), so commits are order-independent.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        if self.buffer.is_empty() {
            self.fill_buffer().await?;
        }

        let mut messages = Vec::with_capacity(max_messages);
        let mut contexts = Vec::with_capacity(max_messages);

        while messages.len() < max_messages {
            if let Some(buffered) = self.buffer.pop_front() {
                messages.push(buffered.msg);
                contexts.push(buffered.reply_context);
            } else {
                break;
            }
        }

        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Received batch of ZeroMQ messages");
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                for (i, ctx_opt) in contexts.into_iter().enumerate() {
                    if let Some(ctx) = ctx_opt {
                        let resp = match dispositions.get(i) {
                            Some(MessageDisposition::Reply(r)) => Some(r.clone()),
                            _ => None,
                        };

                        let mut state = ctx.state.lock().unwrap();
                        state.responses[ctx.index] = resp;
                        state.pending -= 1;

                        if state.pending == 0 {
                            if let Some(tx) = state.tx.take() {
                                let mut final_resps: Vec<CanonicalMessage> =
                                    state.responses.iter().filter_map(|r| r.clone()).collect();
                                for resp in &mut final_resps {
                                    resp.strip_source_metadata();
                                }

                                let payload = serde_json::to_vec(&final_resps).unwrap_or_default();
                                let _ = tx.send(ZmqMessage::from(bytes::Bytes::from(payload)));
                            }
                        }
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });
        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: !self.rx.is_closed(),
            pending: Some(self.rx.len() + self.buffer.len()),
            capacity: self.rx.capacity(),
            error: if self.rx.is_closed() {
                Some("Consumer task terminated".to_string())
            } else {
                None
            },
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
    use crate::models::ZeroMqConfig;
    use crate::CanonicalMessage;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_zeromq_push_pull() {
        let port = 5556;
        let url = format!("tcp://127.0.0.1:{}", port);

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

        let mut consumer = ZeroMqConsumer::new(&consumer_config).await.unwrap();
        let publisher = ZeroMqPublisher::new(&publisher_config).await.unwrap();

        let msg = CanonicalMessage::from("hello zeromq");
        publisher.send(msg).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();
        assert_eq!(received.message.get_payload_str(), "hello zeromq");
    }

    #[test]
    fn only_a_connecting_pull_or_rep_is_warned_about() {
        use ZeroMqSocketType::{Pull, Rep, Sub};
        assert!(stays_silent_after_peer_restart(&Pull, false));
        assert!(stays_silent_after_peer_restart(&Rep, false));
        assert!(!stays_silent_after_peer_restart(&Pull, true));
        assert!(!stays_silent_after_peer_restart(&Sub, false));
    }

    #[test]
    fn decode_batch_strips_spoofed_source_metadata() {
        // A wire payload carries a reserved `mqb.src.*` key. decode_batch must drop
        // it so it can't masquerade as a framework-injected cursor downstream.
        let mut msg = CanonicalMessage::from_vec("body");
        msg.metadata
            .insert("mqb.src.kafka_offset".to_string(), "999".to_string());
        msg.metadata
            .insert("user_key".to_string(), "kept".to_string());
        let payload = serde_json::to_vec(&vec![msg]).unwrap();
        let zmq_msg = ZmqMessage::from(bytes::Bytes::from(payload));

        let decoded = ZeroMqConsumer::decode_batch(zmq_msg, false, &ZeroMqFormat::Json).unwrap();
        assert_eq!(decoded.len(), 1);
        assert!(!decoded[0].metadata.contains_key("mqb.src.kafka_offset"));
        assert_eq!(
            decoded[0].metadata.get("user_key").map(String::as_str),
            Some("kept")
        );
    }

    #[test]
    fn decode_batch_raw_returns_opaque_payload() {
        // Raw mode must hand back the frame bytes verbatim, even when they happen to
        // be valid JSON, without decoding into a CanonicalMessage.
        let json_looking = br#"[{"message_id":"x","payload":"abc"}]"#;
        let zmq_msg = ZmqMessage::from(bytes::Bytes::from(json_looking.to_vec()));

        let decoded = ZeroMqConsumer::decode_batch(zmq_msg, false, &ZeroMqFormat::Raw).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].payload.as_ref(), json_looking);
        assert!(decoded[0].metadata.is_empty());
    }

    #[test]
    fn decode_batch_raw_multipart_yields_one_message_per_frame() {
        // A raw producer may send a multipart message; each payload frame must become
        // its own message rather than only the last frame surfacing.
        let mut zmq_msg = ZmqMessage::from(bytes::Bytes::from_static(b"frame0"));
        zmq_msg.push_back(bytes::Bytes::from_static(b"frame1"));
        zmq_msg.push_back(bytes::Bytes::from_static(b"frame2"));

        let decoded = ZeroMqConsumer::decode_batch(zmq_msg, false, &ZeroMqFormat::Raw).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].payload.as_ref(), b"frame0");
        assert_eq!(decoded[1].payload.as_ref(), b"frame1");
        assert_eq!(decoded[2].payload.as_ref(), b"frame2");
    }

    #[test]
    fn decode_batch_raw_sub_strips_topic_frame() {
        // For SUB sockets the leading frame is the subscription topic and must be
        // dropped; the remaining frames are payload.
        let mut zmq_msg = ZmqMessage::from(bytes::Bytes::from_static(b"my_topic"));
        zmq_msg.push_back(bytes::Bytes::from_static(b"image_bytes"));

        let decoded = ZeroMqConsumer::decode_batch(zmq_msg, true, &ZeroMqFormat::Raw).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].payload.as_ref(), b"image_bytes");
    }

    #[test]
    fn decode_batch_raw_framed_parses_metadata_frame() {
        // raw_framed: frame[0] is JSON metadata, frame[1] is the opaque payload.
        let meta = serde_json::json!({"kind": "jpeg", "trace_id": "abc"});
        let mut zmq_msg = ZmqMessage::from(bytes::Bytes::from(serde_json::to_vec(&meta).unwrap()));
        zmq_msg.push_back(bytes::Bytes::from_static(&[0xFF, 0xD8, 0xFF]));

        let decoded =
            ZeroMqConsumer::decode_batch(zmq_msg, false, &ZeroMqFormat::RawFramed).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].payload.as_ref(), &[0xFF, 0xD8, 0xFF]);
        assert_eq!(
            decoded[0].metadata.get("kind").map(String::as_str),
            Some("jpeg")
        );
        assert_eq!(
            decoded[0].metadata.get("trace_id").map(String::as_str),
            Some("abc")
        );
    }

    #[test]
    fn decode_batch_raw_framed_single_frame_degrades_to_payload() {
        // A single-frame message in raw_framed carries no metadata; treat it as payload.
        let zmq_msg = ZmqMessage::from(bytes::Bytes::from_static(b"just_payload"));
        let decoded =
            ZeroMqConsumer::decode_batch(zmq_msg, false, &ZeroMqFormat::RawFramed).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].payload.as_ref(), b"just_payload");
        assert!(decoded[0].metadata.is_empty());
    }

    #[tokio::test]
    async fn test_zeromq_push_pull_raw_framed() {
        let url = "tcp://127.0.0.1:5558".to_string();

        let consumer_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Pull),
            bind: true,
            format: ZeroMqFormat::RawFramed,
            ..Default::default()
        };
        let publisher_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Push),
            bind: false,
            format: ZeroMqFormat::RawFramed,
            ..Default::default()
        };

        let mut consumer = ZeroMqConsumer::new(&consumer_config).await.unwrap();
        let publisher = ZeroMqPublisher::new(&publisher_config).await.unwrap();

        let raw_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
        let mut msg = CanonicalMessage::new(raw_bytes.clone(), None);
        msg.metadata.insert("kind".to_string(), "jpeg".to_string());
        publisher.send(msg).await.unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();
        assert_eq!(received.message.payload.as_ref(), raw_bytes.as_slice());
        assert_eq!(
            received.message.metadata.get("kind").map(String::as_str),
            Some("jpeg")
        );
    }

    #[tokio::test]
    async fn test_zeromq_push_pull_raw() {
        let url = "tcp://127.0.0.1:5557".to_string();

        let consumer_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Pull),
            bind: true,
            format: crate::models::ZeroMqFormat::Raw,
            ..Default::default()
        };
        let publisher_config = ZeroMqConfig {
            url: url.clone(),
            socket_type: Some(ZeroMqSocketType::Push),
            bind: false,
            format: crate::models::ZeroMqFormat::Raw,
            ..Default::default()
        };

        let mut consumer = ZeroMqConsumer::new(&consumer_config).await.unwrap();
        let publisher = ZeroMqPublisher::new(&publisher_config).await.unwrap();

        // Arbitrary non-JSON binary payload, e.g. a JPEG magic header.
        let raw_bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        publisher
            .send(CanonicalMessage::new(raw_bytes.clone(), None))
            .await
            .unwrap();

        let received = tokio::time::timeout(Duration::from_secs(2), consumer.receive())
            .await
            .expect("Timed out waiting for message")
            .unwrap();
        assert_eq!(received.message.payload.as_ref(), raw_bytes.as_slice());
    }
}
