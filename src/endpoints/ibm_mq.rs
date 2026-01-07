//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::{
    canonical_message::CanonicalMessage,
    models::{IbmMqConfig, IbmMqEndpoint},
    outcomes::SentBatch,
    traits::{ConsumerError, MessageConsumer, MessagePublisher, PublisherError, ReceivedBatch},
};
use anyhow::Context;
use async_trait::async_trait;
use mqi::{
    connection::{Credentials, MqServer, ThreadNone, Tls},
    constants, get, mqstr, open,
    result::ResultCompErrExt,
    types::{
        ApplName, CipherSpec, KeyRepo, MessageFormat, QueueManagerName, QueueName, FORMAT_NONE,
    },
    MqStr, Object, Subscription, Syncpoint,
};
use std::thread;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

type BatchJob = (
    Vec<CanonicalMessage>,
    oneshot::Sender<Result<SentBatch, PublisherError>>,
);

macro_rules! connect_mq {
    ($config:expr) => {
        (|| -> anyhow::Result<_> {
        let usr = $config.user.as_deref();
        let pwd = $config.password.as_deref();

        let qm_name = MqStr::<48>::try_from($config.queue_manager.as_str())
            .context("Invalid queue manager name")?;

        let cipher_spec_str = $config.cipher_spec.as_deref().unwrap_or("");
        let cipher_spec =
            MqStr::<32>::try_from(cipher_spec_str).context("Invalid cipher spec")?;

        let mq_server_string = format!("{}/TCP/{}", $config.channel, $config.connection_name);
        let mq_server =
            MqServer::try_from(mq_server_string.as_str()).context("Invalid MQSERVER")?;

        let credentials = if let (Some(u), Some(p)) = (usr, pwd) {
            Credentials::User(u, p.into())
        } else {
            Credentials::Default
        };

        let (tls_opt, cipher_opt) = if $config.tls.required {
            let key_repo_str = $config
                .tls
                .cert_file
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("TLS required but cert_file (KeyRepo) not provided"))?;

            let key_repo = KeyRepo(MqStr::<256>::try_from(key_repo_str).context("Invalid Key Repo")?);

            let tls = Tls::new(&key_repo, None, &CipherSpec(cipher_spec));

            if let Some(_pass) = &$config.tls.cert_password {
                warn!("IBM MQ key repository password is not supported in this build (requires mqc_9_3_0_0 feature)");
            }
            (Some(tls), None)
        } else {
            (None, Some(CipherSpec(cipher_spec)))
        };

        let opts = (
            constants::MQCNO_RECONNECT_Q_MGR,
            ApplName(mqstr!("mq-bridge")),
            QueueManagerName(qm_name),
            credentials,
            mq_server,
            tls_opt,
            cipher_opt,
        );

        mqi::connect::<ThreadNone>(&opts)
            .discard_warning()
            .context("MQ connect failed")
        })()
    };
}

pub struct IbmMqPublisher {
    tx: mpsc::Sender<BatchJob>,
}

impl IbmMqPublisher {
    pub async fn new(config: IbmMqConfig, queue_name: String) -> Result<Self, PublisherError> {
        let (tx, mut rx) = mpsc::channel::<BatchJob>(100);
        let (init_tx, init_rx) = oneshot::channel();

        thread::spawn(move || {
            let qm = match connect_mq!(&config) {
                Ok(q) => q,
                Err(e) => {
                    let _ = init_tx.send(Err(PublisherError::Retryable(e)));
                    return;
                }
            };

            let q_name = match MqStr::<48>::try_from(queue_name.as_str()) {
                Ok(n) => n,
                Err(e) => {
                    let _ = init_tx.send(Err(PublisherError::Retryable(anyhow::Error::from(e))));
                    return;
                }
            };

            let od = QueueName(q_name);
            let open_options = constants::MQOO_OUTPUT | constants::MQOO_FAIL_IF_QUIESCING;
            let qm_ref = qm.connection_ref();
            let queue = match Object::open(qm_ref, &(od, open_options)) {
                Ok(q) => q.discard_warning(),
                Err(e) => {
                    let _ = init_tx.send(Err(PublisherError::Retryable(anyhow::anyhow!(
                        "MQ open failed: {}",
                        e
                    ))));
                    return;
                }
            };

            if init_tx.send(Ok(())).is_err() {
                warn!("Failed to send init success signal");
                return;
            }

            while let Some((messages, reply_tx)) = rx.blocking_recv() {
                let mut result = Ok(SentBatch::Ack);
                let use_syncpoint = messages.len() > 1;
                let syncpoint = if use_syncpoint {
                    Some(Syncpoint::new(&qm))
                } else {
                    None
                };

                for msg in messages {
                    let pmo = if syncpoint.is_some() {
                        constants::MQPMO_SYNCPOINT | constants::MQPMO_FAIL_IF_QUIESCING
                    } else {
                        constants::MQPMO_NO_SYNCPOINT | constants::MQPMO_FAIL_IF_QUIESCING
                    };

                    if let Err(e) = queue.put_message(&pmo, &(&msg.payload[..], FORMAT_NONE)) {
                        result = Err(PublisherError::Retryable(anyhow::anyhow!(
                            "MQ put failed: {}",
                            e
                        )));
                        break;
                    };
                }

                if result.is_ok() {
                    if let Some(sp) = syncpoint {
                        if let Err(e) = sp.commit() {
                            result = Err(PublisherError::Retryable(anyhow::anyhow!(
                                "MQ commit failed: {}",
                                e
                            )));
                        }
                    }
                } else if let Some(sp) = syncpoint {
                    let _ = sp.backout();
                }

                let _ = reply_tx.send(result);
            }
        });

        match init_rx.await {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(PublisherError::Retryable(anyhow::anyhow!(
                "MQ init thread panicked or dropped"
            ))),
        }
    }
}

