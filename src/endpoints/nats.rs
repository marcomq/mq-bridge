use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::NatsConfig;
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, Sent, SentBatch,
};
use crate::CanonicalMessage;
use crate::APP_NAME;
use anyhow::anyhow;
use async_nats::connection::State;
use async_nats::header::{HeaderMap, HeaderName, HeaderValue};
use async_nats::jetstream::consumer::pull;
use async_nats::{jetstream, jetstream::stream, ConnectOptions};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt, TryStreamExt};
use tracing::{info, trace, warn};
use uuid::Uuid;

enum NatsClient {
    Core(async_nats::Client),
    JetStream(jetstream::Context),
}

pub struct NatsPublisher {
    client: NatsClient,
    core_client: async_nats::Client,
    // Retains the shared registry entry so concurrent publishers reuse this connection.
    _shared_client: std::sync::Arc<async_nats::Client>,
    subject: String,
    // If false, wait for JetStream acknowledgment; if true, fire-and-forget.
    delayed_ack: bool,
    request_reply: bool,
    request_timeout: std::time::Duration,
    // If true, publish a `Nats-Msg-Id` header (from the message id) so JetStream
    // deduplicates redeliveries within the stream's duplicate window.
    deduplicate: bool,
}

impl NatsPublisher {
    pub async fn new(config: &NatsConfig) -> anyhow::Result<Self> {
        let subject = config.subject.as_deref().ok_or_else(|| {
            crate::errors::InvalidConfig(anyhow!("Subject is required for NATS publisher"))
        })?;
        let stream_name = if !config.no_jetstream {
            config
                .stream
                .as_deref()
                .ok_or_else(|| crate::errors::InvalidConfig(anyhow!("stream must be provided when JetStream is enabled: set `stream`, or `no_jetstream: true` for core NATS")))?
        } else {
            config.stream.as_deref().unwrap_or_default()
        };
        // Share one NATS connection across publishers with the same connection settings;
        // the subject is per-publish. JetStream context/stream setup stays per-publisher.
        let identity = crate::support::connection_registry::connection_identity((
            &config.url,
            &config.username,
            &config.password,
            &config.token,
            config.tls.required,
            &config.tls.ca_file,
            &config.tls.cert_file,
            &config.tls.key_file,
            config.tls.accept_invalid_certs,
        ));
        let config_clone = config.clone();
        let shared_client = crate::support::connection_registry::get_or_create(
            "nats-client",
            identity,
            config.shared.unwrap_or(true),
            move || async move {
                let options = build_nats_options(&config_clone).await?;
                Ok(options.connect(server_list(&config_clone.url)).await?)
            },
        )
        .await?;
        let nats_client = (*shared_client).clone();
        let core_client = nats_client.clone();

        let client = if !config.no_jetstream {
            let jetstream = jetstream::new(nats_client);
            info!(stream = %stream_name, "Ensuring NATS JetStream stream exists");
            let subjects = if subject.contains('>') || subject.contains('*') {
                vec![subject.to_string()]
            } else {
                vec![format!("{}.>", stream_name)]
            };
            ensure_stream(
                &jetstream,
                stream::Config {
                    name: stream_name.to_string(),
                    subjects,
                    max_messages: config.stream_max_messages.unwrap_or(1_000_000),
                    max_bytes: config.stream_max_bytes.unwrap_or(1024 * 1024 * 1024), // 1GB
                    ..Default::default()
                },
            )
            .await?;
            NatsClient::JetStream(jetstream)
        } else {
            info!("NATS publisher is in Core mode (non-persistent).");
            if config.delayed_ack {
                tracing::debug!("'delayed_ack' is true but NATS is in Core mode, which always performs fire and forget. The flag will be ignored.");
            }
            NatsClient::Core(nats_client)
        };

        Ok(Self {
            client,
            core_client,
            _shared_client: shared_client,
            subject: subject.to_string(),
            delayed_ack: config.delayed_ack,
            request_reply: config.request_reply,
            request_timeout: std::time::Duration::from_millis(
                config.request_timeout_ms.unwrap_or(30_000),
            ),
            deduplicate: config.deduplicate,
        })
    }
}

