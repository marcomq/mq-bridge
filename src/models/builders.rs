//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Builder and convenience methods on the configuration structs — `new`, `with_*`,
//! and the small accessors that resolve a field to its effective value.

use super::*;

macro_rules! with_value_setters {
    ($config:ty { $($method:ident => $field:ident: $value:ty),* $(,)? }) => {
        impl $config {
            $(pub fn $method(mut self, value: $value) -> Self {
                self.$field = value;
                self
            })*
        }
    };
}

macro_rules! with_optional_setters {
    ($config:ty { $($method:ident => $field:ident: $value:ty),* $(,)? }) => {
        impl $config {
            $(pub fn $method(mut self, value: $value) -> Self {
                self.$field = Some(value);
                self
            })*
        }
    };
}

macro_rules! with_string_setters {
    ($config:ty { $($method:ident => $field:ident),* $(,)? }) => {
        impl $config {
            $(pub fn $method(mut self, value: impl Into<String>) -> Self {
                self.$field = value.into();
                self
            })*
        }
    };
}

macro_rules! with_optional_string_setters {
    ($config:ty { $($method:ident => $field:ident),* $(,)? }) => {
        impl $config {
            $(pub fn $method(mut self, value: impl Into<String>) -> Self {
                self.$field = Some(value.into());
                self
            })*
        }
    };
}

impl RouteOptions {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.concurrency == 0 {
            return Err(anyhow::anyhow!("route concurrency must be at least 1"));
        }
        if self.batch_size == 0 {
            return Err(anyhow::anyhow!("route batch_size must be at least 1"));
        }
        if self.commit_concurrency_limit == 0 {
            return Err(anyhow::anyhow!(
                "route commit_concurrency_limit must be at least 1"
            ));
        }
        Ok(())
    }
}

impl EndpointType {
    pub fn name(&self) -> &'static str {
        match self {
            EndpointType::Aws(_) => "aws",
            EndpointType::Kafka(_) => "kafka",
            EndpointType::Nats(_) => "nats",
            EndpointType::File(_) => "file",
            EndpointType::DirSpool(_) => "dir_spool",
            EndpointType::ObjectStore(_) => "object_store",
            EndpointType::Static(_) => "static",
            EndpointType::Ref(_) => "ref",
            EndpointType::Memory(_) => "memory",
            EndpointType::Sled(_) => "sled",
            EndpointType::Amqp(_) => "amqp",
            EndpointType::MongoDb(_) => "mongodb",
            EndpointType::Mqtt(_) => "mqtt",
            EndpointType::Http(_) => "http",
            EndpointType::WebSocket(_) => "websocket",
            EndpointType::IbmMq(_) => "ibmmq",
            EndpointType::ZeroMq(_) => "zeromq",
            EndpointType::RedisStreams(_) => "redis_streams",
            EndpointType::Grpc(_) => "grpc",
            EndpointType::Sqlx(_) => "sqlx",
            EndpointType::ClickHouse(_) => "clickhouse",
            EndpointType::PostgresCdc(_) => "postgres_cdc",
            EndpointType::Fanout(_) => "fanout",
            EndpointType::StreamBuffer(_) => "stream_buffer",
            EndpointType::Switch(_) => "switch",
            EndpointType::Response(_) => "response",
            EndpointType::Reader(_) => "reader",
            EndpointType::Request(_) => "request",
            EndpointType::Custom { .. } => "custom",
            EndpointType::Null => "null",
        }
    }

    pub fn is_core(&self) -> bool {
        matches!(
            self,
            EndpointType::File(_)
                | EndpointType::DirSpool(_)
                | EndpointType::Static(_)
                | EndpointType::Ref(_)
                | EndpointType::Memory(_)
                | EndpointType::Fanout(_)
                | EndpointType::StreamBuffer(_)
                | EndpointType::Switch(_)
                | EndpointType::Response(_)
                | EndpointType::Reader(_)
                | EndpointType::Request(_)
                | EndpointType::Custom { .. }
                | EndpointType::Null
        )
    }
}

impl MappingRule {
    /// The source path this rule reads from.
    pub fn path(&self) -> &str {
        match self {
            MappingRule::Path(p) => p,
            MappingRule::Detailed(d) => &d.path,
        }
    }
}

impl std::fmt::Display for FaultMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FaultMode::Panic => write!(f, "panic"),
            FaultMode::Disconnect => write!(f, "disconnect"),
            FaultMode::Timeout => write!(f, "timeout"),
            FaultMode::JsonFormatError => write!(f, "json_format_error"),
            FaultMode::Nack => write!(f, "nack"),
        }
    }
}

