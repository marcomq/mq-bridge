use crate::models::{ZeroMqEndpoint, ZeroMqSocketType};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, PublisherError, Received,
    ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::anyhow;
use async_channel::{bounded, Receiver, Sender};
use async_trait::async_trait;
use std::any::Any;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use zeromq::{Socket, SocketRecv, SocketSend, ZmqMessage};

enum SenderSocket {
    Push(zeromq::PushSocket),
    Pub(zeromq::PubSocket),
    Req(zeromq::ReqSocket),
}

enum PublisherJob {
    Send(ZmqMessage, oneshot::Sender<zeromq::ZmqResult<()>>),
    Request(ZmqMessage, oneshot::Sender<zeromq::ZmqResult<ZmqMessage>>),
}

pub struct ZeroMqPublisher {
    tx: Sender<PublisherJob>,
    expects_reply: bool,
}

impl ZeroMqPublisher {
    pub async fn new(endpoint: &ZeroMqEndpoint) -> anyhow::Result<Self> {
        let config = &endpoint.config;
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Push);
        let mut socket = match socket_type {
            ZeroMqSocketType::Push => {
                let mut s = zeromq::PushSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                SenderSocket::Push(s)
            }
            ZeroMqSocketType::Pub => {
                let mut s = zeromq::PubSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                SenderSocket::Pub(s)
            }
            ZeroMqSocketType::Req => {
                let mut s = zeromq::ReqSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                SenderSocket::Req(s)
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported socket type for publisher: {:?}",
                    socket_type
                ))
            }
        };

        let buffer_size = config.internal_buffer_size.unwrap_or(128);
        let (tx, rx) = bounded::<PublisherJob>(buffer_size);
        tokio::spawn(async move {
            while let Ok(job) = rx.recv().await {
                match job {
                    PublisherJob::Send(msg, ack_tx) => match &mut socket {
                        SenderSocket::Push(s) => {
                            let _ = ack_tx.send(s.send(msg).await);
                        }
                        SenderSocket::Pub(s) => {
                            let _ = ack_tx.send(s.send(msg).await);
                        }
                        SenderSocket::Req(_) => {
                            tracing::error!("Req socket received Send job, expected Request");
                        }
                    },
                    PublisherJob::Request(msg, reply_tx) => match &mut socket {
                        SenderSocket::Req(s) => {
                            if let Err(e) = s.send(msg).await {
                                let _ = reply_tx.send(Err(e));
                            } else {
                                let res = s.recv().await;
                                let _ = reply_tx.send(res);
                            }
                        }
                        _ => {
                            tracing::error!("Push/Pub socket received Request job, expected Send");
                        }
                    },
                }
            }
        });

        Ok(Self {
            tx,
            expects_reply: matches!(socket_type, ZeroMqSocketType::Req),
        })
    }
}

#[async_trait]
impl MessagePublisher for ZeroMqPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let payload =
            serde_json::to_vec(&messages).map_err(|e| PublisherError::NonRetryable(anyhow!(e)))?;
        let zmq_msg = ZmqMessage::from(bytes::Bytes::from(payload));

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
            let responses = ZeroMqConsumer::decode_batch(response_zmq)
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

    fn as_any(&self) -> &dyn Any {
        self
    }
}

enum ReceiverSocket {
    Pull(zeromq::PullSocket),
    Sub(zeromq::SubSocket),
    Rep(zeromq::RepSocket),
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
}