#[async_trait]
impl MessagePublisher for NatsPublisher {
    async fn send(&self, message: CanonicalMessage) -> Result<Sent, PublisherError> {
        trace!(
            subject = %self.subject,
            message_id = %format!("{:032x}", message.message_id),
            payload_size = message.payload.len(),
            "Publishing NATS message"
        );
        let headers = publish_headers(&message, self.deduplicate);

        if self.request_reply {
            let request_id = message.message_id;
            let response = tokio::time::timeout(
                self.request_timeout,
                self.core_client.request_with_headers(
                    self.subject.clone(),
                    headers,
                    message.payload,
                ),
            )
            .await
            .map_err(|_| PublisherError::Retryable(anyhow!("NATS request timed out")))?
            .map_err(|e| {
                let oversize = e.kind() == async_nats::RequestErrorKind::MaxPayloadExceeded;
                publish_error(anyhow!("NATS request failed: {}", e), oversize)
            })?;

            // A batch matches each reply to its request by id.
            let mut response_msg = create_nats_canonical_message(&response, None, false, false);
            response_msg.message_id = request_id;
            return Ok(Sent::Response(response_msg));
        }

        match &self.client {
            NatsClient::JetStream(jetstream) => {
                tracing::trace!("Publishing to NATS JetStream subject: {}", self.subject);
                let ack_future = jetstream
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await
                    .map_err(|e| {
                        let oversize =
                            e.kind() == jetstream::context::PublishErrorKind::MaxPayloadExceeded;
                        publish_error(
                            anyhow!("Failed to publish to NATS JetStream: {e}"),
                            oversize,
                        )
                    })?;
                tracing::trace!("Published to NATS JetStream, waiting for ack");

                if !self.delayed_ack {
                    match tokio::time::timeout(std::time::Duration::from_secs(5), ack_future).await
                    {
                        Ok(Ok(_)) => tracing::trace!("Ack received"),
                        Ok(Err(e)) => {
                            return Err(PublisherError::Retryable(anyhow!(
                                "NATS Ack failed: {}",
                                e
                            )))
                        }
                        Err(_) => {
                            return Err(PublisherError::Retryable(anyhow!("NATS Ack timed out")))
                        }
                    }
                }
            }
            NatsClient::Core(client) => {
                client
                    .publish_with_headers(self.subject.clone(), headers, message.payload)
                    .await
                    .map_err(|e| {
                        let oversize =
                            e.kind() == async_nats::client::PublishErrorKind::MaxPayloadExceeded;
                        publish_error(anyhow!("Failed to publish to NATS Core: {e}"), oversize)
                    })?;
            }
        }

        Ok(Sent::Ack)
    }

    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        trace!(
            subject = %self.subject,
            count = messages.len(),
            message_ids = ?LazyMessageIds(&messages),
            "Publishing batch of NATS messages"
        );

        if self.request_reply {
            // For request-reply, we must send individually and gather responses.
            return crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m)))
                .await;
        }

        match &self.client {
            NatsClient::JetStream(_jetstream) => {
                // send_batch_helper pipelines the per-message PubAcks (bounded
                // in-flight, order preserved), so batch_size now translates into
                // overlapping acks instead of one serial round trip per message.
                crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
            }
            NatsClient::Core(_) => {
                // Core NATS is fire-and-forget (no server ack); the helper just
                // streams the publishes into the client write buffer.
                crate::traits::send_batch_helper(self, messages, |p, m| Box::pin(p.send(m))).await
            }
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn flush(&self) -> anyhow::Result<()> {
        self.core_client
            .flush()
            .await
            .map_err(|e| anyhow!("NATS flush failed: {}", e))
    }

    async fn status(&self) -> EndpointStatus {
        EndpointStatus {
            healthy: self.core_client.connection_state() == State::Connected,
            target: self.subject.clone(),
            pending: None,
            capacity: None,
            error: if self.core_client.connection_state() == State::Connected {
                None
            } else {
                Some("Disconnected".to_string())
            },
            ..Default::default()
        }
    }
}

/// A message over the server's payload limit fails alone; resending it cannot succeed.
fn publish_error(error: anyhow::Error, oversize: bool) -> PublisherError {
    if oversize {
        PublisherError::NonRetryable(error)
    } else {
        PublisherError::Retryable(error)
    }
}

fn warn_unsendable_header(key: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        warn!(key = %key, "NATS cannot carry this metadata entry as a header (name with a space, ':' or non-ASCII, or a value with a line break); it is left out. Logged once.");
    }
}

/// The NATS headers of an outgoing message: its metadata and the message id.
fn publish_headers(message: &CanonicalMessage, deduplicate: bool) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (key, value) in &message.metadata {
        if crate::canonical_message::is_source_metadata_key(key) {
            continue; // source/provenance keys must not be forwarded
        }
        // async-nats asserts on a name or value the protocol cannot carry.
        match (key.parse::<HeaderName>(), value.parse::<HeaderValue>()) {
            (Ok(name), Ok(value)) if !key.is_empty() => headers.insert(name, value),
            _ => warn_unsendable_header(key),
        }
    }
    headers.insert(
        "mq_bridge.message_id",
        format!("{:032x}", message.message_id).as_str(),
    );

    // Opt-in JetStream deduplication: set Nats-Msg-Id from the message id so the
    // server drops redeliveries within its duplicate window. Skip if the caller
    // already supplied an explicit Nats-Msg-Id via metadata. The consumer also
    // recovers the message id from this header, so it round-trips either way.
    if deduplicate && headers.get("Nats-Msg-Id").is_none() {
        headers.insert(
            "Nats-Msg-Id",
            format!("{:032x}", message.message_id).as_str(),
        );
    }
    headers
}

enum NatsCore {
    Ephemeral(async_nats::Subscriber),
    JetStream {
        consumer: Box<jetstream::consumer::Consumer<pull::Config>>,
        stream: Box<jetstream::consumer::pull::Stream>,
    },
}