impl AwsConfig {
    /// Creates a new AWS configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_queue_url(mut self, queue_url: impl Into<String>) -> Self {
        self.queue_url = Some(queue_url.into());
        self
    }

    pub fn with_topic_arn(mut self, topic_arn: impl Into<String>) -> Self {
        self.topic_arn = Some(topic_arn.into());
        self
    }

    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    pub fn with_endpoint_url(mut self, endpoint_url: impl Into<String>) -> Self {
        self.endpoint_url = Some(endpoint_url.into());
        self
    }

    pub fn with_credentials(
        mut self,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> Self {
        self.access_key = Some(access_key.into());
        self.secret_key = Some(secret_key.into());
        self
    }
}

impl KafkaConfig {
    /// Creates a new Kafka configuration with the specified broker URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_group_id(mut self, group_id: impl Into<String>) -> Self {
        self.group_id = Some(group_id.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_producer_option(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let options = self.producer_options.get_or_insert_with(Vec::new);
        options.push((key.into(), value.into()));
        self
    }

    pub fn with_consumer_option(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let options = self.consumer_options.get_or_insert_with(Vec::new);
        options.push((key.into(), value.into()));
        self
    }
}

impl SledConfig {
    /// Creates a new Sled configuration with the specified database path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn with_tree(mut self, tree: impl Into<String>) -> Self {
        self.tree = Some(tree.into());
        self
    }

    pub fn with_read_from_start(mut self, read_from_start: bool) -> Self {
        self.read_from_start = read_from_start;
        self
    }
}

impl FileConfig {
    /// Creates a new File configuration with the specified path.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            name_by: NameBy::default(),
            idempotency: None,
            mode: Some(FileConsumerMode::default()),
            delimiter: None,
            format: FileFormat::default(),
            compression: Compression::default(),
            encryption: None,
            source_metadata: false,
        }
    }

    /// Naming scheme for this sink, with the deprecated `idempotency` alias folded in.
    ///
    /// `auto` never resolves to `source_position` here. The two schemes are different sink
    /// *structures* — one appended file versus a directory of part files — so deriving part
    /// files from the input would change what `path` means behind the operator's back. The
    /// object-store sink writes one object per batch either way, so only its name changes and
    /// it can safely auto-resolve. The unused argument keeps both sinks callable alike.
    pub fn resolved_name_by(&self, _source_has_position: bool) -> NameBy {
        match self.name_by.or_idempotency(self.idempotency) {
            NameBy::Auto => NameBy::WriteTime,
            explicit => explicit,
        }
    }

    pub fn with_mode(mut self, mode: FileConsumerMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Returns the effective consumer mode, defaulting to `Consume` if not set.
    pub fn effective_mode(&self) -> FileConsumerMode {
        self.mode.clone().unwrap_or_default()
    }
}

impl DirSpoolConfig {
    /// Creates a directory-spool configuration over `path`, with every other field at the
    /// default the deserializer would have applied.
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            naming_pattern: default_spool_naming_pattern(),
            shard_depth: 0,
            shard_width: default_spool_shard_width(),
            payload_extension: default_spool_payload_extension(),
            metadata_extension: default_spool_metadata_extension(),
            atomic: true,
            fsync: SpoolFsync::default(),
            done_file: default_spool_done_file(),
            emit_done: SpoolDone::default(),
            producer_file: default_spool_producer_file(),
            consumer_file: default_spool_consumer_file(),
            drain_on_read: true,
            stop_on_done: false,
            poll_interval_ms: default_spool_poll_interval_ms(),
            source_metadata: false,
            claim: crate::models::SpoolClaim::default(),
        }
    }

    /// The payload extension without its leading dot, e.g. `bin`.
    pub fn payload_suffix(&self) -> &str {
        self.payload_extension.trim_start_matches('.')
    }

    /// The metadata sidecar extension without its leading dot, or `None` when sidecars are
    /// disabled (an empty `metadata_extension`).
    pub fn metadata_suffix(&self) -> Option<&str> {
        let suffix = self.metadata_extension.trim_start_matches('.');
        (!suffix.is_empty()).then_some(suffix)
    }
}

impl ObjectStoreConfig {
    /// Creates a new object-store configuration for the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    /// Naming scheme for this sink, with the deprecated `idempotency` alias folded in and
    /// `auto` resolved against the input.
    pub fn resolved_name_by(&self, source_has_position: bool) -> NameBy {
        self.name_by
            .or_idempotency(self.idempotency)
            .resolve(source_has_position)
    }

