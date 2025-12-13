//  hot_queue
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/hot_queue

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::endpoints::memory::{get_or_create_channel, MemoryChannel};

/// The top-level configuration is a map of named routes.
/// The key is the route name (e.g., "kafka_to_nats").
pub type Config = HashMap<String, Route>;

/// Defines a single message processing route from an input to an output.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// (Optional) Number of concurrent processing tasks for this route. Defaults to 1.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// The input/source endpoint for the route.
    pub input: Endpoint,
    /// The output/sink endpoint for the route.
    pub output: Endpoint,
}

fn default_concurrency() -> usize {
    1
}

/// Represents a connection point for messages, which can be a source (input) or a sink (output).
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// (Optional) A list of middlewares to apply to the endpoint.
    #[serde(default)]
    pub middlewares: Middlewares,

    /// The specific endpoint implementation, determined by the configuration key (e.g., "kafka", "nats").
    #[serde(flatten)]
    pub endpoint_type: EndpointType,
}

impl Endpoint {
    pub fn new(endpoint_type: EndpointType) -> Self {
        Self {
            middlewares: Middlewares::default(),
            endpoint_type,
        }
    }
    pub fn channel(&self) -> anyhow::Result<MemoryChannel> {
        match &self.endpoint_type {
            EndpointType::Memory(cfg) => Ok(get_or_create_channel(&cfg)),
            _ => Err(anyhow::anyhow!("channel() called on non-memory Endpoint")),
        }
    }
}

/// An enumeration of all supported endpoint types.
/// `#[serde(rename_all = "lowercase")]` ensures that the keys in the config (e.g., "kafka")
/// match the enum variants.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EndpointType {
    Kafka(KafkaEndpoint),
    Nats(NatsEndpoint),
    File(String),
    Static(String),
    Memory(MemoryConfig),
    Amqp(AmqpEndpoint),
    MongoDb(MongoDbEndpoint),
    Mqtt(MqttEndpoint),
    Http(HttpEndpoint),
}

/// Configuration for middlewares applied to an endpoint.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Middlewares {
    #[serde(default)]
    pub deduplication: Option<DeduplicationMiddleware>,
    #[serde(default)]
    pub metrics: Option<MetricsMiddleware>,
    #[serde(default)]
    pub dlq: Option<DeadLetterQueueMiddleware>,
    // Other middlewares like retry can be added here.
}

/// Deduplication middleware configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeduplicationMiddleware {
    pub sled_path: String,
    pub ttl_seconds: u64,
}

/// Metrics middleware configuration. It's currently a struct without fields
/// but can be extended later. Its presence in the config enables the middleware.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsMiddleware {}

/// Dead-Letter Queue (DLQ) middleware configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeadLetterQueueMiddleware {
    #[serde(flatten)]
    pub endpoint: EndpointType,
}

// --- Kafka Specific Configuration ---

/// Kafka endpoint configuration, combining connection and topic details.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KafkaEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: KafkaConfig,
}

/// General Kafka connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct KafkaConfig {
    pub brokers: String,
    pub group_id: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>, // Consider using a secret management type
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub await_ack: bool,
    #[serde(default)]
    pub producer_options: Option<Vec<(String, String)>>,
    #[serde(default)]
    pub consumer_options: Option<Vec<(String, String)>>,
}

// --- NATS Specific Configuration ---

/// NATS endpoint configuration, combining connection and subject details.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NatsEndpoint {
    pub subject: Option<String>,
    pub stream: Option<String>,
    #[serde(flatten)]
    pub config: NatsConfig,
}

/// General NATS connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NatsConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    pub token: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub await_ack: bool,
    pub default_stream: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct MemoryConfig {
    pub topic: String,
    pub capacity: Option<usize>,
}

// --- AMQP Specific Configuration ---

/// AMQP endpoint configuration, combining connection and queue details.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AmqpEndpoint {
    pub queue: Option<String>,
    #[serde(flatten)]
    pub config: AmqpConfig,
}

/// General AMQP connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct AmqpConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub await_ack: bool,
}

// --- MongoDB Specific Configuration ---

/// MongoDB endpoint configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MongoDbEndpoint {
    pub collection: Option<String>,
    #[serde(flatten)]
    pub config: MongoDbConfig,
}

/// General MongoDB connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MongoDbConfig {
    pub url: String,
    pub database: String,
}

// --- MQTT Specific Configuration ---

/// MQTT endpoint configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MqttEndpoint {
    pub topic: Option<String>,
    #[serde(flatten)]
    pub config: MqttConfig,
}

/// General MQTT connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub url: String,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    pub queue_capacity: Option<usize>,
}

// --- HTTP Specific Configuration ---

/// HTTP endpoint configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpEndpoint {
    #[serde(flatten)]
    pub config: HttpConfig,
}

/// General HTTP connection configuration.
#[derive(Debug, Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub url: Option<String>,
    pub listen_address: Option<String>,
    #[serde(default)]
    pub tls: TlsConfig,
    pub response_sink: Option<String>,
}

// --- Common Configuration ---

/// TLS configuration for secure connections.
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub required: bool,
    pub ca_file: Option<String>,
    pub cert_file: Option<String>,
    pub key_file: Option<String>,
    pub cert_password: Option<String>,
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