pub struct NatsConsumer {
    core: NatsCore,
    client: async_nats::Client,
    subject: String,
    source_metadata: bool,
    /// Set by the route when `exit_on_empty`/`--drain` is active. Only then does an
    /// idle read time out into an empty batch; otherwise it blocks indefinitely
    /// (event-driven, no added latency) as a streaming source should.
    exit_on_empty: bool,
    /// Messages received and not yet committed, so a drain can tell its own unacked
    /// messages from those a crashed run left behind.
    held: Arc<AtomicUsize>,
}
use std::any::Any;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

impl NatsConsumer {
    pub async fn new(config: &NatsConfig) -> anyhow::Result<Self> {
        Self::new_with_source_metadata(config, false).await
    }

    pub async fn new_with_source_metadata(
        config: &NatsConfig,
        source_metadata: bool,
    ) -> anyhow::Result<Self> {
        let source_metadata = crate::canonical_message::source_metadata_enabled_for_endpoint(
            source_metadata || config.source_metadata,
        );
        let subject = config.subject.as_deref().ok_or_else(|| {
            crate::errors::InvalidConfig(anyhow!("Subject is required for NATS consumer"))
        })?;
        let stream_name = config.stream.as_deref().ok_or_else(|| {
            crate::errors::InvalidConfig(anyhow!("Stream name is required for NATS consumer"))
        })?;

        let deliver_policy = match config.deliver_policy {
            Some(crate::models::NatsDeliverPolicy::All) | None => {
                jetstream::consumer::DeliverPolicy::All
            }
            Some(crate::models::NatsDeliverPolicy::Last) => {
                jetstream::consumer::DeliverPolicy::Last
            }
            Some(crate::models::NatsDeliverPolicy::New) => jetstream::consumer::DeliverPolicy::New,
            Some(crate::models::NatsDeliverPolicy::LastPerSubject) => {
                jetstream::consumer::DeliverPolicy::LastPerSubject
            }
        };

        let (durable_name, queue_group) = if config.subscriber_mode {
            (None, None)
        } else {
            let durable = durable_name(stream_name, subject);
            let queue = format!("{}-{}", APP_NAME, stream_name.replace('.', "-"));
            (Some(durable), Some(queue))
        };

        let (core, client) = NatsCore::connect(
            config,
            stream_name,
            subject,
            durable_name,
            deliver_policy,
            queue_group,
        )
        .await?;
        Ok(Self {
            core,
            client,
            subject: subject.to_string(),
            source_metadata,
            exit_on_empty: false,
            held: Arc::default(),
        })
    }
}

#[async_trait]
impl MessageConsumer for NatsConsumer {
    // JetStream acks each message individually (and Core has no ack), so commits
    // can run concurrently without risking the data loss cumulative-ack brokers face.
    fn commit_requires_order(&self) -> bool {
        false
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        self.core
            .receive_batch(
                max_messages,
                &self.subject,
                &self.client,
                self.exit_on_empty,
                self.source_metadata,
                &self.held,
            )
            .await
    }

