//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
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

// Module-level constants
const STATUS_TIMEOUT_SECS: u64 = 1;
const RECONNECT_DELAY_SECS: u64 = 1;
const MQ_CERT_VAL_POLICY_NONE: i32 = 2;

/// Macro to perform backout with consistent logging
macro_rules! backout_with_logging {
    ($qm:expr, $context:expr) => {
        match Syncpoint::new($qm).backout() {
            Ok(_) => debug!("Backout {} succeeded", $context),
            Err(e) => warn!("Backout {} FAILED (messages may be lost): {}", $context, e),
        }
    };
}

/// Helper function to handle status requests with timeout
async fn handle_status_request<T>(
    tx: &mpsc::Sender<T>,
    job_constructor: impl FnOnce(oneshot::Sender<EndpointStatus>) -> T,
    endpoint_type: &str,
    target: &str,
) -> EndpointStatus {
    let (reply_tx, reply_rx) = oneshot::channel();
    let send_result = tokio::time::timeout(
        Duration::from_secs(STATUS_TIMEOUT_SECS),
        tx.send(job_constructor(reply_tx)),
    )
    .await;

    match send_result {
        Ok(Ok(_)) => {}
        Ok(Err(_)) => {
            return EndpointStatus {
                healthy: false,
                target: target.to_string(),
                error: Some(format!("{} thread disconnected", endpoint_type)),
                ..Default::default()
            };
        }
        Err(_) => {
            return EndpointStatus {
                healthy: false,
                target: target.to_string(),
                error: Some("Status send timed out".to_string()),
                ..Default::default()
            };
        }
    }

    match tokio::time::timeout(Duration::from_secs(STATUS_TIMEOUT_SECS), reply_rx).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => EndpointStatus {
            healthy: false,
            target: target.to_string(),
            error: Some(format!("{} thread dropped status request", endpoint_type)),
            ..Default::default()
        },
        Err(_) => EndpointStatus {
            healthy: false,
            target: target.to_string(),
            error: Some("Status check timed out".to_string()),
            ..Default::default()
        },
    }
}

/// Marker error: the IBM MQ client library could not be loaded at runtime.
/// Classified as non-retryable so a route fails fast instead of reconnecting
/// forever for a dependency that will not appear without operator action.
#[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
#[derive(Debug)]
struct IbmMqLibraryUnavailable(String);

#[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
impl std::fmt::Display for IbmMqLibraryUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
impl std::error::Error for IbmMqLibraryUnavailable {}

/// True if the IBM MQ client can actually be used in this build: always for the
/// static link (it is bound at build time), or — on the dlopen build — only if
/// the runtime client library is present and loads. Tests use this to skip IBM MQ
/// where no client is installed (e.g. the generic `full` CI job), instead of
/// failing on the first connect attempt.
#[cfg(feature = "ibm-mq-static")]
pub fn ibm_mq_client_available() -> bool {
    true
}
#[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
pub fn ibm_mq_client_available() -> bool {
    load_ibm_mq_library().is_ok()
}

/// True if the connect error is a missing-client-library failure (dlopen build).
fn is_library_unavailable(_e: &anyhow::Error) -> bool {
    #[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
    {
        _e.downcast_ref::<IbmMqLibraryUnavailable>().is_some()
    }
    #[cfg(not(all(feature = "ibm-mq", not(feature = "ibm-mq-static"))))]
    {
        false
    }
}

// Runtime-load the IBM MQ client via dlopen. mqi's load_mqm_default hardcodes
// "libmqm_r.so", which is wrong on macOS (libmqm_r.dylib); this tries the
// platform-correct name plus MQ_INSTALLATION_PATH and an explicit override.
#[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
fn load_ibm_mq_library() -> anyhow::Result<Arc<libmqm_sys::dlopen2::MqmContainer>> {
    use libmqm_sys::dlopen2::MqmContainer;

    let default_name = if cfg!(windows) {
        "mqm.dll"
    } else if cfg!(target_os = "macos") {
        "libmqm_r.dylib"
    } else {
        "libmqm_r.so"
    };

    // Candidates in priority order: explicit override, install dir, bare name.
    let mut candidates: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("MQB_IBM_MQ_LIB") {
        if !p.is_empty() {
            candidates.push(p);
        }
    }
    if let Ok(base) = std::env::var("MQ_INSTALLATION_PATH") {
        if !base.is_empty() {
            candidates.push(format!("{base}/lib64/{default_name}"));
            candidates.push(format!("{base}/lib/{default_name}"));
        }
    }
    candidates.push(default_name.to_string());

    let mut last_err = None;
    for name in &candidates {
        // Safety: loading a dynamic library is inherently unsafe.
        match unsafe { MqmContainer::load(name) } {
            // Arc so the handle is cheaply Clone (required by connection_ref);
            // the Container itself is not Clone.
            Ok(c) => return Ok(Arc::new(c)),
            Err(e) => last_err = Some(format!("{name}: {e}")),
        }
    }
    Err(anyhow::Error::new(IbmMqLibraryUnavailable(format!(
        "failed to load IBM MQ client library (tried {candidates:?}; last error: {}). \
         Install the IBM MQ redistributable client and ensure it is on the library \
         search path, or set MQB_IBM_MQ_LIB to the full path of the libmqm_r library.",
        last_err.unwrap_or_default()
    ))))
}

