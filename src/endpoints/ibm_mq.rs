//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::IbmMqConfig;
use anyhow::Context;
use async_trait::async_trait;

use crate::{
    canonical_message::CanonicalMessage,
    outcomes::SentBatch,
    traits::{
        self, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition, MessagePublisher,
        PublisherError, ReceivedBatch,
    },
};
use mqi::attribute::{InqResItem, SetItems, MQIA_CURRENT_Q_DEPTH, MQIA_MAX_Q_DEPTH};
use mqi::{
    connection::{Credentials, MqServer, ThreadNone, Tls},
    constants, get, mqstr, open,
    result::ResultCompErrExt,
    types::{
        ApplName, CipherSpec, KeyRepo, MessageFormat, QueueManagerName, QueueName, FORMAT_NONE,
    },
    MqStr, Object, Subscription, Syncpoint,
};
use std::{sync::Arc, thread, time::Duration};
use tokio::sync::{mpsc, oneshot, Semaphore};
use tracing::{debug, info, trace, warn};

macro_rules! connect_mq {
    ($config:expr) => {
        (|| -> anyhow::Result<_> {
        let usr = $config.username.as_deref();
        let pwd = $config.password.as_deref();

        let qm_name = MqStr::<48>::try_from($config.queue_manager.as_str())
            .context("Invalid queue manager name")?;

        let cipher_spec_str = $config.cipher_spec.as_deref().unwrap_or("");
        let cipher_spec =
            MqStr::<32>::try_from(cipher_spec_str).context("Invalid cipher spec")?;

        let mq_server_string = format!("{}/TCP/{}", $config.channel, $config.url);
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

enum PublisherJob {
    SendBatch(
        Vec<CanonicalMessage>,
        oneshot::Sender<Result<SentBatch, PublisherError>>,
    ),
    Status(oneshot::Sender<EndpointStatus>),
}

pub struct IbmMqPublisher {
    tx: mpsc::Sender<PublisherJob>,
}

impl IbmMqPublisher {
    pub async fn new(config: &IbmMqConfig) -> Result<Self, PublisherError> {
        let buffer_size = config.internal_buffer_size.unwrap_or(100).max(1);
        let (tx, mut rx) = mpsc::channel::<PublisherJob>(buffer_size);
        let (init_tx, init_rx) = oneshot::channel();
        let config = config.clone();
        info!("Starting IBM MQ publisher");

        thread::spawn(move || {
            let mut init_tx = Some(init_tx);
            loop {
                let qm = match connect_mq!(&config) {
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
                    let mut open_options =
                        constants::MQOO_OUTPUT | constants::MQOO_FAIL_IF_QUIESCING;
                    if !config.disable_status_inq {
                        open_options |= constants::MQOO_INQUIRE;
                    }
                    let qm_ref = qm.connection_ref();

                    if let Some(topic) = &config.topic {
                        let topic_str = MqStr::<1024>::try_from(topic.as_str())
                            .context("Invalid topic string")?;
                        let od = open::ObjectString(&topic_str);
                        Object::open(qm_ref, &(od, open_options))
                            .map_err(|e| anyhow::anyhow!("MQ open topic failed: {}", e))
                    } else {
                        let q_name_str = config.queue.as_deref().ok_or_else(|| {
                            anyhow::anyhow!("Queue name is required for IBM MQ publisher")
                        })?;
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
                while let Some(job) = rx.blocking_recv() {
                    match job {
                        PublisherJob::SendBatch(messages, reply_tx) => {
                            let mut result = Ok(SentBatch::Ack);
                            let syncpoint = Some(Syncpoint::new(&qm));

                            for msg in messages {
                                let pmo =
                                    constants::MQPMO_SYNCPOINT | constants::MQPMO_FAIL_IF_QUIESCING;

                                if let Err(e) =
                                    queue.put_message(&pmo, &(&msg.payload[..], FORMAT_NONE))
                                {
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
                                        match Syncpoint::new(&qm).backout() {
                                            Ok(_) => debug!("Backout on reconnect succeeded"),
                                            Err(e) => warn!("Backout on reconnect FAILED (messages may be lost): {}", e),
                                        }
                                    }
                                }
                            } else if let Some(sp) = syncpoint {
                                let _ = sp.backout();
                            }

                            if result.is_err() {
                                connection_error = true;
                            }
                            let _ = reply_tx.send(result);
                        }
                        PublisherJob::Status(reply_tx) => {
                            let mut healthy = true;
                            let mut error = None;
                            if let Err(e) = queue.inquire(&[mqi::attribute::MQIA_DEF_PRIORITY]) {
                                if e.2 != constants::MQRC_NOT_OPEN_FOR_INQUIRE
                                    && e.2 != constants::MQRC_NOT_AUTHORIZED
                                {
                                    healthy = false;
                                    error = Some(format!("Failed to inquire object status: {}", e));
                                }
                            }

                            let _ = reply_tx.send(EndpointStatus {
                                healthy,
                                target: config
                                    .queue
                                    .clone()
                                    .or(config.topic.clone())
                                    .unwrap_or_default(),
                                pending: None,
                                error,
                                capacity: Some(config.internal_buffer_size.unwrap_or(100).max(1)),
                                ..Default::default()
                            });
                        }
                    }
                    if connection_error {
                        break;
                    }
                }
                while let Ok(job) = rx.try_recv() {
                    match job {
                        PublisherJob::SendBatch(_, reply_tx) => {
                            let _ = reply_tx.send(Err(PublisherError::Retryable(anyhow::anyhow!(
                                "MQ publisher reconnecting, batch rejected"
                            ))));
                        }
                        PublisherJob::Status(reply_tx) => {
                            let _ = reply_tx.send(EndpointStatus {
                                healthy: false,
                                error: Some("Publisher reconnecting".to_string()),
                                ..Default::default()
                            });
                        }
                    }
                }
                if !connection_error {
                    // This means the loop exited because the rx channel was closed.
                    // This happens when the IbmMqPublisher struct is dropped.
                    // We should backout any transaction that might be open.
                    info!("IBM MQ publisher channel closed, backing out any active transaction before exiting thread.");
                    match Syncpoint::new(&qm).backout() {
                        Ok(_) => debug!("Backout on reconnect succeeded"),
                        Err(e) => {
                            warn!("Backout on reconnect FAILED (messages may be lost): {}", e)
                        }
                    }
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
        trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Publishing batch of IBM MQ messages");
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(PublisherJob::SendBatch(messages, reply_tx))
            .await
            .map_err(|_| {
                PublisherError::Retryable(anyhow::anyhow!("MQ publisher thread disconnected"))
            })?;

        reply_rx.await.map_err(|_| {
            PublisherError::Retryable(anyhow::anyhow!("MQ publisher thread dropped reply"))
        })?
    }

    async fn status(&self) -> EndpointStatus {
        let (reply_tx, reply_rx) = oneshot::channel();
        let send_result = tokio::time::timeout(
            Duration::from_secs(1),
            self.tx.send(PublisherJob::Status(reply_tx)),
        )
        .await;

        match send_result {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                return EndpointStatus {
                    healthy: false,
                    error: Some("Publisher thread disconnected".to_string()),
                    ..Default::default()
                };
            }
            Err(_) => {
                return EndpointStatus {
                    healthy: false,
                    error: Some("Status send timed out".to_string()),
                    ..Default::default()
                };
            }
        }

        match tokio::time::timeout(Duration::from_secs(1), reply_rx).await {
            Ok(Ok(status)) => status,
            Ok(Err(_)) => EndpointStatus {
                healthy: false,
                error: Some("Publisher thread dropped status request".to_string()),
                ..Default::default()
            },
            Err(_) => EndpointStatus {
                healthy: false,
                error: Some("Status check timed out".to_string()),
                ..Default::default()
            },
        }
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
        epoch: u64,
        reply_tx: oneshot::Sender<Result<(), ConsumerError>>,
    },
    Backout {
        epoch: u64,
        reply_tx: oneshot::Sender<Result<(), ConsumerError>>,
    },
    Status {
        reply_tx: oneshot::Sender<EndpointStatus>,
    },
}

async fn spawn_consumer_thread(
    config: IbmMqConfig,
) -> Result<mpsc::Sender<ConsumerJob>, ConsumerError> {
    info!("Starting IBM MQ consumer thread");
    let buffer_size = config.internal_buffer_size.unwrap_or(100).max(1);
    let (tx, mut rx) = mpsc::channel::<ConsumerJob>(buffer_size);
    let tx_loop = tx.clone();
    let mut current_epoch = 0u64;
    let (init_tx, init_rx) = oneshot::channel();

    thread::spawn(move || {
        let mut init_tx = Some(init_tx);
        loop {
            current_epoch += 1;
            let qm = match connect_mq!(&config) {
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
                if let Some(topic) = &config.topic {
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
                    let q_name_str = config.queue.as_deref().ok_or_else(|| {
                        anyhow::anyhow!("Queue name is required for IBM MQ consumer")
                    })?;
                    let q_name = MqStr::<48>::try_from(q_name_str).context("Invalid queue name")?;
                    let od = QueueName(q_name);
                    let mut open_options =
                        constants::MQOO_INPUT_AS_Q_DEF | constants::MQOO_FAIL_IF_QUIESCING;
                    if !config.disable_status_inq {
                        open_options |= constants::MQOO_INQUIRE;
                    }
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
                        let mut buffer = vec![0u8; config.max_message_size];

                        for _ in 0..max_messages {
                            let gmo = (
                                constants::MQGMO_WAIT
                                    | constants::MQGMO_SYNCPOINT
                                    | constants::MQGMO_CONVERT
                                    | constants::MQGMO_FAIL_IF_QUIESCING,
                                get::GetWait::Wait(config.wait_timeout_ms),
                            );

                            let res: Result<Option<(_, MessageFormat)>, _> =
                                obj.get_data_with(&gmo, &mut buffer).discard_warning();

                            match res {
                                Ok(opt) => {
                                    if let Some((data, _format)) = opt {
                                        messages.push(CanonicalMessage::new(data.to_vec(), None));
                                        // zeroing buffer to avoid leaking sensitive data
                                        let buffer_len = data.len();
                                        buffer[..buffer_len].fill(0);
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
                            // If an error occurred during batch retrieval (e.g. quiescing),
                            // we must backout any messages already retrieved in this syncpoint
                            // to ensure atomicity and avoid partial batch processing during shutdown.
                            warn!("Error during IBM MQ batch retrieval, backing out: {}", e);
                            match Syncpoint::new(&qm).backout() {
                                Ok(_) => debug!("Backout on reconnect succeeded"),
                                Err(e) => warn!(
                                    "Backout on reconnect FAILED (messages may be lost): {}",
                                    e
                                ),
                            }
                            connection_error = true;
                            let _ = reply_tx.send(Err(e));
                        } else if !messages.is_empty() {
                            trace!(count = messages.len(), message_ids = ?LazyMessageIds(&messages), "Received batch of IBM MQ messages");
                            let tx_commit = tx_loop.clone();
                            let epoch = current_epoch;
                            let commit_fn: traits::BatchCommitFunc =
                                Box::new(move |dispositions: Vec<MessageDisposition>| {
                                    let tx = tx_commit.clone();
                                    Box::pin(async move {
                                        let (reply_tx, reply_rx) = oneshot::channel();
                                        let should_backout = dispositions
                                            .iter()
                                            .any(|d| matches!(d, MessageDisposition::Nack));

                                        let job = if !should_backout {
                                            ConsumerJob::Commit { epoch, reply_tx }
                                        } else {
                                            ConsumerJob::Backout { epoch, reply_tx }
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

                            if reply_tx
                                .send(Ok(ReceivedBatch {
                                    messages,
                                    commit: commit_fn,
                                }))
                                .is_err()
                            {
                                warn!("Consumer dropped reply channel, backing out transaction");
                                match Syncpoint::new(&qm).backout() {
                                    Ok(_) => debug!("Backout on reconnect succeeded"),
                                    Err(e) => warn!(
                                        "Backout on reconnect FAILED (messages may be lost): {}",
                                        e
                                    ),
                                }
                            }
                        } else {
                            let _ = reply_tx.send(Ok(ReceivedBatch {
                                messages,
                                commit: Box::new(|_| Box::pin(async { Ok(()) })),
                            }));
                        }
                    }
                    ConsumerJob::Commit { epoch, reply_tx } => {
                        if epoch != current_epoch {
                            warn!("Stale commit ignored (epoch {}, current {}); messages were backed out on reconnect", epoch, current_epoch);
                            let _ = reply_tx.send(Err(ConsumerError::Connection(anyhow::anyhow!(
                                "Commit attempted for a stale connection after restart"
                            ))));
                            continue;
                        }
                        let sp = Syncpoint::new(&qm);
                        let res = sp.commit().map(|_| ()).map_err(|e| {
                            ConsumerError::Connection(anyhow::anyhow!("Commit failed: {}", e))
                        });
                        if res.is_err() {
                            connection_error = true;
                        }
                        let _ = reply_tx.send(res);
                    }
                    ConsumerJob::Backout { epoch, reply_tx } => {
                        if epoch != current_epoch {
                            let _ = reply_tx.send(Err(ConsumerError::Connection(anyhow::anyhow!(
                                "Backout attempted for a stale connection after restart"
                            ))));
                            continue;
                        }
                        let sp = Syncpoint::new(&qm);
                        let res = sp.backout().map(|_| ()).map_err(|e| {
                            ConsumerError::Connection(anyhow::anyhow!("Backout failed: {}", e))
                        });
                        if res.is_err() {
                            connection_error = true;
                        }
                        let _ = reply_tx.send(res);
                    }
                    ConsumerJob::Status { reply_tx } => {
                        let mut healthy = true;
                        let mut pending = None;
                        let mut capacity = None;
                        let mut error = None;

                        match obj.inquire(&[MQIA_CURRENT_Q_DEPTH, MQIA_MAX_Q_DEPTH]) {
                            Ok(values) => {
                                let mut iter = values.iter();
                                if let Some(InqResItem::Long(int_item)) = iter.next() {
                                    pending = Some(int_item.int_attr()[0] as usize);
                                }
                                if let Some(InqResItem::Long(int_item)) = iter.next() {
                                    capacity = Some(int_item.int_attr()[0] as usize);
                                }
                            }

                            Err(e) => {
                                if e.2 != constants::MQRC_NOT_OPEN_FOR_INQUIRE
                                    && e.2 != constants::MQRC_NOT_AUTHORIZED
                                {
                                    healthy = false;
                                    error = Some(format!("Failed to inquire queue status: {}", e));
                                }
                            }
                        }

                        let _ = reply_tx.send(EndpointStatus {
                            healthy,
                            target: config
                                .queue
                                .clone()
                                .or(config.topic.clone())
                                .unwrap_or_default(),
                            pending,
                            capacity,
                            error,
                            ..Default::default()
                        });
                    }
                }

                if connection_error {
                    warn!("Connection error detected in consumer thread, backing out any active transaction before reconnecting.");
                    if let Err(e) = Syncpoint::new(&qm).backout() {
                        warn!(
                            "Backout on reconnect failed (broker may have already cleaned up): {}",
                            e
                        );
                    }
                    break;
                }
            }

            if !connection_error {
                // This means the loop exited because the rx channel was closed.
                // This happens when the IbmMqConsumer struct is dropped.
                // We should backout any active transaction before exiting.
                info!("IBM MQ consumer channel closed, backing out any active transaction before exiting thread.");
                match Syncpoint::new(&qm).backout() {
                    Ok(_) => debug!("Backout on reconnect succeeded"),
                    Err(e) => warn!("Backout on reconnect FAILED (messages may be lost): {}", e),
                }
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
    permit: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl MessageConsumer for IbmMqConsumer {
    async fn receive_batch(
        &mut self,
        max_messages: usize,
    ) -> Result<ReceivedBatch, crate::traits::ConsumerError> {
        let permit = self.permit.clone().acquire_owned().await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ consumer semaphore closed"))
        })?;

        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(ConsumerJob::Receive {
                max_messages,
                reply_tx,
            })
            .await
            .map_err(|e| ConsumerError::Connection(anyhow::anyhow!(e)))?;

        let batch = reply_rx.await.map_err(|_| {
            ConsumerError::Connection(anyhow::anyhow!("MQ consumer thread dropped reply"))
        })??;

        if batch.messages.is_empty() {
            // If we got an empty batch, there's no commit to wait for.
            // We can release the permit immediately and return.
            drop(permit);
            return Ok(batch);
        }

        let original_commit: Arc<traits::BatchCommitFunc> = Arc::from(batch.commit);
        let permit = Arc::new(std::sync::Mutex::new(Some(permit)));
        let wrapped_commit = Box::new(move |dispositions| {
            let original_commit = original_commit.clone();
            let permit = permit.clone();
            Box::pin(async move {
                let result = original_commit(dispositions).await;
                if let Some(p) = permit.lock().unwrap().take() {
                    drop(p); // Release the permit, allowing the next receive_batch to proceed.
                }
                result
            }) as traits::BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch {
            messages: batch.messages,
            commit: wrapped_commit,
        })
    }

    async fn status(&self) -> EndpointStatus {
        let (reply_tx, reply_rx) = oneshot::channel();
        let send_result = tokio::time::timeout(
            Duration::from_secs(1),
            self.tx.send(ConsumerJob::Status { reply_tx }),
        )
        .await;

        match send_result {
            Ok(Ok(_)) => {}
            Ok(Err(_)) => {
                return EndpointStatus {
                    healthy: false,
                    error: Some("Consumer thread disconnected".to_string()),
                    ..Default::default()
                };
            }
            Err(_) => {
                return EndpointStatus {
                    healthy: false,
                    error: Some("Status send timed out".to_string()),
                    ..Default::default()
                };
            }
        }

        match tokio::time::timeout(Duration::from_secs(1), reply_rx).await {
            Ok(Ok(status)) => status,
            Ok(Err(_)) => EndpointStatus {
                healthy: false,
                error: Some("Consumer thread dropped status request".to_string()),
                ..Default::default()
            },
            Err(_) => EndpointStatus {
                healthy: false,
                error: Some("Status check timed out".to_string()),
                ..Default::default()
            },
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl IbmMqConsumer {
    pub async fn new(config: &IbmMqConfig) -> Result<Self, ConsumerError> {
        let tx = spawn_consumer_thread(config.clone()).await?;
        Ok(Self {
            tx,
            permit: Arc::new(Semaphore::new(1)),
        })
    }
}