impl ZeroMqConsumer {
    pub async fn new(endpoint: &ZeroMqEndpoint) -> anyhow::Result<Self> {
        let config = &endpoint.config;
        let socket_type = config.socket_type.clone().unwrap_or(ZeroMqSocketType::Pull);
        let mut socket = match socket_type {
            ZeroMqSocketType::Pull => {
                let mut s = zeromq::PullSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                ReceiverSocket::Pull(s)
            }
            ZeroMqSocketType::Sub => {
                let mut s = zeromq::SubSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                let topic = endpoint.topic.as_deref().unwrap_or("");
                s.subscribe(topic).await?;
                ReceiverSocket::Sub(s)
            }
            ZeroMqSocketType::Rep => {
                let mut s = zeromq::RepSocket::new();
                if config.bind {
                    s.bind(&config.url).await?;
                } else {
                    s.connect(&config.url).await?;
                }
                ReceiverSocket::Rep(s)
            }
            _ => {
                return Err(anyhow!(
                    "Unsupported socket type for consumer: {:?}",
                    socket_type
                ))
            }
        };

        let buffer_size = config.internal_buffer_size.unwrap_or(128);
        let (tx, rx) = bounded::<Result<ConsumerItem, ConsumerError>>(buffer_size);
        tokio::spawn(async move {
            loop {
                let res = match &mut socket {
                    ReceiverSocket::Pull(s) => s.recv().await.map(|msg| ConsumerItem {
                        msg,
                        reply_tx: None,
                    }),
                    ReceiverSocket::Sub(s) => s.recv().await.map(|msg| ConsumerItem {
                        msg,
                        reply_tx: None,
                    }),
                    ReceiverSocket::Rep(s) => {
                        match s.recv().await {
                            Ok(msg) => {
                                let (reply_tx, reply_rx) = oneshot::channel();
                                let item = ConsumerItem {
                                    msg,
                                    reply_tx: Some(reply_tx),
                                };
                                if tx.send(Ok(item)).await.is_err() {
                                    break;
                                }
                                // Wait for the reply from the consumer logic
                                let reply = reply_rx
                                    .await
                                    .unwrap_or_else(|_| ZmqMessage::from(bytes::Bytes::new()));
                                s.send(reply).await.map(|_| ConsumerItem {
                                    msg: ZmqMessage::from(bytes::Bytes::new()),
                                    reply_tx: None,
                                }) // Dummy return to satisfy type, we loop anyway
                            }
                            Err(e) => Err(e),
                        }
                    }
                };

                // For Rep, we already handled the send inside the match. For others, we send here.
                // Actually, let's restructure to avoid the dummy return.
                if let ReceiverSocket::Rep(_) = socket {
                    if let Err(e) = res {
                        let _ = tx.send(Err(ConsumerError::Connection(anyhow!(e)))).await;
                    }
                    continue;
                }

                let item_res = res.map_err(|e| ConsumerError::Connection(anyhow!(e)));
                if tx.send(item_res).await.is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            rx,
            buffer: VecDeque::new(),
        })
    }

    pub(crate) fn decode_batch(zmq_msg: ZmqMessage) -> anyhow::Result<Vec<CanonicalMessage>> {
        let frames = zmq_msg.into_vec();
        let payload = frames.last().cloned().unwrap_or_default();
        if payload.is_empty() {
            return Ok(vec![]);
        }
        if let Ok(messages) = serde_json::from_slice::<Vec<CanonicalMessage>>(&payload) {
            return Ok(messages);
        }
        if let Ok(message) = serde_json::from_slice::<CanonicalMessage>(&payload) {
            return Ok(vec![message]);
        }
        Ok(vec![CanonicalMessage::new(payload.to_vec(), None)])
    }

    async fn fill_buffer(&mut self) -> Result<(), ConsumerError> {
        let item = self
            .rx
            .recv()
            .await
            .map_err(|_| ConsumerError::EndOfStream)??;
        let msgs =
            Self::decode_batch(item.msg).map_err(|e| ConsumerError::Connection(anyhow!(e)))?;

        if let Some(tx) = item.reply_tx {
            let count = msgs.len();
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
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if self.buffer.is_empty() {
            self.fill_buffer().await?;
        }

        let mut messages = Vec::with_capacity(max_messages);
        let mut contexts = Vec::with_capacity(max_messages);

        while messages.len() < max_messages && !self.buffer.is_empty() {
            let buffered = self.buffer.pop_front().unwrap();
            messages.push(buffered.msg);
            contexts.push(buffered.reply_context);
        }

        let commit = Box::new(move |responses: Option<Vec<CanonicalMessage>>| {
            Box::pin(async move {
                let resps = responses.unwrap_or_default();

                for (i, ctx_opt) in contexts.into_iter().enumerate() {
                    if let Some(ctx) = ctx_opt {
                        let resp = resps.get(i).cloned();

                        let mut state = ctx.state.lock().unwrap();
                        state.responses[ctx.index] = resp;
                        state.pending -= 1;

                        if state.pending == 0 {
                            if let Some(tx) = state.tx.take() {
                                let final_resps: Vec<CanonicalMessage> =
                                    state.responses.iter().filter_map(|r| r.clone()).collect();

                                let payload = serde_json::to_vec(&final_resps).unwrap_or_default();
                                let _ = tx.send(ZmqMessage::from(bytes::Bytes::from(payload)));
                            }
                        }
                    }
                }
            }) as BoxFuture<'static, ()>
        });
        Ok(ReceivedBatch { messages, commit })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub struct ZeroMqSubscriber(ZeroMqConsumer);

impl ZeroMqSubscriber {
    pub async fn new(endpoint: &ZeroMqEndpoint) -> anyhow::Result<Self> {
        let mut endpoint = endpoint.clone();
        if endpoint.config.socket_type.is_none() {
            endpoint.config.socket_type = Some(ZeroMqSocketType::Sub);
        }
        Ok(Self(ZeroMqConsumer::new(&endpoint).await?))
    }
}

#[async_trait]
impl MessageConsumer for ZeroMqSubscriber {
    async fn receive(&mut self) -> Result<Received, ConsumerError> {
        self.0.receive().await
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.0.receive_batch(max_messages).await
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

        let consumer_config = ZeroMqEndpoint {
            topic: None,
            config: ZeroMqConfig {
                url: url.clone(),
                socket_type: Some(ZeroMqSocketType::Pull),
                bind: true,
                ..Default::default()
            },
        };

        let publisher_config = ZeroMqEndpoint {
            topic: None,
            config: ZeroMqConfig {
                url: url.clone(),
                socket_type: Some(ZeroMqSocketType::Push),
                bind: false,
                ..Default::default()
            },
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
}