    /// Whether to prefix object keys with `YYYY/MM/DD/`. Unset means on, and the date layout
    /// only applies to `write_time` naming — a source-range name is written flat.
    pub fn date_partition_enabled(&self, name_by: NameBy) -> bool {
        name_by != NameBy::SourcePosition && self.date_partition.unwrap_or(true)
    }

    pub fn with_checkpoint(
        mut self,
        checkpoint_store: impl Into<String>,
        cursor_id: impl Into<String>,
    ) -> Self {
        self.checkpoint_store = Some(checkpoint_store.into());
        self.cursor_id = Some(cursor_id.into());
        self
    }
}

impl NatsConfig {
    /// Creates a new NATS configuration with the specified server URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn with_stream(mut self, stream: impl Into<String>) -> Self {
        self.stream = Some(stream.into());
        self
    }

    pub fn with_deliver_policy(mut self, policy: NatsDeliverPolicy) -> Self {
        self.deliver_policy = Some(policy);
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl MemoryConfig {
    pub fn new(topic: impl Into<String>, capacity: Option<usize>) -> Self {
        Self {
            topic: topic.into(),
            url: None,
            capacity,
            ..Default::default()
        }
    }

    pub fn new_with_url(url: impl Into<String>, capacity: Option<usize>) -> Self {
        let url = url.into();
        Self {
            topic: url.clone(),
            url: Some(url),
            capacity,
            ..Default::default()
        }
    }

    pub fn with_subscribe(self, subscribe_mode: bool) -> Self {
        Self {
            subscribe_mode,
            ..self
        }
    }

    pub fn with_request_reply(mut self, request_reply: bool) -> Self {
        self.request_reply = request_reply;
        self
    }

    /// Gets the effective transport identifier.
    /// If topic contains ://, it's treated as a URL, otherwise as memory://topic.
    pub fn get_transport_identifier(&self) -> anyhow::Result<String> {
        let identifier = if !self.topic.is_empty() {
            &self.topic
        } else if let Some(url) = self.url.as_ref().filter(|url| !url.is_empty()) {
            url
        } else {
            return Err(anyhow::anyhow!(
                "MemoryConfig: 'topic' (or 'url' alias) is required."
            ));
        };

        // If topic doesn't contain ://, treat it as memory://topic for backward compatibility
        if identifier.contains("://") {
            Ok(identifier.clone())
        } else {
            Ok(format!("memory://{}", identifier))
        }
    }

    /// Check if the transport URL scheme suggests IPC (inter-process communication).
    /// IPC transports should enable nack by default for reliability.
    pub fn is_ipc_transport(&self) -> bool {
        if let Ok(identifier) = self.get_transport_identifier() {
            identifier.starts_with("ipc://")
                || identifier.starts_with("unix://")
                || identifier.starts_with("pipe://")
        } else {
            false
        }
    }

    /// Apply smart defaults based on the transport type.
    /// For IPC transports, enable_nack defaults to true for reliability.
    pub fn with_smart_defaults(mut self) -> Self {
        if !self.enable_nack_overridden && self.is_ipc_transport() {
            self.enable_nack = true;
        }
        self
    }
}

impl StreamBufferConfig {
    /// Creates a `stream_buffer` config for the given topic.
    ///
    /// Add `with_correlation_id` when constructing a consumer for one stream.
    /// Leave the correlation id unset when constructing the publisher buffer
    /// used by `HttpConfig::stream_response_to`.
    pub fn new(topic: impl Into<String>) -> Self {
        Self {
            topic: topic.into(),
            ..Default::default()
        }
    }

    /// Selects the response stream partition that a consumer should read.
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }

    /// Sets the per-correlation partition capacity.
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.capacity = Some(capacity);
        self
    }
}