#[async_trait]
impl MessagePublisher for IbmMqPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send((messages, reply_tx)).await.map_err(|_| {
            PublisherError::Retryable(anyhow::anyhow!("MQ publisher thread disconnected"))
        })?;

        reply_rx.await.map_err(|_| {
            PublisherError::Retryable(anyhow::anyhow!("MQ publisher thread dropped reply"))
        })?
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

type ConsumerJob = (usize, oneshot::Sender<Result<ReceivedBatch, ConsumerError>>);

pub struct IbmMqConsumer {
    tx: mpsc::Sender<ConsumerJob>,
}

#[async_trait]
impl MessageConsumer for IbmMqConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<ReceivedBatch, crate::traits::ConsumerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send((max_messages, reply_tx)).await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ consumer thread disconnected"))
        })?;

        reply_rx.await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ consumer thread dropped reply"))
        })?
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl IbmMqConsumer {
    pub async fn new(config: IbmMqConfig, queue_name: String) -> Result<Self, ConsumerError> {
        let (tx, mut rx) = mpsc::channel::<ConsumerJob>(100);
        let (init_tx, init_rx) = oneshot::channel();

        thread::spawn(move || {
            let init = || -> anyhow::Result<_> {
                let qm = connect_mq!(&config)?;
                let q_name =
                    MqStr::<48>::try_from(queue_name.as_str()).context("Invalid queue name")?;
                Ok((qm, q_name))
            };

            let (qm, q_name) = match init() {
                Ok(v) => v,
                Err(e) => {
                    let _ = init_tx.send(Err(ConsumerError::Connection(e)));
                    return;
                }
            };

            let od = QueueName(q_name);
            let open_options = constants::MQOO_INPUT_AS_Q_DEF | constants::MQOO_FAIL_IF_QUIESCING;
            let qm_ref = qm.connection_ref();
            let obj = match Object::open(qm_ref, &(od, open_options)) {
                Ok(o) => o.discard_warning(),
                Err(e) => {
                    let _ = init_tx.send(Err(ConsumerError::Connection(anyhow::anyhow!(
                        "MQ open failed: {}",
                        e
                    ))));
                    return;
                }
            };

            if init_tx.send(Ok(())).is_err() {
                warn!("Failed to send init success signal");
                return;
            }

            while let Some((max_messages, reply_tx)) = rx.blocking_recv() {
                let mut messages = Vec::with_capacity(max_messages);
                let mut error = None;
                let mut buffer = vec![0u8; 1024 * 1024];

                for _ in 0..max_messages {
                    let gmo = (
                        constants::MQGMO_WAIT
                            | constants::MQGMO_NO_SYNCPOINT
                            | constants::MQGMO_CONVERT
                            | constants::MQGMO_FAIL_IF_QUIESCING,
                        get::GetWait::Wait(500),
                    );

                    let res: Result<Option<(_, MessageFormat)>, _> =
                        obj.get_data_with(&gmo, &mut buffer).discard_warning();

                    match res {
                        Ok(opt) => {
                            if let Some((data, _format)) = opt {
                                messages.push(CanonicalMessage::new(data.to_vec(), None));
                            }
                        }

                        Err(e) => {
                            if e.0 == constants::MQCC_FAILED
                                && e.2 == constants::MQRC_NO_MSG_AVAILABLE
                            {
                                break;
                            }

                            error = Some(ConsumerError::Connection(anyhow::anyhow!(
                                "MQ get failed: {}",
                                e
                            )));

                            break;
                        }
                    }
                }

                if let Some(e) = error {
                    let _ = reply_tx.send(Err(e));
                } else {
                    let _ = reply_tx.send(Ok(ReceivedBatch {
                        messages,
                        commit: Box::new(|_| Box::pin(async {})),
                    }));
                }
            }
        });

        match init_rx.await {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ConsumerError::Connection(anyhow::anyhow!(
                "MQ init thread panicked"
            ))),
        }
    }
}