    async fn status(&self) -> EndpointStatus {
        let mut healthy = self.client.connection_state() == State::Connected;
        let mut pending = None;
        let mut error = None;

        if healthy {
            match &self.core {
                NatsCore::Ephemeral(_sub) => {
                    pending = None;
                }
                NatsCore::JetStream { consumer, .. } => match consumer.get_info().await {
                    Ok(info) => {
                        pending = Some(info.num_pending.try_into().unwrap_or(usize::MAX));
                    }
                    Err(e) => {
                        healthy = false;
                        error = Some(format!("Failed to get consumer info: {}", e));
                    }
                },
            }
        } else {
            error = Some(format!(
                "Disconnected: {:?}",
                self.client.connection_state()
            ));
        }

        EndpointStatus {
            healthy,
            target: self.subject.clone(),
            pending,
            error,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The servers of a comma-separated `url`; async-nats parses a `&str` as one address.
fn server_list(url: &str) -> Vec<&str> {
    url.split(',')
        .map(str::trim)
        .filter(|server| !server.is_empty())
        .collect()
}

// ... rest of the file
async fn build_nats_options(config: &NatsConfig) -> anyhow::Result<ConnectOptions> {
    let options = if let Some(token) = &config.token {
        ConnectOptions::with_token(token.clone())
    } else if let (Some(user), Some(pass)) = (&config.username, &config.password) {
        ConnectOptions::with_user_and_password(user.clone(), pass.clone())
    } else {
        ConnectOptions::new()
    };

    if !config.tls.required {
        let has_credentials = config.token.is_some() || config.password.is_some();
        crate::support::tls_check::warn_plaintext_credentials(
            "nats",
            &config.url,
            has_credentials,
            config
                .url
                .split(',')
                .all(|server| server.trim().starts_with("tls://")),
        );
        return Ok(options);
    }

    // `require_tls`: without it the client follows the server and stays in the clear.
    let tls_config = crate::support::tls::client_config(&config.tls)?;
    Ok(options.require_tls(true).tls_client_config(tls_config))
}

/// `get_or_create_stream`, tolerant of another client creating the stream at the same moment.
async fn ensure_stream(
    jetstream: &jetstream::Context,
    config: stream::Config,
) -> anyhow::Result<stream::Stream> {
    let name = config.name.clone();
    match jetstream.get_or_create_stream(config).await {
        Ok(stream) => Ok(stream),
        // Lost the creation race: the stream exists now, so take it as a later start would.
        Err(e)
            if e.to_string()
                .to_ascii_lowercase()
                .contains("stream name already in use") =>
        {
            jetstream.get_stream(&name).await.map_err(|_| anyhow!(e))
        }
        Err(e) => Err(anyhow!(e)),
    }
}

/// True for JetStream setup failures a reconnect cannot change: the stream exists but its
/// configuration is incompatible with the route's `stream`/`subject` (most often the subject
/// is not covered by the stream's subjects). Retrying rebuilds the identical mismatch.
fn is_permanent_jetstream_setup_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("filter subject")
        || m.contains("does not match any subject")
        || m.contains("subjects overlap")
        || m.contains("stream name already in use")
}

/// Classify a JetStream setup failure so a configuration mismatch stops the route instead of
/// spinning on the reconnect interval forever. Connect/IO failures stay retryable.
fn classify_jetstream_setup_error(
    e: impl std::fmt::Display,
    stream: &str,
    subject: &str,
) -> anyhow::Error {
    let msg = format!("NATS JetStream setup failed (stream '{stream}', subject '{subject}'): {e}");
    if is_permanent_jetstream_setup_error(&msg) {
        anyhow::Error::new(ConsumerError::Permanent(anyhow!(
            "{msg}. This is a configuration mismatch, not a transient failure: reconcile the \
             route's `stream`/`subject` with the existing stream, or update the stream."
        )))
    } else {
        anyhow!(msg)
    }
}

impl NatsCore {
    async fn connect(
        config: &NatsConfig,
        stream_name: &str,
        subject: &str,
        durable_name: Option<String>,
        deliver_policy: jetstream::consumer::DeliverPolicy,
        queue_group: Option<String>,
    ) -> anyhow::Result<(Self, async_nats::Client)> {
        let options = build_nats_options(config).await?;
        let client = options.connect(server_list(&config.url)).await?;
        let client_clone = client.clone();

        if !config.no_jetstream {
            let jetstream = jetstream::new(client);
            info!(stream = %stream_name, subject = %subject, "NATS endpoint is in JetStream mode.");

            ensure_stream(
                &jetstream,
                stream::Config {
                    name: stream_name.to_string(),
                    subjects: vec![subject.to_string()],
                    max_messages: config.stream_max_messages.unwrap_or(1_000_000),
                    max_bytes: config.stream_max_bytes.unwrap_or(1024 * 1024 * 1024), // 1GB
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| classify_jetstream_setup_error(e, stream_name, subject))?;

            let stream = jetstream.get_stream(stream_name).await?;

            let max_ack_pending = config.prefetch_count.unwrap_or(10000) as i64;
            let consumer = stream
                .create_consumer(jetstream::consumer::pull::Config {
                    durable_name,
                    filter_subject: subject.to_string(),
                    deliver_policy,
                    max_ack_pending,
                    ..Default::default()
                })
                .await
                .map_err(|e| classify_jetstream_setup_error(e, stream_name, subject))?;

            let stream = consumer.messages().await?;
            info!(stream = %stream_name, subject = %subject, "NATS JetStream subscribed");
            Ok((
                NatsCore::JetStream {
                    consumer: Box::new(consumer),
                    stream: Box::new(stream),
                },
                client_clone,
            ))
        } else {
            info!(subject = %subject, "NATS endpoint is in Core mode.");
            let sub = if let Some(qg) = queue_group {
                info!(queue_group = %qg, "Using queue subscription");
                client.queue_subscribe(subject.to_string(), qg).await?
            } else {
                client.subscribe(subject.to_string()).await?
            };
            client.flush().await?;
            info!(subject = %subject, "NATS Core subscribed");
            Ok((NatsCore::Ephemeral(sub), client_clone))
        }
    }

    async fn receive_batch(
        &mut self,
        max_messages: usize,
        subject: &str,
        client: &async_nats::Client,
        exit_on_empty: bool,
        source_metadata: bool,
        held: &Arc<AtomicUsize>,
    ) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        // In drain mode (`exit_on_empty`/`--drain`) block only briefly for the first
        // message, then surface an empty batch so the drain can fire instead of blocking
        // forever. Otherwise (streaming, the default) block indefinitely — event-driven,
        // no added latency. `.next()` is cancel-safe, so the timeout loses no message.
        match self {
            NatsCore::JetStream { stream, consumer } => {
                let mut canonical_messages = Vec::with_capacity(max_messages);
                let mut jetstream_messages = Vec::with_capacity(max_messages);

                tracing::trace!("Waiting for next NATS JetStream message");
                let mut drain_deadline = None;
                let message_stream = loop {
                    match crate::traits::drain_gated(exit_on_empty, stream.next()).await {
                        Some(next) => break next,
                        None if drain_must_wait(consumer, held, &mut drain_deadline).await => {}
                        None => return Ok(ReceivedBatch::empty()), // Idle: let the drain fire.
                    }
                };
                tracing::trace!("Received NATS JetStream message");

                // Process the first message if it exists
                match message_stream {
                    Some(Ok(first_message)) => {
                        let sequence = first_message.info().ok().map(|meta| meta.stream_sequence);
                        canonical_messages.push(create_nats_canonical_message(
                            &first_message,
                            sequence,
                            false,
                            source_metadata,
                        ));
                        jetstream_messages.push(first_message);
                    }
                    Some(Err(e)) => return Err(ConsumerError::Connection(anyhow::anyhow!(e))),
                    None => {
                        return Err(ConsumerError::Connection(anyhow::anyhow!(
                            "NATS JetStream ended"
                        )))
                    }
                }

                // Greedily fetch the rest of the batch
                while canonical_messages.len() < max_messages {
                    match stream.try_next().now_or_never() {
                        Some(Ok(Some(message))) => {
                            let sequence = message.info().ok().map(|meta| meta.stream_sequence);
                            canonical_messages.push(create_nats_canonical_message(
                                &message,
                                sequence,
                                false,
                                source_metadata,
                            ));
                            jetstream_messages.push(message);
                        }
                        _ => break, // No more messages in the buffer or stream ended/errored
                    }
                }

                trace!(count = canonical_messages.len(), subject = %subject, message_ids = ?LazyMessageIds(&canonical_messages), "Received batch of NATS JetStream messages");
                let client = client.clone();
                let count = jetstream_messages.len();
                held.fetch_add(count, Ordering::AcqRel);
                let held = Arc::clone(held);
                let commit_closure: BatchCommitFunc = Box::new(move |dispositions| {
                    Box::pin(async move {
                        // Handle replies if responses are provided

                        if dispositions.len() != jetstream_messages.len() {
                            tracing::warn!(
                                    "NATS JetStream batch reply count mismatch: received {} messages but got {} responses. Pairing up to the shorter length.",
                                    jetstream_messages.len(),
                                    dispositions.len()
                                );
                        }
                        handle_jetstream_replies(&client, &jetstream_messages, &dispositions).await;

                        // Acknowledge messages concurrently.
                        // A concurrency limit of 100 is chosen to balance parallelism
                        // with not overwhelming the NATS server or spawning too many tasks.
                        let acked = handle_jetstream_acks(jetstream_messages, dispositions).await;
                        held.fetch_sub(count, Ordering::AcqRel);
                        acked
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                Ok(ReceivedBatch {
                    messages: canonical_messages,
                    commit: commit_closure,
                })
            }
            NatsCore::Ephemeral(sub) => {
                let mut messages = Vec::with_capacity(max_messages);
                let mut reply_subjects = Vec::with_capacity(max_messages);

                let Some(first) = crate::traits::drain_gated(exit_on_empty, sub.next()).await
                else {
                    return Ok(ReceivedBatch::empty()); // Idle: let the drain fire.
                };
                if let Some(message) = first {
                    // Note: reply_subjects are recorded from the native Message.reply while metadata["reply_to"]
                    // may reflect header-provided values (read by route.rs). The batch commit closure always
                    // publishes to the original Message.reply to preserve native NATS reply semantics.
                    reply_subjects.push(message.reply.clone());
                    messages.push(create_nats_canonical_message(
                        &message,
                        None,
                        true,
                        source_metadata,
                    ));

                    while messages.len() < max_messages {
                        match sub.next().now_or_never() {
                            Some(Some(message)) => {
                                reply_subjects.push(message.reply.clone());
                                messages.push(create_nats_canonical_message(
                                    &message,
                                    None,
                                    true,
                                    source_metadata,
                                ))
                            }
                            _ => break,
                        }
                    }
                } else {
                    return Err(ConsumerError::Connection(anyhow::anyhow!(
                        "NATS Core subscription ended"
                    )));
                }

                let client = client.clone();
                let commit_closure: BatchCommitFunc = Box::new(move |dispositions| {
                    Box::pin(async move {
                        let mut sent_reply = false;
                        if dispositions.len() != reply_subjects.len() {
                            tracing::warn!(
                                    "NATS Core batch reply count mismatch: received {} messages but got {} responses. Pairing up to the shorter length.",
                                    reply_subjects.len(),
                                    dispositions.len()
                                );
                        }
                        for (reply_opt, disposition) in reply_subjects.iter().zip(dispositions) {
                            // Only send a reply if the NATS message has a reply subject and the disposition is a Reply.
                            if let (Some(reply), MessageDisposition::Reply(resp)) =
                                (reply_opt, disposition)
                            {
                                let publish_result = tokio::time::timeout(
                                    std::time::Duration::from_secs(60),
                                    client.publish(reply.clone(), resp.payload),
                                )
                                .await;

                                match publish_result {
                                    Err(_) => {
                                        tracing::error!(
                                            subject = %reply,
                                            "Failed to publish NATS reply (timeout)"
                                        );
                                    }
                                    Ok(Err(e)) => {
                                        tracing::error!(
                                            subject = %reply,
                                            error = %e,
                                            "Failed to publish NATS reply"
                                        );
                                    }
                                    Ok(Ok(_)) => {
                                        sent_reply = true;
                                    }
                                }
                            }
                        }
                        if sent_reply {
                            client.flush().await.map_err(|e| {
                                anyhow::anyhow!("Failed to flush NATS replies: {}", e)
                            })?;
                        }
                        Ok(())
                    }) as BoxFuture<'static, anyhow::Result<()>>
                });

                trace!(count = messages.len(), subject = %subject, message_ids = ?LazyMessageIds(&messages), "Received batch of NATS Core messages");
                Ok(ReceivedBatch {
                    messages,
                    commit: commit_closure,
                })
            }
        }
    }
}

fn create_nats_canonical_message(
    message: &async_nats::Message,
    sequence: Option<u64>,
    include_native_reply_to: bool,
    source_metadata: bool,
) -> CanonicalMessage {
    // The most reliable ID is the JetStream sequence number.
    let mut message_id: Option<u128> = None;

    if let Some(headers) = &message.headers {
        if let Some(val) = headers.get("mq_bridge.message_id") {
            if let Ok(id) = u128::from_str_radix(val.as_str(), 16) {
                message_id = Some(id);
            }
        }
    }

    if message_id.is_none() {
        message_id = sequence.map(|s| s as u128);
    }

    // If no sequence is available (e.g., Core NATS), fall back to the Nats-Msg-Id header.
    if message_id.is_none() {
        if let Some(headers) = &message.headers {
            if let Some(msg_id_header) = headers.get("Nats-Msg-Id") {
                let id_str = msg_id_header.as_str();
                // Attempt to parse the ID as a UUID or a raw u128.
                if let Ok(uuid) = Uuid::parse_str(id_str) {
                    message_id = Some(uuid.as_u128());
                } else if let Ok(n) = id_str.parse::<u128>() {
                    message_id = Some(n);
                } else {
                    warn!(header_value = %id_str, "Could not parse 'Nats-Msg-Id' header as a UUID or u128");
                }
            }
        }
    }

    let mut canonical_message = CanonicalMessage::new(message.payload.to_vec(), message_id);
    if let Some(headers) = &message.headers {
        if !headers.is_empty() {
            let mut metadata = std::collections::HashMap::new();
            for (key, value) in headers.iter() {
                let key = key.to_string();
                // Never let an inbound header spoof a reserved `mqb.src.*` value;
                // the authoritative cursor keys are injected below.
                if crate::canonical_message::is_source_metadata_key(&key) {
                    continue;
                }
                // Join multiple values with comma to avoid data loss
                let joined_value = value
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                if !joined_value.is_empty() {
                    metadata.insert(key, joined_value);
                }
            }
            canonical_message.metadata = metadata;
        }
    }
    if include_native_reply_to {
        if let Some(reply) = &message.reply {
            canonical_message
                .metadata
                .entry("reply_to".to_string())
                .or_insert_with(|| reply.to_string());
        }
    }
    // Source-position cursor keys (useful for dlt-style pull consumers; the
    // per-message subject is the only way to recover it under a wildcard
    // subscription). `nats_stream_sequence` is absent for core NATS.
    if source_metadata {
        canonical_message.metadata.insert(
            "mqb.src.nats_subject".to_string(),
            message.subject.to_string(),
        );
        if let Some(sequence) = sequence {
            canonical_message.metadata.insert(
                "mqb.src.nats_stream_sequence".to_string(),
                sequence.to_string(),
            );
        }
    }
    canonical_message
}

/// Where the reply to a JetStream message goes: its `reply_to` header.
fn jetstream_reply_subject(message: &async_nats::Message) -> Option<async_nats::Subject> {
    let reply_to = message.headers.as_ref()?.get("reply_to")?.as_str();
    (!reply_to.is_empty() && !reply_to.starts_with("$JS.")).then(|| reply_to.into())
}

/// The durable consumer of a subject. A server refuses `.`, `*`, `>` and spaces in it.
fn durable_name(stream_name: &str, subject: &str) -> String {
    let subject: String = subject
        .chars()
        .map(|c| match c {
            '*' => "_any_".to_string(),
            '>' => "_all_".to_string(),
            c if c == '.' || c.is_whitespace() => "-".to_string(),
            c => c.to_string(),
        })
        .collect();
    format!("{}-{}-{}", APP_NAME, stream_name, subject)
}

async fn handle_jetstream_replies(
    client: &async_nats::Client,
    messages: &[async_nats::jetstream::Message],
    dispositions: &[MessageDisposition],
) {
    let mut sent_reply = false;
    for (msg, disposition) in messages.iter().zip(dispositions.iter()) {
        // Only a message that names a `reply_to` gets the response.
        if let Some(reply) = jetstream_reply_subject(msg).as_ref() {
            let payload = match disposition {
                MessageDisposition::Reply(resp) => Some(resp.payload.clone()),
                _ => None,
            };

            if let Some(p) = payload {
                let publish_result = tokio::time::timeout(
                    std::time::Duration::from_secs(60),
                    client.publish(reply.clone(), p),
                )
                .await;

                match publish_result {
                    Err(_) => {
                        tracing::error!(subject = %reply, "Failed to publish NATS reply (timeout)");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(subject = %reply, error = %e, "Failed to publish NATS reply");
                    }
                    Ok(Ok(_)) => {
                        sent_reply = true;
                    }
                }
            }
        }
    }

    if sent_reply {
        if let Err(error) = client.flush().await {
            tracing::error!(error = %error, "Failed to flush NATS JetStream replies");
        }
    }
}

/// An idle drain is only empty once the consumer has nothing undelivered and no unacked
/// messages besides ours: what a crashed run pulled comes back only after `ack_wait`,
/// so a drain started sooner waits for it, at most that long.
async fn drain_must_wait(
    consumer: &jetstream::consumer::Consumer<pull::Config>,
    held: &AtomicUsize,
    deadline: &mut Option<Instant>,
) -> bool {
    let info = match consumer.get_info().await {
        Ok(info) => info,
        Err(e) => {
            warn!(error = %e, "NATS drain could not read consumer info; treating idle as empty");
            return false;
        }
    };
    if info.num_pending == 0 && info.num_ack_pending <= held.load(Ordering::Acquire) {
        return false;
    }
    let ack_wait = match info.config.ack_wait {
        wait if wait.is_zero() => Duration::from_secs(30),
        wait => wait,
    };
    let deadline =
        *deadline.get_or_insert_with(|| Instant::now() + ack_wait + Duration::from_secs(2));
    if Instant::now() < deadline {
        trace!(
            num_pending = info.num_pending,
            num_ack_pending = info.num_ack_pending,
            "NATS drain idle, but the consumer still has messages; waiting"
        );
        return true;
    }
    warn!(
        num_pending = info.num_pending,
        num_ack_pending = info.num_ack_pending,
        "NATS drain ending with messages still pending after ack_wait"
    );
    false
}

async fn handle_jetstream_acks(
    messages: Vec<async_nats::jetstream::Message>,
    dispositions: Vec<MessageDisposition>,
) -> anyhow::Result<()> {
    let ack_futures =
        messages
            .into_iter()
            .zip(dispositions)
            .map(|(message, disposition)| async move {
                match disposition {
                    MessageDisposition::Ack | MessageDisposition::Reply(_) => message
                        .ack()
                        .await
                        .map_err(|e| anyhow!("Failed to ACK NATS message: {}", e)),
                    MessageDisposition::Nack => message
                        .ack_with(async_nats::jetstream::AckKind::Nak(None))
                        .await
                        .map_err(|e| anyhow!("Failed to NAK NATS message: {}", e)),
                }
            });

    let results: Vec<Result<(), anyhow::Error>> = futures::stream::iter(ack_futures)
        .buffer_unordered(100)
        .collect()
        .await;

    for res in results {
        if let Err(e) = res {
            tracing::error!(error = %e, "NATS JetStream ack failed");
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn nats_message(reply: Option<&str>, headers: Option<HeaderMap>) -> async_nats::Message {
        let payload = Bytes::from_static(b"payload");
        async_nats::Message {
            subject: async_nats::Subject::from_static("test.subject"),
            reply: reply.map(|reply| async_nats::Subject::from(reply.to_string())),
            length: payload.len(),
            payload,
            headers,
            status: None,
            description: None,
        }
    }

    #[test]
    fn core_native_reply_is_mapped_to_reply_to_metadata() {
        let message = nats_message(Some("_INBOX.reply"), None);

        let canonical = create_nats_canonical_message(&message, None, true, false);

        assert_eq!(
            canonical.metadata.get("reply_to").map(String::as_str),
            Some("_INBOX.reply")
        );
    }

    #[test]
    fn jetstream_ack_reply_is_not_mapped_to_reply_to_metadata() {
        let message = nats_message(Some("$JS.ACK.test-stream.consumer.1.1"), None);

        let canonical = create_nats_canonical_message(&message, Some(1), false, false);

        assert!(!canonical.metadata.contains_key("reply_to"));
    }

    #[test]
    fn jetstream_exposes_source_cursor_metadata() {
        let message = nats_message(None, None);

        let canonical = create_nats_canonical_message(&message, Some(42), false, true);

        assert_eq!(
            canonical
                .metadata
                .get("mqb.src.nats_subject")
                .map(String::as_str),
            Some("test.subject")
        );
        assert_eq!(
            canonical
                .metadata
                .get("mqb.src.nats_stream_sequence")
                .map(String::as_str),
            Some("42")
        );
        // Core NATS (no sequence) omits the sequence key.
        let core = create_nats_canonical_message(&message, None, false, true);
        assert!(!core.metadata.contains_key("mqb.src.nats_stream_sequence"));
        assert!(crate::canonical_message::is_source_metadata_key(
            "mqb.src.nats_subject"
        ));
    }

    #[test]
    fn inbound_source_metadata_header_cannot_spoof_cursor() {
        // An upstream producer sets a reserved `mqb.src.*` header. It must be
        // dropped, and the authoritative subject cursor must win.
        let mut headers = HeaderMap::new();
        headers.insert("mqb.src.kafka_offset", "999");
        headers.insert("mqb.src.nats_subject", "evil.subject");
        headers.insert("user_key", "kept");
        let message = nats_message(None, Some(headers));

        let canonical = create_nats_canonical_message(&message, Some(7), false, true);

        assert!(!canonical.metadata.contains_key("mqb.src.kafka_offset"));
        assert_eq!(
            canonical
                .metadata
                .get("mqb.src.nats_subject")
                .map(String::as_str),
            Some("test.subject"),
            "spoofed subject must be overwritten by the real one"
        );
        assert_eq!(
            canonical.metadata.get("user_key").map(String::as_str),
            Some("kept")
        );
    }

    #[test]
    fn jetstream_preserves_explicit_reply_to_header() {
        let mut headers = HeaderMap::new();
        headers.insert("reply_to", "app.reply.subject");
        let message = nats_message(Some("$JS.ACK.test-stream.consumer.1.1"), Some(headers));

        let canonical = create_nats_canonical_message(&message, Some(1), false, false);

        assert_eq!(
            canonical.metadata.get("reply_to").map(String::as_str),
            Some("app.reply.subject")
        );
    }

    #[test]
    fn core_native_reply_does_not_overwrite_explicit_reply_to_header() {
        let mut headers = HeaderMap::new();
        headers.insert("reply_to", "app.reply.subject");
        let message = nats_message(Some("_INBOX.native"), Some(headers));

        let canonical = create_nats_canonical_message(&message, None, true, false);

        assert_eq!(
            canonical.metadata.get("reply_to").map(String::as_str),
            Some("app.reply.subject")
        );
    }

    /// async-nats asserts on such a name or value, which took the route down.
    #[test]
    fn metadata_nats_cannot_carry_as_a_header_is_left_out() {
        let mut message = CanonicalMessage::new(b"payload".to_vec(), Some(1));
        for (key, value) in [
            ("content type", "a"),
            ("a:b", "a"),
            ("größe", "a"),
            ("", "a"),
            ("multiline", "one\ntwo"),
            ("kept", "value"),
        ] {
            message.metadata.insert(key.to_string(), value.to_string());
        }

        let headers = publish_headers(&message, false);

        assert_eq!(headers.get("kept").map(|v| v.as_str()), Some("value"));
        assert!(headers.get("mq_bridge.message_id").is_some());
        assert_eq!(headers.len(), 2);
    }

    /// The `reply` of a JetStream message is its ack subject, where a response body
    /// starting with `-NAK` or `+TERM` is an ack command.
    #[test]
    fn a_jetstream_reply_goes_to_the_reply_to_header_and_never_to_the_ack_subject() {
        let ack_subject = "$JS.ACK.test-stream.consumer.1.1";
        let without_header = nats_message(Some(ack_subject), None);
        assert_eq!(jetstream_reply_subject(&without_header), None);

        let mut headers = HeaderMap::new();
        headers.insert("reply_to", "app.reply.subject");
        let with_header = nats_message(Some(ack_subject), Some(headers));
        assert_eq!(
            jetstream_reply_subject(&with_header).as_deref(),
            Some("app.reply.subject")
        );
    }

    /// Without `require_tls` the client followed the server, and a server that does not
    /// ask for TLS got the password in the clear.
    #[tokio::test]
    async fn tls_required_holds_for_a_server_that_does_not_ask_for_tls() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        #[cfg(feature = "rustls-aws-lc")]
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        #[cfg(not(feature = "rustls-aws-lc"))]
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("nats://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let info = "INFO {\"server_id\":\"x\",\"server_name\":\"x\",\"host\":\"127.0.0.1\",\"port\":4222,\"version\":\"2.10.0\",\"go\":\"go\",\"proto\":1,\"headers\":true,\"max_payload\":1048576}\r\n";
            stream.write_all(info.as_bytes()).await.unwrap();
            let mut first = [0u8; 7];
            stream.read_exact(&mut first).await.unwrap();
            first
        });
        let config = NatsConfig {
            url,
            username: Some("user".to_string()),
            password: Some("secret".to_string()),
            tls: crate::models::TlsConfig {
                required: true,
                accept_invalid_certs: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let options = build_nats_options(&config).await.unwrap();
        let second = std::time::Duration::from_secs(5);
        let _ = tokio::time::timeout(second, options.connect(&config.url)).await;

        let first = tokio::time::timeout(second, server).await.unwrap().unwrap();
        assert_ne!(&first, b"CONNECT", "the client went on without TLS");
    }

    #[test]
    fn a_comma_separated_url_is_a_list_of_servers() {
        use async_nats::ToServerAddrs;

        let servers = server_list("nats://n1:4222, nats://n2:4223,");
        assert_eq!(servers, ["nats://n1:4222", "nats://n2:4223"]);
        let parsed: Vec<_> = servers.to_server_addrs().unwrap().collect();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].port(), 4223);

        assert_eq!(server_list("localhost:4222"), ["localhost:4222"]);
    }

    #[test]
    fn a_durable_name_has_no_character_the_server_refuses() {
        assert_eq!(
            durable_name("events", "events.orders"),
            format!("{APP_NAME}-events-events-orders")
        );
        for subject in ["events.>", "events.*.new", "events.a b"] {
            let name = durable_name("events", subject);
            assert!(
                !name.contains(['.', '*', '>', ' ']),
                "{subject} gave {name}"
            );
        }
        assert_ne!(
            durable_name("events", "events.>"),
            durable_name("events", "events.*")
        );
    }

    #[test]
    fn core_native_reply_true_with_no_reply_is_absent() {
        let message = nats_message(None, None);
        let canonical = create_nats_canonical_message(&message, None, true, false);
        assert!(!canonical.metadata.contains_key("reply_to"));
    }
}