impl AmqpConfig {
    /// Creates a new AMQP configuration with the specified connection URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    pub fn with_exchange(mut self, exchange: impl Into<String>) -> Self {
        self.exchange = Some(exchange.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl MongoDbConfig {
    /// Creates a new MongoDB configuration with the specified URL and database name.
    pub fn new(url: impl Into<String>, database: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            database: database.into(),
            ..Default::default()
        }
    }

    pub fn with_collection(mut self, collection: impl Into<String>) -> Self {
        self.collection = Some(collection.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn with_change_stream(mut self, change_stream: bool) -> Self {
        self.change_stream = change_stream;
        self
    }

    /// The effective consume mode: the explicit `consume` field if set, otherwise derived from the
    /// deprecated `change_stream` boolean.
    pub fn resolved_consume(&self) -> MongoConsume {
        if let Some(mode) = self.consume {
            return mode;
        }
        if self.change_stream {
            MongoConsume::CaptureNew
        } else {
            MongoConsume::default()
        }
    }
}

impl MqttConfig {
    /// Creates a new MQTT configuration with the specified broker URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl ZeroMqConfig {
    /// Creates a new ZeroMQ configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_socket_type(mut self, socket_type: ZeroMqSocketType) -> Self {
        self.socket_type = Some(socket_type);
        self
    }

    pub fn with_bind(mut self, bind: bool) -> Self {
        self.bind = bind;
        self
    }
}

impl RedisStreamsConfig {
    /// Creates a new Redis Streams configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_stream(mut self, stream: impl Into<String>) -> Self {
        self.stream = Some(stream.into());
        self
    }

    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn with_subscriber(mut self, subscriber: bool) -> Self {
        self.subscriber_mode = subscriber;
        self
    }

    pub fn with_reader_connections(mut self, connections: usize) -> Self {
        self.reader_connections = Some(connections);
        self
    }
}

impl GrpcConfig {
    /// Creates a new gRPC configuration with the specified server URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    /// Enable or disable server mode for this gRPC endpoint.
    pub fn with_server_mode(mut self, server_mode: bool) -> Self {
        self.server_mode = server_mode;
        self
    }
}

impl HttpConfig {
    /// Creates a new HTTP configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_workers(mut self, workers: usize) -> Self {
        self.workers = Some(workers);
        self
    }

    pub fn with_method(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_receive_streamable(mut self, receive_streamable: bool) -> Self {
        self.receive_streamable = receive_streamable;
        self
    }

    pub fn with_inline_response_fast_path(mut self, inline_response_fast_path: bool) -> Self {
        self.inline_response_fast_path = Some(inline_response_fast_path);
        self
    }

    pub fn with_server_protocol(mut self, server_protocol: HttpServerProtocol) -> Self {
        self.server_protocol = server_protocol;
        self
    }

    pub fn inline_response_fast_path_enabled(&self) -> bool {
        self.inline_response_fast_path.unwrap_or(true)
    }

    /// Request-body codec for a publisher: explicit `compression`, else gzip when
    /// `compression_enabled`, else none.
    pub fn publisher_compression(&self) -> Compression {
        match self.compression {
            Compression::None if self.compression_enabled == Some(true) => Compression::Gzip,
            other => other,
        }
    }

    /// Whether a consumer compresses responses (then it negotiates the best codec the client
    /// accepts). Driven by `compression_enabled`; the publisher-only `compression` codec is ignored.
    pub fn consumer_compression_enabled(&self) -> bool {
        self.compression_enabled == Some(true)
    }

    pub fn with_stream_response_to(mut self, endpoint: Endpoint) -> Self {
        self.stream_response_to = Some(Box::new(endpoint));
        self
    }
}

impl WebSocketConfig {
    /// Creates a new WebSocket configuration with the specified URL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            ..Default::default()
        }
    }

    pub fn with_path(mut self, path: impl Into<String>) -> Self {
        self.path = Some(path.into());
        self
    }

    pub fn with_backlog(mut self, backlog: u32) -> Self {
        self.backlog = Some(backlog);
        self
    }

    pub fn with_execution_mode(mut self, execution_mode: WebSocketExecutionMode) -> Self {
        self.execution_mode = execution_mode;
        self
    }
}

impl IbmMqConfig {
    /// Creates a new IBM MQ configuration with the specified connection URL, queue manager, and channel.
    pub fn new(
        url: impl Into<String>,
        queue_manager: impl Into<String>,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            queue_manager: queue_manager.into(),
            channel: channel.into(),
            disable_status_inq: false,
            ..Default::default()
        }
    }

    pub fn with_queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    pub fn with_topic(mut self, topic: impl Into<String>) -> Self {
        self.topic = Some(topic.into());
        self
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl PostgresCdcConfig {
    /// Creates a new Postgres CDC configuration for the specified publication.
    pub fn new(url: impl Into<String>, publication: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            publication: publication.into(),
            slot_name: default_pg_cdc_slot(),
            create_slot: true,
            status_interval_ms: default_pg_cdc_status_interval_ms(),
            ..Default::default()
        }
    }

    pub fn with_slot(mut self, slot_name: impl Into<String>) -> Self {
        self.slot_name = slot_name.into();
        self
    }

    pub fn with_checkpoint_store(mut self, checkpoint_store: impl Into<String>) -> Self {
        self.checkpoint_store = Some(checkpoint_store.into());
        self
    }
}