impl TlsConfig {
    pub fn is_mtls_client_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }
    pub fn is_tls_server_configured(&self) -> bool {
        self.required && self.cert_file.is_some() && self.key_file.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::{Config as ConfigBuilder, Environment, File, FileFormat};

    const TEST_YAML: &str = r#"
kafka_to_nats:
  concurrency: 10
  input:
    middlewares:
      deduplication:
        sled_path: "/tmp/hot_queue/dedup_db"
        ttl_seconds: 3600
      metrics: {}
      dlq:
        nats:
          subject: "dlq-subject"
          url: "nats://localhost:4222"
    kafka:
      topic: "input-topic"
      brokers: "localhost:9092"
      group_id: "my-consumer-group"
      tls:
        required: true
        ca_file: "/path_to_ca"
        cert_file: "/path_to_cert"
        key_file: "/path_to_key"
        cert_password: "password"
        accept_invalid_certs: true
  output:
    nats:
      subject: "output-subject"
      url: "nats://localhost:4222"
"#;

    fn assert_config_values(config: &Config) {
        assert_eq!(config.len(), 1);
        let route = config.get("kafka_to_nats").expect("Route should exist");

        assert_eq!(route.concurrency, 10);

        // --- Assert Input ---
        let input = &route.input;
        assert!(input.middlewares.metrics.is_some());
        let dedup = input.middlewares.deduplication.as_ref().unwrap();
        assert_eq!(dedup.sled_path, "/tmp/hot_queue/dedup_db");
        assert_eq!(dedup.ttl_seconds, 3600);

        if let EndpointType::Nats(dlq_nats) = &input.middlewares.dlq.as_ref().unwrap().endpoint {
            assert_eq!(dlq_nats.subject, Some("dlq-subject".to_string()));
            assert_eq!(dlq_nats.config.url, "nats://localhost:4222");
        } else {
            panic!("DLQ endpoint should be NATS");
        }

        if let EndpointType::Kafka(kafka) = &input.endpoint_type {
            assert_eq!(kafka.topic, Some("input-topic".to_string()));
            assert_eq!(kafka.config.brokers, "localhost:9092");
            assert_eq!(kafka.config.group_id, Some("my-consumer-group".to_string()));
            let tls = &kafka.config.tls;
            assert_eq!(tls.required, true);
            assert_eq!(tls.ca_file.as_deref(), Some("/path_to_ca"));
            assert_eq!(tls.accept_invalid_certs, true);
        } else {
            panic!("Input endpoint should be Kafka");
        }

        // --- Assert Output ---
        let output = &route.output;
        if let EndpointType::Nats(nats) = &output.endpoint_type {
            assert_eq!(nats.subject, Some("output-subject".to_string()));
            assert_eq!(nats.config.url, "nats://localhost:4222");
        } else {
            panic!("Output endpoint should be NATS");
        }
    }

    #[test]
    fn test_deserialize_from_yaml() {
        let builder =
            ConfigBuilder::builder().add_source(File::from_str(TEST_YAML, FileFormat::Yaml));

        let config: Config = builder.build().unwrap().try_deserialize().unwrap();
        assert_config_values(&config);
    }

    #[test]
    fn test_deserialize_from_env() {
        // Set environment variables based on README
        unsafe {
            std::env::set_var("HQ__KAFKA_TO_NATS__CONCURRENCY", "10");
            std::env::set_var("HQ__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC", "input-topic");
            std::env::set_var("HQ__KAFKA_TO_NATS__INPUT__KAFKA__BROKERS", "localhost:9092");
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__INPUT__KAFKA__GROUP_ID",
                "my-consumer-group",
            );
            std::env::set_var("HQ__KAFKA_TO_NATS__INPUT__KAFKA__TLS__REQUIRED", "true");
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__INPUT__KAFKA__TLS__CA_FILE",
                "/path_to_ca",
            );
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__INPUT__KAFKA__TLS__ACCEPT_INVALID_CERTS",
                "true",
            );
            std::env::set_var("HQ__KAFKA_TO_NATS__OUTPUT__NATS__SUBJECT", "output-subject");
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__OUTPUT__NATS__URL",
                "nats://localhost:4222",
            );
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__INPUT__MIDDLEWARES__DLQ__NATS__SUBJECT",
                "dlq-subject",
            );
            std::env::set_var(
                "HQ__KAFKA_TO_NATS__INPUT__MIDDLEWARES__DLQ__NATS__URL",
                "nats://localhost:4222",
            );
        }

        let builder = ConfigBuilder::builder()
            // Enable automatic type parsing for values from environment variables.
            .add_source(
                Environment::with_prefix("HQ")
                    .separator("__")
                    .try_parsing(true),
            );

        let config: Config = builder
            .build()
            .expect("Failed to build config")
            .try_deserialize()
            .expect("Failed to deserialize config");

        // We can't test all values from env, but we can check the ones we set.
        assert_eq!(config.get("kafka_to_nats").unwrap().concurrency, 10);
        if let EndpointType::Kafka(k) = &config.get("kafka_to_nats").unwrap().input.endpoint_type {
            assert_eq!(k.topic, Some("input-topic".to_string()));
            assert!(k.config.tls.required);
        }
    }
}