macro_rules! connect_mq {
    ($config:expr) => {
        (|| -> anyhow::Result<_> {
        let usr = $config.username.as_deref();
        let pwd = $config.password.as_deref();

        let qm_name = MqStr::<48>::try_from($config.queue_manager.as_str())
            .context("Invalid queue manager name")?;

        let mq_server_string = format!("{}/TCP/{}", $config.channel, $config.url);
        let mq_server =
            MqServer::try_from(mq_server_string.as_str()).context("Invalid MQSERVER")?;

        let credentials = if let (Some(u), Some(p)) = (usr, pwd) {
            Credentials::User(u, p.into())
        } else {
            Credentials::Default
        };

        let (tls_opt, cipher_opt) = if $config.tls.required {
            let cipher_spec_str = $config.tls.cipher_spec.as_deref().unwrap_or("");
            let cipher =
                CipherSpec(MqStr::<32>::try_from(cipher_spec_str).context("Invalid cipher spec")?);

            if let Some(key_repo_str) = $config.tls.key_repository.as_deref() {
                let key_repo =
                    KeyRepo(MqStr::<256>::try_from(key_repo_str).context("Invalid Key Repo")?);
                let mut tls = Tls::new(&key_repo, None, &cipher);

                if $config.tls.accept_invalid_certs {
                    tls.cert_val_policy(MQ_CERT_VAL_POLICY_NONE);
                }

                if let Some(pass) = $config.tls.key_repository_password.as_deref() {
                    tls.key_repo_password(Some(mqi::types::ProtectedSecret::new(pass)));
                }

                (Some(tls), None)
            } else {
                let mut tls = Tls::default();

                if $config.tls.accept_invalid_certs {
                    tls.cert_val_policy(MQ_CERT_VAL_POLICY_NONE);
                } else {
                    anyhow::bail!(
                        "TLS required but neither tls.key_repository (MQ key repository) nor tls.accept_invalid_certs was provided"
                    );
                }

                if $config.tls.key_repository_password.is_some() {
                    anyhow::bail!(
                        "tls.key_repository_password requires tls.key_repository to be set (the MQ key repository whose password it unlocks)"
                    );
                }

                (Some(tls), Some(cipher))
            }
        } else {
            (None, None)
        };

        let opts = (
            ApplName(mqstr!("mq-bridge")),
            tls_opt,
            QueueManagerName(qm_name),
            credentials,
            cipher_opt,
            mq_server,
        );

        // Link path: client is statically linked at build time.
        #[cfg(feature = "ibm-mq-static")]
        let connected = mqi::connect::<ThreadNone>(&opts)
            .discard_warning()
            .context("MQ connect failed");
        // dlopen path: load the IBM client at runtime (no SDK needed at build).
        #[cfg(all(feature = "ibm-mq", not(feature = "ibm-mq-static")))]
        let connected = {
            let lib = load_ibm_mq_library()?;
            mqi::connect_lib::<ThreadNone, _>(lib, &opts)
                .discard_warning()
                .context("MQ connect failed")
        };
        connected
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
    target: String,
}

impl IbmMqPublisher {
    pub async fn new(config: &IbmMqConfig) -> Result<Self, PublisherError> {
        let buffer_size = config.internal_buffer_size.unwrap_or(100).max(1);
        let (tx, mut rx) = mpsc::channel::<PublisherJob>(buffer_size);
        let (init_tx, init_rx) = oneshot::channel();
        let config = config.clone();
        let target = config
            .queue
            .clone()
            .or(config.topic.clone())
            .unwrap_or_default();
        info!("Starting IBM MQ publisher");

        thread::spawn(move || {
            let mut init_tx = Some(init_tx);
            loop {
                let qm = match connect_mq!(&config) {
                    Ok(q) => q,
                    Err(e) => {
                        if let Some(tx) = init_tx.take() {
                            // A missing client library will not fix itself; fail fast.
                            let err = if is_library_unavailable(&e) {
                                PublisherError::NonRetryable(e)
                            } else {
                                PublisherError::Retryable(e)
                            };
                            let _ = tx.send(Err(err));
                            return;
                        }
                        thread::sleep(Duration::from_secs(RECONNECT_DELAY_SECS));
                        continue;
                    }
                };

                let queue = match (|| -> anyhow::Result<_> {
                    let mut open_options =
                        constants::MQOO_OUTPUT | constants::MQOO_FAIL_IF_QUIESCING;
                    let qm_ref = qm.connection_ref();

                    if let Some(topic) = &config.topic {
                        // MQOO_INQUIRE is invalid for a topic opened for output
                        // (MQRC_OPTIONS_ERROR); status checks tolerate
                        // MQRC_NOT_OPEN_FOR_INQUIRE, so it is only set for queues.
                        let topic_str = MqStr::<1024>::try_from(topic.as_str())
                            .context("Invalid topic string")?;
                        let od = open::ObjectString(&topic_str);
                        Object::open(qm_ref, &(od, open_options))
                            .map_err(|e| anyhow::anyhow!("MQ open topic failed: {}", e))
                    } else {
                        if !config.disable_status_inq {
                            open_options |= constants::MQOO_INQUIRE;
                        }
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
                        thread::sleep(Duration::from_secs(RECONNECT_DELAY_SECS));
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
                                        backout_with_logging!(&qm, "on commit failure");
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
                                target: config
                                    .queue
                                    .clone()
                                    .or(config.topic.clone())
                                    .unwrap_or_default(),
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
                    backout_with_logging!(&qm, "on channel close");
                    break;
                }
            }
        });

        match init_rx.await {
            Ok(Ok(())) => Ok(Self { tx, target }),
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
        handle_status_request(&self.tx, PublisherJob::Status, "Publisher", &self.target).await
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
                    thread::sleep(Duration::from_secs(RECONNECT_DELAY_SECS));
                    continue;
                }
            };

            let (_sub, obj) = match (|| -> anyhow::Result<_> {
                let qm_ref = qm.connection_ref();
                // Determine if we are acting as a Subscriber (Topic) or a Consumer (Queue)
                if let Some(topic) = &config.topic {
                    let topic_str =
                        MqStr::<1024>::try_from(topic.as_str()).context("Invalid topic string")?;
                    // Ephemeral managed subscription: no MQSO_RESUME, which would
                    // require a subscription name (MQRC_SUB_NAME_ERROR otherwise).
                    let sub_opts = (
                        constants::MQSO_CREATE
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
                    thread::sleep(Duration::from_secs(RECONNECT_DELAY_SECS));
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
                                    | constants::MQGMO_FAIL_IF_QUIESCING
                                    // Strip pub/sub RFH2 so the clean body is
                                    // delivered (otherwise topic gets yield empty data).
                                    | constants::MQGMO_NO_PROPERTIES,
                                get::GetWait::Wait(config.wait_timeout_ms),
                            );

                            let res: Result<Option<(_, MessageFormat)>, _> =
                                obj.get_data_with(&gmo, &mut buffer).discard_warning();

                            match res {
                                Ok(opt) => {
                                    if let Some((data, _format)) = opt {
                                        let mut canonical =
                                            CanonicalMessage::new(data.to_vec(), None);
                                        // Source cursor: the queue this was read from.
                                        // Opt-in via MQB_SOURCE_METADATA; off by default.
                                        if crate::canonical_message::source_metadata_enabled() {
                                            if let Some(queue) = config.queue.as_deref() {
                                                canonical.metadata.insert(
                                                    "mqb.src.ibmmq_queue".to_string(),
                                                    queue.to_string(),
                                                );
                                            }
                                        }
                                        messages.push(canonical);
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
                            backout_with_logging!(&qm, "on batch retrieval error");
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
                                backout_with_logging!(&qm, "on dropped reply channel");
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
                    backout_with_logging!(&qm, "on reconnect");
                    break;
                }
            }

            if !connection_error {
                // This means the loop exited because the rx channel was closed.
                // This happens when the IbmMqConsumer struct is dropped.
                // We should backout any active transaction before exiting.
                info!("IBM MQ consumer channel closed, backing out any active transaction before exiting thread.");
                backout_with_logging!(&qm, "on channel close");
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
    target: String,
}

#[async_trait]
impl MessageConsumer for IbmMqConsumer {
    // Intentionally keeps the ordered default: messages are retrieved under an MQ
    // syncpoint and committed/backed-out as one transaction, so commits must stay
    // serialized and in order to preserve transaction boundaries.
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

        let original_commit = batch.commit;
        let wrapped_commit = Box::new(move |dispositions| {
            Box::pin(async move {
                let result = original_commit(dispositions).await;
                drop(permit); // Release the permit, allowing the next receive_batch to proceed.
                result
            }) as traits::BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch {
            messages: batch.messages,
            commit: wrapped_commit,
        })
    }

    async fn status(&self) -> EndpointStatus {
        handle_status_request(
            &self.tx,
            |reply_tx| ConsumerJob::Status { reply_tx },
            "Consumer",
            &self.target,
        )
        .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl IbmMqConsumer {
    pub async fn new(config: &IbmMqConfig) -> anyhow::Result<Self> {
        let tx = spawn_consumer_thread(config.clone())
            .await
            .map_err(|ce| match ce {
                // A missing client library will not fix itself; mark it
                // non-retryable so the route fails fast instead of
                // reconnecting forever.
                ConsumerError::Connection(inner) if is_library_unavailable(&inner) => {
                    anyhow::Error::new(crate::errors::ProcessingError::NonRetryable(inner))
                }
                other => anyhow::Error::new(other),
            })?;
        Ok(Self {
            tx,
            permit: Arc::new(Semaphore::new(1)),
            target: config
                .queue
                .clone()
                .or(config.topic.clone())
                .unwrap_or_default(),
        })
    }
}

#[cfg(all(test, feature = "ibm-mq", not(feature = "ibm-mq-static")))]
mod dlopen_tests {
    use super::*;

    // Proves the runtime dlopen of the IBM client succeeds where it is installed.
    // Ignored by default (needs the client present); run with:
    //   MQ_INSTALLATION_PATH=/opt/mqm cargo test -p mq-bridge \
    //     --no-default-features --features ibm-mq -- --ignored loads_client
    #[test]
    #[ignore]
    fn loads_client() {
        load_ibm_mq_library().expect("IBM MQ client library should load");
    }

    // A failed library load must be classified non-retryable so routes fail
    // fast instead of reconnecting forever. Skips on hosts that have the client
    // installed: the loader always falls back to the bare library name, so there
    // is no way to force a miss there.
    #[test]
    fn missing_library_is_non_retryable() {
        // Save & restore the env we mutate so this test can't leak into sibling
        // dlopen tests (e.g. loads_client reads MQ_INSTALLATION_PATH). Drop runs
        // even if the assertions below panic.
        struct EnvRestore {
            lib: Option<String>,
            path: Option<String>,
        }
        impl Drop for EnvRestore {
            fn drop(&mut self) {
                match &self.lib {
                    Some(v) => std::env::set_var("MQB_IBM_MQ_LIB", v),
                    None => std::env::remove_var("MQB_IBM_MQ_LIB"),
                }
                match &self.path {
                    Some(v) => std::env::set_var("MQ_INSTALLATION_PATH", v),
                    None => std::env::remove_var("MQ_INSTALLATION_PATH"),
                }
            }
        }
        let _restore = EnvRestore {
            lib: std::env::var("MQB_IBM_MQ_LIB").ok(),
            path: std::env::var("MQ_INSTALLATION_PATH").ok(),
        };

        std::env::set_var("MQB_IBM_MQ_LIB", "/nonexistent/path/libmqm_r");
        std::env::remove_var("MQ_INSTALLATION_PATH");
        let err = match load_ibm_mq_library() {
            Ok(_) => {
                eprintln!("skipping: IBM MQ client is installed, cannot test a missing one");
                return;
            }
            Err(e) => e,
        };
        assert!(
            is_library_unavailable(&err),
            "missing-library error must be classified non-retryable: {err:?}"
        );
    }
}
