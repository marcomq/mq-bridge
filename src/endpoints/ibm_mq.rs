//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::models::IbmMqEndpoint;
use anyhow::Context;
use async_trait::async_trait;

use crate::{
    canonical_message::CanonicalMessage,
    outcomes::SentBatch,
    traits::{
        self, ConsumerError, CustomEndpointFactory, MessageConsumer, MessagePublisher,
        PublisherError, ReceivedBatch,
    },
};
use mqi::{
    connection::{Credentials, MqServer, ThreadNone, Tls},
    constants, get, mqstr, open,
    result::ResultCompErrExt,
    types::{
        ApplName, CipherSpec, KeyRepo, MessageFormat, QueueManagerName, QueueName, FORMAT_NONE,
    },
    MqStr, Object, Subscription, Syncpoint,
};
use std::{thread, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

type BatchJob = (
    Vec<CanonicalMessage>,
    oneshot::Sender<Result<SentBatch, PublisherError>>,
);

pub async fn create_ibm_mq_publisher(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqPublisher> {
    info!("Creating IBM MQ publisher for route {}", name);
    Ok(IbmMqPublisher::new(endpoint, name).await?)
}

pub async fn create_ibm_mq_consumer(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqConsumer> {
    info!("Creating IBM MQ consumer for route {}", name);
    Ok(IbmMqConsumer::new(endpoint, name).await?)
}

pub async fn create_ibm_mq_subscriber(
    name: &str,
    endpoint: &IbmMqEndpoint,
) -> anyhow::Result<IbmMqSubscriber> {
    info!("Creating IBM MQ subscriber for route {}", name);
    Ok(IbmMqSubscriber::new(endpoint, name).await?)
}

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
            constants::MQCNO_STANDARD_BINDING,
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
    pub async fn new(endpoint: &IbmMqEndpoint, route_name: &str) -> Result<Self, PublisherError> {
        let (tx, mut rx) = mpsc::channel::<BatchJob>(100);
        let (init_tx, init_rx) = oneshot::channel();
        let route_name = route_name.to_string();
        let endpoint = endpoint.clone();

        thread::spawn(move || {
            let mut init_tx = Some(init_tx);
            loop {
                let qm = match connect_mq!(&endpoint.config) {
                    Ok(q) => q,
                    Err(e) => {
                        if let Some(tx) = init_tx.take() {
                            let _ = tx.send(Err(PublisherError::Retryable(e)));
                            return;
                        }
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };

                let queue = match (|| -> anyhow::Result<_> {
                    let open_options = constants::MQOO_OUTPUT | constants::MQOO_FAIL_IF_QUIESCING;
                    let qm_ref = qm.connection_ref();

                    if let Some(topic) = &endpoint.topic {
                        let topic_str = MqStr::<1024>::try_from(topic.as_str())
                            .context("Invalid topic string")?;
                        let od = open::ObjectString(&topic_str);
                        Object::open(qm_ref, &(od, open_options))
                            .map_err(|e| anyhow::anyhow!("MQ open topic failed: {}", e))
                    } else {
                        let q_name_str = endpoint.queue.as_deref().unwrap_or(&route_name);
                        let q_name =
                            MqStr::<48>::try_from(q_name_str).context("Invalid queue name")?;
                        let od = QueueName(q_name);
                        Object::open(qm_ref, &(od, open_options))
                            .map_err(|e| anyhow::anyhow!("MQ open queue failed: {}", e))
                    }
                })() {
                    Ok(q) => q.discard_warning(),
                    Err(e) => {
                        if let Some(tx) = init_tx.take() {
                            let _ = tx.send(Err(PublisherError::Retryable(e)));
                            return;
                        }
                        thread::sleep(Duration::from_secs(1));
                        continue;
                    }
                };

                if let Some(tx) = init_tx.take() {
                    let _ = tx.send(Ok(()));
                }

                let mut connection_error = false;
                while let Some((messages, reply_tx)) = rx.blocking_recv() {
                    let mut result = Ok(SentBatch::Ack);
                    let syncpoint = Some(Syncpoint::new(&qm));

                    for msg in messages {
                        let pmo = constants::MQPMO_SYNCPOINT | constants::MQPMO_FAIL_IF_QUIESCING;

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
                                let _ = Syncpoint::new(&qm).backout();
                            }
                        }
                    } else if let Some(sp) = syncpoint {
                        let _ = sp.backout();
                    }

                    if result.is_err() {
                        connection_error = true;
                    }
                    let _ = reply_tx.send(result);
                    if connection_error {
                        break;
                    }
                }

                if !connection_error {
                    break;
                }
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

enum ConsumerJob {
    Receive {
        max_messages: usize,
        reply_tx: oneshot::Sender<Result<ReceivedBatch, ConsumerError>>,
    },
    Commit {
        reply_tx: oneshot::Sender<Result<(), ConsumerError>>,
    },
    Backout {
        reply_tx: oneshot::Sender<Result<(), ConsumerError>>,
    },
}

async fn spawn_consumer_thread(
    endpoint: IbmMqEndpoint,
    route_name: String,
) -> Result<mpsc::Sender<ConsumerJob>, ConsumerError> {
    let (tx, mut rx) = mpsc::channel::<ConsumerJob>(100);
    let tx_loop = tx.clone();
    let (init_tx, init_rx) = oneshot::channel();

    thread::spawn(move || {
        let mut init_tx = Some(init_tx);
        loop {
            let qm = match connect_mq!(&endpoint.config) {
                Ok(q) => q,
                Err(e) => {
                    if let Some(tx) = init_tx.take() {
                        let _ = tx.send(Err(ConsumerError::Connection(e)));
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };

            let (_sub, obj) = match (|| -> anyhow::Result<_> {
                let qm_ref = qm.connection_ref();
                // Determine if we are acting as a Subscriber (Topic) or a Consumer (Queue)
                if let Some(topic) = &endpoint.topic {
                    let topic_str =
                        MqStr::<1024>::try_from(topic.as_str()).context("Invalid topic string")?;
                    let sub_opts = (
                        constants::MQSO_CREATE
                            | constants::MQSO_RESUME
                            | constants::MQSO_MANAGED
                            | constants::MQSO_NON_DURABLE,
                        open::ObjectString(&topic_str),
                    );
                    let (sub, obj) = Subscription::subscribe_managed(qm_ref, sub_opts)
                        .map_err(|e| anyhow::anyhow!("MQ subscribe failed: {}", e))?
                        .discard_warning();
                    Ok((Some(sub), obj))
                } else {
                    let q_name_str = endpoint.queue.as_deref().unwrap_or(&route_name);
                    let q_name = MqStr::<48>::try_from(q_name_str).context("Invalid queue name")?;
                    let od = QueueName(q_name);
                    let open_options =
                        constants::MQOO_INPUT_AS_Q_DEF | constants::MQOO_FAIL_IF_QUIESCING;
                    let obj = Object::open(qm_ref, &(od, open_options))
                        .map_err(|e| anyhow::anyhow!("MQ open failed: {}", e))?
                        .discard_warning();
                    Ok((None, obj))
                }
            })() {
                Ok(res) => res,
                Err(e) => {
                    if let Some(tx) = init_tx.take() {
                        let _ = tx.send(Err(ConsumerError::Connection(e)));
                        return;
                    }
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };

            if let Some(tx) = init_tx.take() {
                let _ = tx.send(Ok(()));
            }

            let mut connection_error = false;
            while let Some(job) = rx.blocking_recv() {
                match job {
                    ConsumerJob::Receive {
                        max_messages,
                        reply_tx,
                    } => {
                        let mut messages = Vec::with_capacity(max_messages);
                        let mut error = None;
                        let mut buffer = vec![0u8; endpoint.config.max_message_size];

                        for _ in 0..max_messages {
                            let gmo = (
                                constants::MQGMO_WAIT
                                    | constants::MQGMO_SYNCPOINT
                                    | constants::MQGMO_CONVERT
                                    | constants::MQGMO_FAIL_IF_QUIESCING,
                                get::GetWait::Wait(endpoint.config.wait_timeout_ms),
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

                        if !messages.is_empty() {
                            if error.is_some() {
                                connection_error = true;
                            }
                            let tx_commit = tx_loop.clone();
                            let commit_fn =
                                Box::new(move |failed: Option<Vec<CanonicalMessage>>| {
                                    let tx = tx_commit.clone();
                                    Box::pin(async move {
                                        let (reply_tx, reply_rx) = oneshot::channel();
                                        let should_backout =
                                            failed.is_some_and(|msgs| !msgs.is_empty());
                                        let job = if !should_backout {
                                            ConsumerJob::Commit { reply_tx }
                                        } else {
                                            ConsumerJob::Backout { reply_tx }
                                        };
                                        tx.send(job)
                                            .await
                                            .map_err(|_| anyhow::anyhow!("Consumer thread dead"))?;
                                        reply_rx.await.map_err(|_| {
                                            anyhow::anyhow!("Consumer thread dropped reply")
                                        })??;
                                        Ok(())
                                    })
                                        as traits::BoxFuture<'static, anyhow::Result<()>>
                                });

                            let _ = reply_tx.send(Ok(ReceivedBatch {
                                messages,
                                commit: commit_fn,
                            }));
                        } else if let Some(e) = error {
                            connection_error = true;
                            let _ = reply_tx.send(Err(e));
                        } else {
                            let _ = reply_tx.send(Ok(ReceivedBatch {
                                messages,
                                commit: Box::new(|_| Box::pin(async { Ok(()) })),
                            }));
                        }
                    }
                    ConsumerJob::Commit { reply_tx } => {
                        let sp = Syncpoint::new(&qm);
                        let res = sp.commit().map(|_| ()).map_err(|e| {
                            ConsumerError::Connection(anyhow::anyhow!("Commit failed: {}", e))
                        });
                        if res.is_err() {
                            connection_error = true;
                        }
                        let _ = reply_tx.send(res);
                    }
                    ConsumerJob::Backout { reply_tx } => {
                        let sp = Syncpoint::new(&qm);
                        let res = sp.backout().map(|_| ()).map_err(|e| {
                            ConsumerError::Connection(anyhow::anyhow!("Backout failed: {}", e))
                        });
                        if res.is_err() {
                            connection_error = true;
                        }
                        let _ = reply_tx.send(res);
                    }
                }

                if connection_error {
                    break;
                }
            }

            if !connection_error {
                break;
            }
        }
    });

    match init_rx.await {
        Ok(Ok(())) => Ok(tx),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ConsumerError::Connection(anyhow::anyhow!(
            "MQ init thread panicked"
        ))),
    }
}

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
        self.tx
            .send(ConsumerJob::Receive {
                max_messages,
                reply_tx,
            })
            .await
            .map_err(|_| {
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
    pub async fn new(endpoint: &IbmMqEndpoint, route_name: &str) -> Result<Self, ConsumerError> {
        if endpoint.topic.is_some() {
            warn!("IbmMqConsumer initialized with a 'topic' set. It will behave as a Subscriber. Verify if this is intended.");
        }
        let tx = spawn_consumer_thread(endpoint.clone(), route_name.to_string()).await?;
        Ok(Self { tx })
    }
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
        self.tx
            .send(ConsumerJob::Receive {
                max_messages,
                reply_tx,
            })
            .await
            .map_err(|_| {
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
    pub async fn new(endpoint: &IbmMqEndpoint, route_name: &str) -> Result<Self, ConsumerError> {
        if endpoint.topic.is_none() {
            warn!("IbmMqSubscriber initialized without a 'topic' set. It will behave as a Queue Consumer. Verify if this is intended.");
        }
        let tx = spawn_consumer_thread(endpoint.clone(), route_name.to_string()).await?;
        Ok(Self { tx })
    }
}

#[derive(Debug)]
pub struct IbmMqFactory;

#[async_trait]
impl CustomEndpointFactory for IbmMqFactory {
    async fn create_consumer(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        let endpoint: IbmMqEndpoint = serde_json::from_value(config.clone())?;
        if endpoint.topic.is_some() {
            Ok(Box::new(
                create_ibm_mq_subscriber(route_name, &endpoint).await?,
            ))
        } else {
            Ok(Box::new(
                create_ibm_mq_consumer(route_name, &endpoint).await?,
            ))
        }
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        let endpoint: IbmMqEndpoint = serde_json::from_value(config.clone())?;
        Ok(
            Box::new(create_ibm_mq_publisher(route_name, &endpoint).await?)
                as Box<dyn MessagePublisher>,
        )
    }
}