pub async fn create_ibm_mq_publisher(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqPublisher> {
    info!("Creating IBM MQ publisher for route {}", name);
    let queue_name = endpoint.queue.as_deref().unwrap_or(name).to_string();
    Ok(IbmMqPublisher::new(endpoint.config.clone(), queue_name).await?)
}

pub async fn create_ibm_mq_consumer(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqConsumer> {
    info!("Creating IBM MQ consumer for route {}", name);
    let queue_name = endpoint.queue.as_deref().unwrap_or(name).to_string();
    Ok(IbmMqConsumer::new(endpoint.config.clone(), queue_name).await?)
}

pub struct IbmMqSubscriber {
    tx: mpsc::Sender<ConsumerJob>,
}

#[async_trait]
impl MessageConsumer for IbmMqSubscriber {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<ReceivedBatch, crate::traits::ConsumerError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send((max_messages, reply_tx)).await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ subscriber thread disconnected"))
        })?;

        reply_rx.await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ subscriber thread dropped reply"))
        })?
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl IbmMqSubscriber {
    pub async fn new(config: IbmMqConfig, topic_name: String) -> Result<Self, ConsumerError> {
        let (tx, mut rx) = mpsc::channel::<ConsumerJob>(100);
        let (init_tx, init_rx) = oneshot::channel();

        thread::spawn(move || {
            let init = || -> anyhow::Result<_> {
                let qm = connect_mq!(&config)?;
                let topic_str =
                    MqStr::<1024>::try_from(topic_name.as_str()).context("Invalid topic string")?;
                Ok((qm, topic_str))
            };

            let (qm, topic_str) = match init() {
                Ok(v) => v,
                Err(e) => {
                    let _ = init_tx.send(Err(ConsumerError::Connection(e)));
                    return;
                }
            };

            let sub_opts = (
                constants::MQSO_CREATE
                    | constants::MQSO_RESUME
                    | constants::MQSO_MANAGED
                    | constants::MQSO_NON_DURABLE,
                open::ObjectString(&topic_str),
            );

            let qm_ref = qm.connection_ref();
            // We must keep `_sub` alive for the duration of the thread to maintain the subscription.
            let (_sub, obj) = match Subscription::subscribe_managed(qm_ref, sub_opts) {
                Ok(res) => res.discard_warning(),
                Err(e) => {
                    let _ = init_tx.send(Err(ConsumerError::Connection(anyhow::anyhow!(
                        "MQ subscribe failed: {}",
                        e
                    ))));
                    return;
                }
            };

            if init_tx.send(Ok(())).is_err() {
                warn!("Failed to send init success signal");
                return;
            }

            while let Some((max_messages, reply_tx)) = rx.blocking_recv() {
                let mut messages = Vec::with_capacity(max_messages);
                let mut error = None;

                for _ in 0..max_messages {
                    let gmo = (
                        constants::MQGMO_WAIT
                            | constants::MQGMO_NO_SYNCPOINT
                            | constants::MQGMO_CONVERT
                            | constants::MQGMO_FAIL_IF_QUIESCING,
                        get::GetWait::Wait(500),
                    );

                    let mut buffer = vec![0u8; 1024 * 1024];

                    let res = obj
                        .get_data_with::<MessageFormat, _>(&gmo, &mut buffer)
                        .discard_warning();

                    match res {
                        Ok(opt) => {
                            if let Some((data, _format)) = opt {
                                messages.push(CanonicalMessage::new(data.to_vec(), None));
                            }
                        }

                        Err(e) => {
                            if e.0 == constants::MQCC_FAILED
                                && e.2 == constants::MQRC_NO_MSG_AVAILABLE
                            {
                                break;
                            }

                            error = Some(ConsumerError::Connection(anyhow::anyhow!(
                                "MQ get failed: {}",
                                e
                            )));

                            break;
                        }
                    }
                }

                if let Some(e) = error {
                    let _ = reply_tx.send(Err(e));
                } else {
                    let _ = reply_tx.send(Ok(ReceivedBatch {
                        messages,
                        commit: Box::new(|_| Box::pin(async {})),
                    }));
                }
            }
        });

        match init_rx.await {
            Ok(Ok(())) => Ok(Self { tx }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ConsumerError::Connection(anyhow::anyhow!(
                "MQ init thread panicked"
            ))),
        }
    }
}

pub async fn create_ibm_mq_subscriber(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqSubscriber> {
    info!("Creating IBM MQ subscriber for route {}", name);
    let topic_name = endpoint
        .topic
        .as_deref()
        .or(endpoint.queue.as_deref())
        .unwrap_or(name)
        .to_string();
    Ok(IbmMqSubscriber::new(endpoint.config.clone(), topic_name).await?)
}