impl SqlxConfig {
    /// Creates a new SQLx configuration for the specified table.
    pub fn new(url: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            table: table.into(),
            ..Default::default()
        }
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl ClickHouseConfig {
    /// Creates a new ClickHouse configuration for the specified table.
    pub fn new(url: impl Into<String>, table: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            table: table.into(),
            compression: default_gzip_compression(),
            ..Default::default()
        }
    }

    pub fn with_credentials(
        mut self,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

impl TlsConfig {
    /// Creates a new TLS configuration with default settings (TLS not required).
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_ca_file(mut self, ca_file: impl Into<String>) -> Self {
        self.ca_file = Some(ca_file.into());
        self.required = true;
        self
    }

    pub fn with_client_cert(
        mut self,
        cert_file: impl Into<String>,
        key_file: impl Into<String>,
    ) -> Self {
        self.cert_file = Some(cert_file.into());
        self.key_file = Some(key_file.into());
        self.required = true;
        self
    }

    pub fn with_insecure(mut self, accept_invalid_certs: bool) -> Self {
        self.accept_invalid_certs = accept_invalid_certs;
        self
    }

    /// Checks if mutual TLS (mTLS) client authentication is configured.
    pub fn is_mtls_client_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }

    /// Checks if TLS server certificate authentication is configured.
    pub fn is_tls_server_configured(&self) -> bool {
        self.is_mtls_client_configured()
    }

    /// Checks if the TLS configuration is sufficient to make a TLS client connection.
    pub fn is_tls_client_configured(&self) -> bool {
        self.required
            || self.ca_file.is_some()
            || (self.cert_file.is_some() && self.key_file.is_some())
    }

    /// Helper to normalize a URL by adding the appropriate scheme prefix (http:// or https://) if missing.
    pub fn normalize_url(&self, url: &str) -> String {
        if url
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
            || url
                .get(..8)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        {
            url.to_string()
        } else {
            let is_tls = self.required;
            let scheme = if is_tls { "https" } else { "http" };
            format!("{}://{}", scheme, url)
        }
    }
}

// Straightforward fluent setters which do not need custom validation or side effects.
with_string_setters!(RouteOptions { with_description => description });
with_value_setters!(RouteOptions { with_concurrency => concurrency: usize, with_batch_size => batch_size: usize, with_commit_concurrency_limit => commit_concurrency_limit: usize, with_startup_timeout_ms => startup_timeout_ms: u64, with_reconnect_interval_ms => reconnect_interval_ms: u64, with_empty_batch_delay_ms => empty_batch_delay_ms: u64, with_allow_fault_injection => allow_fault_injection: bool, with_exit_on_empty => exit_on_empty: bool });
with_optional_string_setters!(DeduplicationMiddleware { with_store => store, with_sled_path => sled_path, with_key => key });
with_value_setters!(DeduplicationMiddleware { with_ttl_seconds => ttl_seconds: u64 });
with_value_setters!(RetryMiddleware { with_max_attempts => max_attempts: usize, with_initial_interval_ms => initial_interval_ms: u64, with_max_interval_ms => max_interval_ms: u64, with_multiplier => multiplier: f64 });
with_value_setters!(DelayMiddleware { with_delay_ms => delay_ms: u64 });
with_value_setters!(LimiterMiddleware { with_messages_per_second => messages_per_second: f64 });
with_value_setters!(BufferMiddleware { with_max_messages => max_messages: usize, with_max_delay_ms => max_delay_ms: u64 });
with_string_setters!(CookieJarMiddleware { with_cookie_metadata_key => cookie_metadata_key, with_set_cookie_metadata_key => set_cookie_metadata_key });
with_optional_string_setters!(CookieJarMiddleware { with_shared_scope => shared_scope, with_export_metadata_prefix => export_metadata_prefix });
with_value_setters!(CookieJarMiddleware { with_capture_metadata_keys => capture_metadata_keys: Vec<String>, with_inject_metadata => inject_metadata: HashMap<String, String> });
with_string_setters!(WeakJoinMiddleware { with_group_by => group_by });
with_optional_string_setters!(WeakJoinMiddleware { with_branch_by => branch_by });
with_value_setters!(WeakJoinMiddleware { with_expected_count => expected_count: usize, with_timeout_ms => timeout_ms: u64, with_required => required: Vec<String>, with_on_timeout => on_timeout: WeakJoinTimeout });
with_value_setters!(TransformMiddleware { with_mapping => mapping: HashMap<String, MappingRule>, with_coerce => coerce: bool, with_apply_defaults => apply_defaults: bool, with_coerce_empty_as_null => coerce_empty_as_null: bool, with_on_error => on_error: TransformErrorPolicy });
with_optional_setters!(TransformMiddleware { with_schema => schema: serde_json::Value });
with_optional_string_setters!(TransformMiddleware { with_schema_file => schema_file });
with_value_setters!(RandomPanicMiddleware { with_mode => mode: FaultMode, with_enabled => enabled: bool });
with_optional_setters!(RandomPanicMiddleware { with_trigger_on_message => trigger_on_message: usize });
with_value_setters!(CompressionMiddleware { with_algorithm => algorithm: Compression });
with_optional_setters!(CompressionMiddleware { with_max_decompressed_bytes => max_decompressed_bytes: u64 });

with_value_setters!(StaticConfig { with_raw => raw: bool, with_metadata => metadata: std::collections::HashMap<String, String> });
with_string_setters!(StaticConfig { with_body => body });
with_value_setters!(EncryptionConfig { with_cipher => cipher: CipherKind, with_decrypt_keys => decrypt_keys: HashMap<String, String> });
with_string_setters!(EncryptionConfig { with_key_id => key_id, with_key => key });

with_optional_setters!(AwsConfig { with_max_messages => max_messages: i32, with_wait_time_seconds => wait_time_seconds: i32 });
with_value_setters!(AwsConfig { with_binary_payload_mode => binary_payload_mode: bool });
with_value_setters!(KafkaConfig { with_tls => tls: TlsConfig, with_source_metadata => source_metadata: bool, with_delayed_ack => delayed_ack: bool });
with_optional_setters!(KafkaConfig { with_shared => shared: bool, with_partitions => partitions: i32 });
with_optional_setters!(KafkaConfig { with_producer_options => producer_options: Vec<(String, String)>, with_consumer_options => consumer_options: Vec<(String, String)> });
with_optional_string_setters!(KafkaConfig { with_partition_key => partition_key });
with_value_setters!(SledConfig { with_delete_after_read => delete_after_read: bool });
with_value_setters!(FileConfig { with_name_by => name_by: NameBy, with_format => format: FileFormat, with_compression => compression: Compression });
with_optional_setters!(FileConfig { with_idempotency => idempotency: bool, with_encryption => encryption: EncryptionConfig });
with_optional_string_setters!(FileConfig { with_delimiter => delimiter });

with_value_setters!(ObjectStoreConfig { with_name_by => name_by: NameBy, with_format => format: FileFormat, with_compression => compression: Compression });
with_optional_setters!(ObjectStoreConfig { with_idempotency => idempotency: bool, with_date_partition => date_partition: bool });
with_optional_setters!(ObjectStoreConfig { with_polling_interval_ms => polling_interval_ms: u64, with_max_object_bytes => max_object_bytes: u64, with_encryption => encryption: EncryptionConfig });
with_optional_string_setters!(ObjectStoreConfig { with_delimiter => delimiter, with_checkpoint_store => checkpoint_store, with_cursor_id => cursor_id, with_extension => extension });

with_value_setters!(NatsConfig { with_tls => tls: TlsConfig, with_request_reply => request_reply: bool, with_delayed_ack => delayed_ack: bool, with_deduplicate => deduplicate: bool, with_no_jetstream => no_jetstream: bool, with_subscriber_mode => subscriber_mode: bool, with_source_metadata => source_metadata: bool });
with_optional_setters!(NatsConfig { with_request_timeout_ms => request_timeout_ms: u64, with_stream_max_messages => stream_max_messages: i64, with_stream_max_bytes => stream_max_bytes: i64, with_prefetch_count => prefetch_count: usize, with_shared => shared: bool });
with_optional_string_setters!(NatsConfig { with_token => token });
with_optional_setters!(MemoryConfig { with_request_timeout_ms => request_timeout_ms: u64 });
with_value_setters!(MemoryConfig { with_enable_nack => enable_nack: bool });

with_value_setters!(AmqpConfig { with_subscribe_mode => subscribe_mode: bool, with_source_metadata => source_metadata: bool, with_tls => tls: TlsConfig, with_no_persistence => no_persistence: bool, with_no_declare_queue => no_declare_queue: bool, with_delayed_ack => delayed_ack: bool });
with_optional_setters!(AmqpConfig { with_prefetch_count => prefetch_count: u16 });
with_value_setters!(MongoDbConfig { with_tls => tls: TlsConfig, with_request_reply => request_reply: bool, with_source_metadata => source_metadata: bool, with_format => format: MongoDbFormat, with_report_outcome => report_outcome: bool });
with_optional_setters!(MongoDbConfig { with_polling_interval_ms => polling_interval_ms: u64, with_reply_polling_ms => reply_polling_ms: u64, with_consume => consume: MongoConsume, with_request_timeout_ms => request_timeout_ms: u64, with_ttl_seconds => ttl_seconds: u64, with_capped_size_bytes => capped_size_bytes: i64, with_shared => shared: bool });
with_optional_string_setters!(MongoDbConfig { with_receive_query => receive_query, with_checkpoint_store => checkpoint_store, with_id_field => id_field, with_cursor_id => cursor_id, with_meta_collection => meta_collection });

with_value_setters!(MqttConfig { with_tls => tls: TlsConfig, with_clean_session => clean_session: bool, with_protocol => protocol: MqttProtocol, with_delayed_ack => delayed_ack: bool });
with_optional_setters!(MqttConfig { with_queue_capacity => queue_capacity: usize, with_max_inflight => max_inflight: u16, with_qos => qos: u8, with_keep_alive_seconds => keep_alive_seconds: u64, with_session_expiry_interval => session_expiry_interval: u32 });
with_optional_string_setters!(ZeroMqConfig { with_topic => topic });
with_optional_setters!(ZeroMqConfig { with_internal_buffer_size => internal_buffer_size: usize, with_request_timeout_ms => request_timeout_ms: u64 });
with_value_setters!(ZeroMqConfig { with_format => format: ZeroMqFormat, with_backend => backend: ZeroMqBackend });
with_optional_string_setters!(RedisStreamsConfig { with_consumer_name => consumer_name, with_username => username, with_password => password });
with_value_setters!(RedisStreamsConfig { with_subscriber_mode => subscriber_mode: bool, with_read_from_start => read_from_start: bool });
with_optional_setters!(RedisStreamsConfig { with_block_ms => block_ms: u64, with_redelivery_timeout_ms => redelivery_timeout_ms: u64, with_maxlen => maxlen: usize, with_approx_trim => approx_trim: bool, with_internal_buffer_size => internal_buffer_size: usize });

with_value_setters!(GrpcConfig { with_tls => tls: TlsConfig, with_server_streaming => server_streaming: bool, with_reflection => reflection: bool, with_metadata => metadata: HashMap<String, String>, with_binary_metadata => binary_metadata: HashMap<String, Vec<u8>> });
with_optional_setters!(GrpcConfig { with_timeout_ms => timeout_ms: u64, with_connect_timeout_ms => connect_timeout_ms: u64, with_request_timeout_ms => request_timeout_ms: u64, with_idle_stream_timeout_ms => idle_stream_timeout_ms: u64, with_overall_timeout_ms => overall_timeout_ms: u64, with_initial_stream_window_size => initial_stream_window_size: u32, with_initial_connection_window_size => initial_connection_window_size: u32, with_concurrency_limit_per_connection => concurrency_limit_per_connection: usize, with_http2_keepalive_interval_ms => http2_keepalive_interval_ms: u64, with_http2_keepalive_timeout_ms => http2_keepalive_timeout_ms: u64, with_max_decoding_message_size => max_decoding_message_size: usize, with_max_encoding_message_size => max_encoding_message_size: usize, with_descriptor_set_bytes => descriptor_set_bytes: Vec<u8>, with_request => request: serde_json::Value, with_shared => shared: bool });
with_optional_string_setters!(GrpcConfig { with_consumer_id => consumer_id, with_descriptor_set_path => descriptor_set_path, with_service_name => service_name, with_method_name => method_name, with_bearer_token => bearer_token, with_api_key => api_key, with_api_key_name => api_key_name });
with_value_setters!(HttpConfig { with_tls => tls: TlsConfig, with_fire_and_forget => fire_and_forget: bool, with_pass_through_status => pass_through_status: bool, with_compression => compression: Compression, with_custom_headers => custom_headers: HashMap<String, String> });
with_optional_setters!(HttpConfig { with_request_timeout_ms => request_timeout_ms: u64, with_internal_buffer_size => internal_buffer_size: usize, with_batch_concurrency => batch_concurrency: usize, with_tcp_keepalive_ms => tcp_keepalive_ms: u64, with_pool_idle_timeout_ms => pool_idle_timeout_ms: u64, with_compression_enabled => compression_enabled: bool, with_compression_threshold_bytes => compression_threshold_bytes: usize, with_concurrency_limit => concurrency_limit: usize, with_basic_auth => basic_auth: (String, String), with_shared => shared: bool });
with_optional_string_setters!(HttpConfig { with_message_id_header => message_id_header });
with_optional_string_setters!(WebSocketConfig { with_message_id_header => message_id_header });
with_optional_setters!(WebSocketConfig { with_routed_queue_capacity => routed_queue_capacity: usize });

with_value_setters!(IbmTlsConfig { with_required => required: bool, with_accept_invalid_certs => accept_invalid_certs: bool });
with_optional_string_setters!(IbmTlsConfig { with_cipher_spec => cipher_spec, with_key_repository => key_repository, with_key_repository_password => key_repository_password });
with_value_setters!(IbmMqConfig { with_tls => tls: IbmTlsConfig, with_max_message_size => max_message_size: usize, with_wait_timeout_ms => wait_timeout_ms: i32, with_disable_status_inq => disable_status_inq: bool });
with_optional_setters!(IbmMqConfig { with_internal_buffer_size => internal_buffer_size: usize });
with_string_setters!(SwitchConfig { with_metadata_key => metadata_key });
with_value_setters!(SwitchConfig { with_cases => cases: HashMap<String, Endpoint>, with_when => when: Vec<SwitchCase> });
with_optional_setters!(SwitchConfig { with_default => default: Box<Endpoint> });
with_value_setters!(RequestForwardConfig { with_to => to: Box<Endpoint>, with_forward_to => forward_to: Box<Endpoint> });

with_value_setters!(PostgresCdcConfig { with_source_metadata => source_metadata: bool, with_create_slot => create_slot: bool, with_create_publication => create_publication: bool, with_publication_tables => publication_tables: Vec<String>, with_temporary_slot => temporary_slot: bool, with_status_interval_ms => status_interval_ms: u64, with_tls => tls: TlsConfig });
with_optional_string_setters!(PostgresCdcConfig { with_cursor_id => cursor_id });
with_value_setters!(SqlxConfig { with_delete_after_read => delete_after_read: bool, with_auto_create_table => auto_create_table: bool, with_bulk_copy => bulk_copy: bool, with_create_publication => create_publication: bool, with_tls => tls: TlsConfig });
with_optional_setters!(SqlxConfig { with_polling_interval_ms => polling_interval_ms: u64, with_max_polling_interval_ms => max_polling_interval_ms: u64, with_max_connections => max_connections: u32, with_min_connections => min_connections: u32, with_acquire_timeout_ms => acquire_timeout_ms: u64, with_idle_timeout_ms => idle_timeout_ms: u64, with_max_lifetime_ms => max_lifetime_ms: u64, with_test_before_acquire => test_before_acquire: bool, with_shared => shared: bool });
with_optional_string_setters!(SqlxConfig { with_insert_query => insert_query, with_select_query => select_query, with_cursor_column => cursor_column, with_cursor_id => cursor_id, with_checkpoint_store => checkpoint_store, with_publication => publication, with_slot_name => slot_name });
with_optional_string_setters!(ClickHouseConfig { with_database => database, with_cursor_column => cursor_column, with_cursor_id => cursor_id, with_checkpoint_store => checkpoint_store, with_select_columns => select_columns });
with_value_setters!(ClickHouseConfig { with_async_insert => async_insert: bool, with_tls => tls: TlsConfig, with_compression => compression: Compression });
with_optional_setters!(ClickHouseConfig { with_columns => columns: std::collections::BTreeMap<String, String>, with_wait_for_async_insert => wait_for_async_insert: bool, with_polling_interval_ms => polling_interval_ms: u64, with_max_polling_interval_ms => max_polling_interval_ms: u64, with_request_timeout_ms => request_timeout_ms: u64, with_connect_timeout_ms => connect_timeout_ms: u64 });
with_value_setters!(TlsConfig { with_required => required: bool, with_accept_invalid_certs => accept_invalid_certs: bool });
with_optional_string_setters!(TlsConfig { with_cert_password => cert_password });
