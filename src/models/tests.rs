//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Unit tests for the configuration models.

#[allow(unused_imports)]
use super::*;

mod null_endpoint_tests {
    use super::*;

    #[test]
    fn null_endpoint_json_round_trip() {
        let value = serde_json::to_value(Endpoint::null()).expect("serialize");
        let back: Endpoint = serde_json::from_value(value).expect("deserialize");
        assert!(matches!(back.endpoint_type, EndpointType::Null));
    }

    /// The schema advertises unit variants as bare strings, so `"null"` must parse.
    #[test]
    fn null_endpoint_accepts_string_and_unit_forms() {
        for input in ["\"null\"", "null", "{}"] {
            let endpoint: Endpoint = serde_json::from_str(input).unwrap_or_else(|e| {
                panic!("failed to parse {input}: {e}");
            });
            assert!(matches!(endpoint.endpoint_type, EndpointType::Null));
        }
    }

    #[test]
    fn unknown_endpoint_string_is_rejected() {
        let err = serde_json::from_str::<Endpoint>("\"kafka\"").expect_err("should fail");
        assert!(
            err.to_string().contains("unknown variant"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn nested_null_endpoint_json_round_trip() {
        let config =
            HttpConfig::new("http://localhost:8080").with_stream_response_to(Endpoint::null());
        let value = serde_json::to_value(&config).expect("serialize");
        let back: HttpConfig = serde_json::from_value(value).expect("deserialize");
        let nested = back.stream_response_to.expect("stream_response_to present");
        assert!(matches!(nested.endpoint_type, EndpointType::Null));
    }

    #[test]
    fn nested_null_endpoint_yaml_forms() {
        for yaml in [
            "url: http://localhost:8080\nstream_response_to: \"null\"\n",
            "url: http://localhost:8080\nstream_response_to: {}\n",
        ] {
            let config: HttpConfig = serde_yaml_ng::from_str(yaml)
                .unwrap_or_else(|e| panic!("failed to parse {yaml:?}: {e}"));
            let nested = config
                .stream_response_to
                .expect("stream_response_to present");
            assert!(matches!(nested.endpoint_type, EndpointType::Null));
        }
    }

    /// In an `Option<Box<Endpoint>>` field, a bare `null` is consumed by serde's `Option`
    /// layer as `None` and never reaches the endpoint visitor.
    #[test]
    fn nested_bare_null_yaml_is_none() {
        let config: HttpConfig =
            serde_yaml_ng::from_str("url: http://localhost:8080\nstream_response_to: null\n")
                .expect("deserialize");
        assert!(config.stream_response_to.is_none());
    }

    #[test]
    fn null_endpoint_yaml_round_trip() {
        let yaml = serde_yaml_ng::to_string(&Endpoint::null()).expect("serialize");
        let back: Endpoint = serde_yaml_ng::from_str(&yaml).expect("deserialize");
        assert!(matches!(back.endpoint_type, EndpointType::Null));
    }

    #[test]
    fn source_metadata_is_a_source_configuration_option() {
        let endpoint: Endpoint =
            serde_yaml_ng::from_str("kafka:\n  url: localhost:9092\n  source_metadata: true\n")
                .expect("deserialize Kafka source metadata option");
        let EndpointType::Kafka(config) = endpoint.endpoint_type else {
            panic!("expected Kafka endpoint");
        };
        assert!(config.source_metadata);
    }
}

mod config_tests {
    use super::*;
    use config::{Config as ConfigBuilder, Environment};

    /// Defaults must still fire for a middleware assembled from flattened `MQB__…`
    /// environment keys, where the config crate synthesizes the nested tables and
    /// every leaf arrives as a string.
    #[test]
    fn pack_defaults_survive_env_flattened_config() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const VARS: [&str; 3] = [
            "MQB__ENVPACK__INPUT__MEMORY__TOPIC",
            "MQB__ENVPACK__OUTPUT__MEMORY__TOPIC",
            "MQB__ENVPACK__OUTPUT__MIDDLEWARES__0__PACK__FORMAT",
        ];
        struct EnvCleanup(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for EnvCleanup {
            fn drop(&mut self) {
                unsafe {
                    for (name, previous) in self.0.drain(..) {
                        match previous {
                            Some(value) => std::env::set_var(name, value),
                            None => std::env::remove_var(name),
                        }
                    }
                }
            }
        }
        let _cleanup = EnvCleanup(
            VARS.into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect(),
        );

        unsafe {
            std::env::set_var("MQB__ENVPACK__INPUT__MEMORY__TOPIC", "in");
            std::env::set_var("MQB__ENVPACK__OUTPUT__MEMORY__TOPIC", "out");
            // Only `format` is given; every other pack field must come from its default.
            std::env::set_var(
                "MQB__ENVPACK__OUTPUT__MIDDLEWARES__0__PACK__FORMAT",
                "benthos_binary",
            );
        }

        let config: Config = ConfigBuilder::builder()
            .add_source(
                Environment::with_prefix("MQB")
                    .separator("__")
                    .try_parsing(true),
            )
            .build()
            .expect("config builds")
            .try_deserialize()
            .expect("config deserializes");

        let route = config.get("envpack").expect("route exists");
        let Middleware::Pack(cfg) = &route.output.middlewares[0] else {
            panic!("expected pack, got {:?}", route.output.middlewares);
        };
        assert_eq!(cfg.format, PackFormat::BenthosBinary);
        assert_eq!(
            cfg.max_messages, 1000,
            "default fn must fire on the env path"
        );
        assert_eq!(cfg.max_bytes, 4 * 1024 * 1024, "default fn must fire");
        assert!(!cfg.drop_message_id, "ids are kept when the key is absent");
    }

    /// The runtime loads routes through the `config` crate, not serde directly, so the
    /// boolean toggle on `pack` is checked on that path too.
    #[test]
    fn pack_boolean_survives_the_config_crate_loader() {
        let yaml = r#"
packed_route:
  input:
    memory: { topic: "in" }
    middlewares:
      - unpack: {}
  output:
    memory: { topic: "out" }
    middlewares:
      - pack: { max_messages: 250, drop_message_id: true }
"#;
        let config: Config = ConfigBuilder::builder()
            .add_source(config::File::from_str(yaml, config::FileFormat::Yaml))
            .build()
            .expect("config builds")
            .try_deserialize()
            .expect("config deserializes");

        let route = config.get("packed_route").expect("route exists");
        let Middleware::Pack(cfg) = &route.output.middlewares[0] else {
            panic!("expected pack, got {:?}", route.output.middlewares);
        };
        assert_eq!(cfg.max_messages, 250);
        assert!(
            cfg.drop_message_id,
            "`drop_message_id: true` must survive the config crate"
        );
        assert!(matches!(route.input.middlewares[0], Middleware::Unpack(_)));
    }

    const TEST_YAML: &str = r#"
kafka_to_nats:
  concurrency: 10
  input:
    middlewares:
      - deduplication:
          sled_path: "/tmp/mq-bridge/dedup_db"
          ttl_seconds: 3600
      - metrics: {}
      - retry:
          max_attempts: 5
          initial_interval_ms: 200
      - random_panic:
          mode: nack
      - dlq:
          endpoint:
            nats:
              subject: "dlq-subject"
              url: "nats://localhost:4222"
    kafka:
      topic: "input-topic"
      url: "localhost:9092"
      group_id: "my-consumer-group"
      tls:
        required: true
        ca_file: "/path_to_ca"
        cert_file: "/path_to_cert"
        key_file: "/path_to_key"
        cert_password: "password"
        accept_invalid_certs: true
  output:
    middlewares:
      - metrics: {}
      - dlq:
          endpoint:
            file:
              path: "error.out"
    nats:
      subject: "output-subject"
      url: "nats://localhost:4222"
"#;

    fn assert_config_values(config: &Config) {
        assert_eq!(config.len(), 1);
        let route = config.get("kafka_to_nats").expect("Route should exist");

        assert_eq!(route.options.concurrency, 10);

        // --- Assert Input ---
        let input = &route.input;
        assert_eq!(input.middlewares.len(), 5);

        let mut has_dedup = false;
        let mut has_metrics = false;
        let mut has_dlq = false;
        let mut has_retry = false;
        let mut has_random_panic = false;
        for middleware in &input.middlewares {
            match middleware {
                Middleware::Deduplication(dedup) => {
                    assert_eq!(dedup.sled_path.as_deref(), Some("/tmp/mq-bridge/dedup_db"));
                    assert_eq!(dedup.ttl_seconds, 3600);
                    has_dedup = true;
                }
                Middleware::Metrics(_) => {
                    has_metrics = true;
                }
                Middleware::Custom { .. } => {}
                Middleware::Dlq(dlq) => {
                    assert!(dlq.endpoint.middlewares.is_empty());
                    if let EndpointType::Nats(nats_cfg) = &dlq.endpoint.endpoint_type {
                        assert_eq!(nats_cfg.subject, Some("dlq-subject".to_string()));
                        assert_eq!(nats_cfg.url, "nats://localhost:4222");
                    }
                    has_dlq = true;
                }
                Middleware::Retry(retry) => {
                    assert_eq!(retry.max_attempts, 5);
                    assert_eq!(retry.initial_interval_ms, 200);
                    has_retry = true;
                }
                Middleware::RandomPanic(rp) => {
                    assert!(rp.mode == FaultMode::Nack);
                    has_random_panic = true;
                }
                Middleware::Delay(_) => {}
                Middleware::Timeout(_) => {}
                Middleware::WeakJoin(_) => {}
                Middleware::Limiter(_) => {}
                Middleware::Buffer(_) => {}
                Middleware::CookieJar(_) => {}
                Middleware::Transform(_) => {}
                Middleware::Encryption(_) => {}
                Middleware::Compression(_) => {}
                Middleware::Pack(_) => {}
                Middleware::Unpack(_) => {}
                Middleware::Id(_) => {}
                Middleware::Filter(_) => {}
                Middleware::Otel(_) => {}
                Middleware::Lookup(_) => {}
                Middleware::Aggregate(_) => {}
            }
        }

        if let EndpointType::Kafka(kafka) = &input.endpoint_type {
            assert_eq!(kafka.topic, Some("input-topic".to_string()));
            assert_eq!(kafka.url, "localhost:9092");
            assert_eq!(kafka.group_id, Some("my-consumer-group".to_string()));
            let tls = &kafka.tls;
            assert!(tls.required);
            assert_eq!(tls.ca_file.as_deref(), Some("/path_to_ca"));
            assert!(tls.accept_invalid_certs);
        } else {
            panic!("Input endpoint should be Kafka");
        }
        assert!(has_dedup);
        assert!(has_metrics);
        assert!(has_dlq);
        assert!(has_retry);
        assert!(has_random_panic);

        // --- Assert Output ---
        let output = &route.output;
        assert_eq!(output.middlewares.len(), 2);
        assert!(matches!(output.middlewares[0], Middleware::Metrics(_)));

        if let EndpointType::Nats(nats) = &output.endpoint_type {
            assert_eq!(nats.subject, Some("output-subject".to_string()));
            assert_eq!(nats.url, "nats://localhost:4222");
        } else {
            panic!("Output endpoint should be NATS");
        }
    }

    #[test]
    fn test_deserialize_from_yaml() {
        // We use serde_yaml directly here because the `config` crate's processing
        // can interfere with complex deserialization logic.
        let result: Result<Config, _> = serde_yaml_ng::from_str(TEST_YAML);
        println!("Deserialized from YAML: {:#?}", result);
        let config = result.expect("Failed to deserialize TEST_YAML");
        assert_config_values(&config);
    }

    /// Every test that reads the `MQB__` namespace sees *all* of it, so two of them
    /// running at once would each pick up the other's routes — and one's cleanup can
    /// unset vars the other is still reading. Serialize them.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_deserialize_from_env() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        const VARS: [&str; 11] = [
            "MQB__KAFKA_TO_NATS__CONCURRENCY",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__URL",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__GROUP_ID",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__REQUIRED",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__CA_FILE",
            "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__ACCEPT_INVALID_CERTS",
            "MQB__KAFKA_TO_NATS__OUTPUT__NATS__SUBJECT",
            "MQB__KAFKA_TO_NATS__OUTPUT__NATS__URL",
            "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__SUBJECT",
            "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__URL",
        ];
        struct EnvCleanup(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for EnvCleanup {
            fn drop(&mut self) {
                unsafe {
                    for (name, previous) in self.0.drain(..) {
                        if let Some(value) = previous {
                            std::env::set_var(name, value);
                        } else {
                            std::env::remove_var(name);
                        }
                    }
                }
            }
        }
        let _cleanup = EnvCleanup(
            VARS.into_iter()
                .map(|name| (name, std::env::var_os(name)))
                .collect(),
        );

        // Set environment variables based on README
        unsafe {
            std::env::set_var("MQB__KAFKA_TO_NATS__CONCURRENCY", "10");
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__TOPIC", "input-topic");
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__URL", "localhost:9092");
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__GROUP_ID",
                "my-consumer-group",
            );
            std::env::set_var("MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__REQUIRED", "true");
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__CA_FILE",
                "/path_to_ca",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__KAFKA__TLS__ACCEPT_INVALID_CERTS",
                "true",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__OUTPUT__NATS__SUBJECT",
                "output-subject",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__OUTPUT__NATS__URL",
                "nats://localhost:4222",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__SUBJECT",
                "dlq-subject",
            );
            std::env::set_var(
                "MQB__KAFKA_TO_NATS__INPUT__MIDDLEWARES__0__DLQ__ENDPOINT__NATS__URL",
                "nats://localhost:4222",
            );
        }

        let builder = ConfigBuilder::builder()
            // Enable automatic type parsing for values from environment variables.
            .add_source(
                Environment::with_prefix("MQB")
                    .separator("__")
                    .try_parsing(true),
            );

        let config: Config = builder
            .build()
            .expect("Failed to build config")
            .try_deserialize()
            .expect("Failed to deserialize config");

        // We can't test all values from env, but we can check the ones we set.
        assert_eq!(config.get("kafka_to_nats").unwrap().options.concurrency, 10);
        if let EndpointType::Kafka(k) = &config.get("kafka_to_nats").unwrap().input.endpoint_type {
            assert_eq!(k.topic, Some("input-topic".to_string()));
            assert!(k.tls.required);
        } else {
            panic!("Expected Kafka endpoint");
        }

        let input = &config.get("kafka_to_nats").unwrap().input;
        assert_eq!(input.middlewares.len(), 1);
        if let Middleware::Dlq(_) = &input.middlewares[0] {
            // Correctly parsed
        } else {
            panic!("Expected DLQ middleware");
        }
    }

    #[test]
    fn test_extract_secrets() {
        let mut config = Config::new();
        let mut route = Route::default();

        // Setup Kafka with secrets
        let mut kafka_config = KafkaConfig::new("kafka://user:pass@localhost:9092");
        kafka_config.username = Some("user".to_string());
        kafka_config.password = Some("pass".to_string());
        kafka_config.tls.cert_password = Some("certpass".to_string());

        route.input = Endpoint {
            endpoint_type: EndpointType::Kafka(kafka_config),
            middlewares: vec![],
            handler: None,
        };

        // Setup HTTP with basic auth
        let mut http_config = HttpConfig::new("http://httpuser:httppass@localhost");
        http_config.basic_auth = Some(("httpuser".to_string(), "httppass".to_string()));
        http_config
            .custom_headers
            .insert("X-API-Key".to_string(), "http-api-key".to_string());
        http_config.custom_headers.insert(
            "X_API_KEY".to_string(),
            "http-underscore-api-key".to_string(),
        );
        http_config.custom_headers.insert(
            "X-Access-Token".to_string(),
            "http-access-token".to_string(),
        );
        http_config.custom_headers.insert(
            "X-Authentication".to_string(),
            "http-authentication".to_string(),
        );
        http_config.custom_headers.insert(
            "Authorization".to_string(),
            "Bearer secret-token".to_string(),
        );
        http_config
            .custom_headers
            .insert("X-Trace-Id".to_string(), "trace-value".to_string());

        route.output = Endpoint {
            endpoint_type: EndpointType::Http(http_config),
            middlewares: vec![],
            handler: None,
        };

        config.insert("test_route".to_string(), route);

        let secrets = extract_config_secrets(&mut config);

        // Verify secrets extracted
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__URL")
                .map(|s| s.as_str()),
            Some("kafka://user:pass@localhost:9092")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__USERNAME")
                .map(|s| s.as_str()),
            Some("user")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__PASSWORD")
                .map(|s| s.as_str()),
            Some("pass")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__INPUT__KAFKA__TLS__CERT_PASSWORD")
                .map(|s| s.as_str()),
            Some("certpass")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__URL")
                .map(|s| s.as_str()),
            Some("http://httpuser:httppass@localhost")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__BASIC_AUTH__0")
                .map(|s| s.as_str()),
            Some("httpuser")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__BASIC_AUTH__1")
                .map(|s| s.as_str()),
            Some("httppass")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__CUSTOM_HEADERS__582D4150492D4B6579")
                .map(|s| s.as_str()),
            Some("http-api-key")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__CUSTOM_HEADERS__585F4150495F4B4559")
                .map(|s| s.as_str()),
            Some("http-underscore-api-key")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__CUSTOM_HEADERS__582D4163636573732D546F6B656E")
                .map(|s| s.as_str()),
            Some("http-access-token")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__CUSTOM_HEADERS__582D41757468656E7469636174696F6E")
                .map(|s| s.as_str()),
            Some("http-authentication")
        );
        assert_eq!(
            secrets
                .get("MQB__TEST_ROUTE__OUTPUT__HTTP__CUSTOM_HEADERS__417574686F72697A6174696F6E")
                .map(|s| s.as_str()),
            Some("Bearer secret-token")
        );

        // Verify config cleared
        let route = config.get("test_route").unwrap();
        let EndpointType::Kafka(k) = &route.input.endpoint_type else {
            panic!("expected Kafka input");
        };
        assert!(k.url.is_empty());
        assert!(k.username.is_none());
        assert!(k.password.is_none());
        assert!(k.tls.cert_password.is_none());
        let EndpointType::Http(h) = &route.output.endpoint_type else {
            panic!("expected HTTP output");
        };
        assert!(h.url.is_empty());
        assert!(h.basic_auth.is_none());
        assert!(!h.custom_headers.contains_key("X-API-Key"));
        assert!(!h.custom_headers.contains_key("X_API_KEY"));
        assert!(!h.custom_headers.contains_key("X-Access-Token"));
        assert!(!h.custom_headers.contains_key("X-Authentication"));
        assert!(!h.custom_headers.contains_key("Authorization"));
        assert_eq!(
            h.custom_headers.get("X-Trace-Id").map(|s| s.as_str()),
            Some("trace-value")
        );
    }

    #[test]
    fn extracted_dynamic_secret_keys_do_not_collide() {
        let mut encryption = EncryptionConfig::default();
        encryption
            .decrypt_keys
            .insert("old-key".to_string(), "hyphen-key".to_string());
        encryption
            .decrypt_keys
            .insert("old_key".to_string(), "underscore-key".to_string());

        let mut secrets = HashMap::new();
        encryption.extract_secrets("MQB__ENCRYPTION", &mut secrets);

        assert_eq!(
            secrets.get("MQB__ENCRYPTION__DECRYPT_KEYS__6F6C642D6B6579"),
            Some(&"hyphen-key".to_string())
        );
        assert_eq!(
            secrets.get("MQB__ENCRYPTION__DECRYPT_KEYS__6F6C645F6B6579"),
            Some(&"underscore-key".to_string())
        );
    }

    #[test]
    fn custom_config_secrets_are_extracted_by_field_name_and_url() {
        let mut endpoint = Endpoint {
            endpoint_type: EndpointType::Custom {
                name: "connect".to_string(),
                config: serde_json::json!({
                    "connector": "amqp_0_9",
                    "urls": ["amqp://guest:guest@localhost:5672/", "amqp://other:5672/"],
                    "key": "${! json(\"id\") }",
                    "max_tokens": "5",
                    "password": "${DB_PASSWORD}",
                    "token": "12345",
                    "sasl": { "user": "bob", "password": "pw" },
                    "tls": { "client_certs": [{ "cert": "c", "private_key": "pk" }] },
                    "dsn": "bob:pw@tcp(localhost:3306)/db",
                    "X-Api-Token": "kept",
                }),
            },
            middlewares: vec![Middleware::Custom {
                name: "connect".to_string(),
                config: serde_json::json!({ "api_key": "mw-key" }),
            }],
            handler: None,
        };

        let mut secrets = HashMap::new();
        endpoint.extract_secrets("MQB__R__OUTPUT", &mut secrets);

        let expected: HashMap<String, String> = [
            (
                "MQB__R__OUTPUT__CUSTOM__CONFIG__URLS__0",
                "amqp://guest:guest@localhost:5672/",
            ),
            ("MQB__R__OUTPUT__CUSTOM__CONFIG__SASL__PASSWORD", "pw"),
            (
                "MQB__R__OUTPUT__CUSTOM__CONFIG__TLS__CLIENT_CERTS__0__PRIVATE_KEY",
                "pk",
            ),
            (
                "MQB__R__OUTPUT__CUSTOM__CONFIG__DSN",
                "bob:pw@tcp(localhost:3306)/db",
            ),
            (
                "MQB__R__OUTPUT__MIDDLEWARES__0__CUSTOM__CONFIG__API_KEY",
                "mw-key",
            ),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
        assert_eq!(secrets, expected);

        let EndpointType::Custom { config, .. } = &endpoint.endpoint_type else {
            panic!("expected a custom endpoint");
        };
        assert_eq!(
            config,
            &serde_json::json!({
                "connector": "amqp_0_9",
                "urls": ["", "amqp://other:5672/"],
                "key": "${! json(\"id\") }",
                "max_tokens": "5",
                "password": "${DB_PASSWORD}",
                "token": "12345",
                "sasl": { "user": "bob" },
                "tls": { "client_certs": [{ "cert": "c" }] },
                "X-Api-Token": "kept",
            })
        );
    }

    #[test]
    fn grpc_binary_metadata_is_extracted_reversibly() {
        let mut grpc = GrpcConfig::new("https://localhost:50051");
        grpc.binary_metadata
            .insert("authorization-bin".to_string(), vec![0, 127, 255]);
        grpc.binary_metadata
            .insert("x-trace-bin".to_string(), vec![1, 2, 3]);
        grpc.bearer_token = Some("bearer-secret".to_string());
        grpc.api_key = Some("api-secret".to_string());

        let mut secrets = HashMap::new();
        grpc.extract_secrets("MQB__ROUTE__OUTPUT__GRPC", &mut secrets);

        assert!(grpc.binary_metadata.is_empty());
        assert!(grpc.bearer_token.is_none());
        assert!(grpc.api_key.is_none());
        for (key, expected) in [
            (
                "MQB__ROUTE__OUTPUT__GRPC__BINARY_METADATA__617574686F72697A6174696F6E2D62696E",
                vec![0, 127, 255],
            ),
            (
                "MQB__ROUTE__OUTPUT__GRPC__BINARY_METADATA__782D74726163652D62696E",
                vec![1, 2, 3],
            ),
        ] {
            let restored: Vec<u8> = serde_json::from_str(secrets.get(key).unwrap()).unwrap();
            assert_eq!(restored, expected);
        }
        assert_eq!(
            secrets.get("MQB__ROUTE__OUTPUT__GRPC__BEARER_TOKEN"),
            Some(&"bearer-secret".to_string())
        );
        assert_eq!(
            secrets.get("MQB__ROUTE__OUTPUT__GRPC__API_KEY"),
            Some(&"api-secret".to_string())
        );
    }

    #[test]
    fn test_extract_sensitive_url_only_strips_authority_credentials() {
        let mut config = Config::new();
        let path_at_route = Route {
            output: Endpoint {
                endpoint_type: EndpointType::Http(HttpConfig::new(
                    "https://example.com/path/user@example.com?email=a@b.test",
                )),
                middlewares: vec![],
                handler: None,
            },
            ..Default::default()
        };
        config.insert("path_at_route".to_string(), path_at_route);

        let credential_route = Route {
            output: Endpoint {
                endpoint_type: EndpointType::Http(HttpConfig::new(
                    "https://user:pass@example.com/path",
                )),
                middlewares: vec![],
                handler: None,
            },
            ..Default::default()
        };
        config.insert("credential_route".to_string(), credential_route);

        let query_at_route = Route {
            output: Endpoint {
                endpoint_type: EndpointType::Http(HttpConfig::new(
                    "https://example.com?next=a@b.test",
                )),
                middlewares: vec![],
                handler: None,
            },
            ..Default::default()
        };
        config.insert("query_at_route".to_string(), query_at_route);

        let fragment_at_route = Route {
            output: Endpoint {
                endpoint_type: EndpointType::Http(HttpConfig::new(
                    "https://example.com#user@example.com",
                )),
                middlewares: vec![],
                handler: None,
            },
            ..Default::default()
        };
        config.insert("fragment_at_route".to_string(), fragment_at_route);

        let secrets = extract_config_secrets(&mut config);

        let EndpointType::Http(http) = &config.get("path_at_route").unwrap().output.endpoint_type
        else {
            panic!("expected HTTP output");
        };
        assert_eq!(
            http.url,
            "https://example.com/path/user@example.com?email=a@b.test"
        );
        let EndpointType::Http(http) = &config.get("query_at_route").unwrap().output.endpoint_type
        else {
            panic!("expected HTTP output");
        };
        assert_eq!(http.url, "https://example.com?next=a@b.test");
        let EndpointType::Http(http) = &config
            .get("fragment_at_route")
            .unwrap()
            .output
            .endpoint_type
        else {
            panic!("expected HTTP output");
        };
        assert_eq!(http.url, "https://example.com#user@example.com");
        let EndpointType::Http(http) =
            &config.get("credential_route").unwrap().output.endpoint_type
        else {
            panic!("expected HTTP output");
        };
        assert!(http.url.is_empty());
        assert_eq!(
            secrets
                .get("MQB__CREDENTIAL_ROUTE__OUTPUT__HTTP__URL")
                .map(String::as_str),
            Some("https://user:pass@example.com/path")
        );
        assert!(!secrets.contains_key("MQB__PATH_AT_ROUTE__OUTPUT__HTTP__URL"));
        assert!(!secrets.contains_key("MQB__QUERY_AT_ROUTE__OUTPUT__HTTP__URL"));
        assert!(!secrets.contains_key("MQB__FRAGMENT_AT_ROUTE__OUTPUT__HTTP__URL"));
    }

    #[test]
    fn test_memory_config_requires_topic_or_url() {
        let err = serde_yaml_ng::from_str::<MemoryConfig>("{}").unwrap_err();
        assert!(err
            .to_string()
            .contains("MemoryConfig: 'topic' (or 'url' alias) is required."));
    }

    #[test]
    fn test_file_config_inference() {
        let yaml = r#"
mode: group_subscribe
path: "/tmp/test"
group_id: "my_group"
"#;
        let config: FileConfig = serde_yaml_ng::from_str(yaml).unwrap();
        match config.mode {
            Some(FileConsumerMode::GroupSubscribe { group_id, .. }) => {
                assert_eq!(group_id, "my_group")
            }
            _ => panic!("Expected GroupSubscribe"),
        }

        let yaml_queue = r#"
mode: consume
path: "/tmp/test"
"#;
        let config_queue: FileConfig = serde_yaml_ng::from_str(yaml_queue).unwrap();
        match config_queue.mode {
            Some(FileConsumerMode::Consume { delete }) => assert!(!delete),
            _ => panic!("Expected Consume"),
        }
    }

    #[test]
    fn endpoint_config_builders_initialize_required_fields_and_defaults() {
        let object_store = ObjectStoreConfig::new("s3://bucket/prefix")
            .with_checkpoint("file:///tmp/object-store.json", "orders");
        assert_eq!(object_store.url, "s3://bucket/prefix");
        assert!(object_store.date_partition_enabled(NameBy::WriteTime));
        assert_eq!(object_store.cursor_id.as_deref(), Some("orders"));

        let postgres_cdc = PostgresCdcConfig::new("postgres://localhost/db", "events")
            .with_slot("events_slot")
            .with_checkpoint_store("file:///tmp/postgres-cdc.json");
        assert_eq!(postgres_cdc.slot_name, "events_slot");
        assert!(postgres_cdc.create_slot);
        assert!(postgres_cdc.status_interval_ms > 0);

        let sqlx = SqlxConfig::new("postgres://localhost/db", "messages")
            .with_credentials("user", "secret");
        assert_eq!(sqlx.table, "messages");
        assert_eq!(sqlx.username.as_deref(), Some("user"));

        let clickhouse = ClickHouseConfig::new("http://localhost:8123", "messages")
            .with_credentials("default", "secret");
        assert_eq!(clickhouse.table, "messages");
        assert_eq!(clickhouse.compression, Compression::Gzip);
        assert_eq!(clickhouse.username.as_deref(), Some("default"));
    }
}

#[cfg(feature = "schema")]
mod schema_tests {
    use super::*;

    #[test]
    fn generate_json_schema() {
        let schema = schemars::schema_for!(Config);
        let schema_json = serde_json::to_string_pretty(&schema).unwrap();

        let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        path.push("mq-bridge.schema.json");
        std::fs::write(path, schema_json).expect("Failed to write schema file");
    }
}

mod switch_config_tests {
    use crate::models::{Endpoint, EndpointType, SwitchConfig};
    use std::collections::HashMap;

    fn null_endpoint() -> Endpoint {
        Endpoint::new(EndpointType::Null)
    }

    fn lookup() -> SwitchConfig {
        let mut cases = HashMap::new();
        cases.insert("archive".to_string(), null_endpoint());
        SwitchConfig {
            metadata_key: "kind".to_string(),
            cases,
            when: Vec::new(),
            default: None,
        }
    }

    fn predicate() -> SwitchConfig {
        SwitchConfig {
            metadata_key: String::new(),
            cases: HashMap::new(),
            when: vec![crate::models::SwitchCase {
                condition: "amount > 100".to_string(),
                to: null_endpoint(),
            }],
            default: None,
        }
    }

    #[test]
    fn each_mode_on_its_own_is_valid() {
        lookup().validate().unwrap();
        predicate().validate().unwrap();
    }

    /// Mixing the modes would hide which one a message actually took.
    #[test]
    fn both_modes_at_once_are_rejected() {
        let mut config = lookup();
        config.when = predicate().when;
        assert!(config.validate().is_err());
    }

    #[test]
    fn neither_mode_is_rejected() {
        let config = SwitchConfig {
            metadata_key: String::new(),
            cases: HashMap::new(),
            when: Vec::new(),
            default: Some(Box::new(null_endpoint())),
        };
        assert!(config.validate().is_err());
    }

    /// `cases` without a key to look them up by never matches anything.
    #[test]
    fn cases_without_a_metadata_key_are_rejected() {
        let mut config = lookup();
        config.metadata_key.clear();
        assert!(config.validate().is_err());
    }
}

#[cfg(feature = "filter")]
mod filter_expression_deserialization_tests {
    use crate::models::{Endpoint, Middleware, SwitchCase};

    #[test]
    fn middleware_rejects_invalid_expression_during_deserialization() {
        let error = serde_yaml_ng::from_str::<Endpoint>(
            "middlewares:\n  - filter: 'amount >'\nnull: null\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains("invalid filter expression"));
    }

    #[test]
    fn switch_case_rejects_invalid_expression_during_deserialization() {
        let error =
            serde_yaml_ng::from_str::<SwitchCase>("if: 'items[0].qty > 1'\nto:\n  null: null\n")
                .unwrap_err();

        assert!(error.to_string().contains("indexed path"));
    }

    #[test]
    fn valid_filter_expression_still_deserializes() {
        let endpoint = serde_yaml_ng::from_str::<Endpoint>(
            "middlewares:\n  - filter: 'amount > 100'\nnull: null\n",
        )
        .unwrap();

        assert!(
            matches!(&endpoint.middlewares[0], Middleware::Filter(filter) if filter.expression == "amount > 100")
        );
    }

    #[test]
    fn filter_accepts_a_map_and_writes_a_bare_string_without_on_error() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<Endpoint>(yaml);
        let bare = parse("middlewares:\n  - filter: 'amount > 100'\nnull: null\n").unwrap();
        let map = parse(
            "middlewares:\n  - filter: { expression: 'amount > 100', on_error: drop }\nnull: null\n",
        )
        .unwrap();
        let Middleware::Filter(filter) = &map.middlewares[0] else {
            panic!("expected a filter");
        };
        assert_eq!(filter.on_error, crate::models::InputErrorPolicy::Drop);

        let written = |endpoint: &Endpoint| {
            serde_json::to_value(&endpoint.middlewares[0]).unwrap()["filter"].clone()
        };
        assert_eq!(written(&bare), "amount > 100");
        assert_eq!(
            written(&map),
            serde_json::json!({"expression": "amount > 100", "on_error": "drop"})
        );

        let error =
            parse("middlewares:\n  - filter: { expression: 'items[0].qty > 1' }\nnull: null\n")
                .unwrap_err();
        assert!(error.to_string().contains("indexed path"), "{error}");
        assert!(parse("middlewares:\n  - filter: { expresion: 'a > 1' }\nnull: null\n").is_err());
    }

    #[test]
    fn unknown_field_in_file_and_object_store_endpoint_is_rejected() {
        let error = serde_yaml_ng::from_str::<Endpoint>("file:\n  path: x\n  fromat: raw\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `fromat`"), "{error}");

        // The flattened consumer mode's keys are still accepted.
        serde_yaml_ng::from_str::<Endpoint>("file:\n  path: x\n  mode: consume\n  delete: true\n")
            .unwrap();

        let error = serde_yaml_ng::from_str::<Endpoint>(
            "object_store:\n  url: memory:///x\n  fromat: raw\n",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("fromat"), "{error}");
    }
}

mod format_name_tests {
    use super::*;

    #[test]
    fn descriptive_format_names_mean_the_old_ones() {
        for (alias, name) in [
            ("payload", "raw"),
            ("envelope", "normal"),
            ("envelope_json", "json"),
            ("envelope_text", "text"),
        ] {
            let aliased: FileFormat = serde_json::from_value(alias.into()).unwrap();
            let named: FileFormat = serde_json::from_value(name.into()).unwrap();
            assert_eq!(aliased, named);
            // Written back under the old name, so older versions read the config.
            assert_eq!(serde_json::to_value(&aliased).unwrap(), name);
            let mongo: MongoDbFormat = serde_json::from_value(alias.into()).unwrap();
            assert_eq!(serde_json::to_value(mongo).unwrap(), name);
        }
    }
}

mod secret_extraction_tests {
    use super::*;

    fn secrets_of(yaml: &str) -> (Config, Vec<String>) {
        let mut config: Config = serde_yaml_ng::from_str(yaml).expect("config parses");
        let mut keys: Vec<String> = extract_config_secrets(&mut config).into_keys().collect();
        keys.sort();
        (config, keys)
    }

    #[test]
    fn names_that_share_a_secret_key_are_reported() {
        let routes = |first: &str, second: &str| -> Config {
            let yaml = format!(
                "{first}:\n  input: {{ memory: {{ topic: a }} }}\n  output: {{ \"null\": null }}\n\
                 {second}:\n  input: {{ memory: {{ topic: b }} }}\n  output: {{ \"null\": null }}\n"
            );
            serde_yaml_ng::from_str(&yaml).expect("config parses")
        };
        assert!(check_config_secret_keys(&routes("my-route", "other")).is_ok());
        let error = check_config_secret_keys(&routes("my-route", "my_route")).unwrap_err();
        assert!(error.to_string().contains("route names"), "{error}");

        let switch: Config = serde_yaml_ng::from_str(
            r#"
r:
  input: { memory: { topic: a } }
  output:
    fanout:
      - switch:
          metadata_key: kind
          cases:
            a-b: { "null": null }
            a_b: { "null": null }
"#,
        )
        .expect("config parses");
        let error = check_config_secret_keys(&switch).unwrap_err();
        assert!(error.to_string().contains("switch case"), "{error}");
    }

    #[test]
    fn only_whole_words_mark_a_header_as_a_secret() {
        for (header, secret) in [
            ("X-Api-Key", true),
            ("X_API_KEY", true),
            ("Authorization", true),
            ("Proxy-Authorization", true),
            ("X-Auth", true),
            ("X-Access-Token", true),
            ("Set-Cookie", true),
            ("X-Author", false),
            ("X-Keyboard-Layout", false),
            ("X-Trace-Id", false),
        ] {
            assert_eq!(
                super::secrets::is_sensitive_map_key(header),
                secret,
                "{header}"
            );
        }
    }

    #[test]
    fn every_credentialed_endpoint_gives_up_its_url_and_login() {
        for (kind, name, extra) in [
            ("nats", "NATS", ""),
            ("amqp", "AMQP", ""),
            ("mongodb", "MONGODB", "database: d"),
            ("mqtt", "MQTT", ""),
            ("ibmmq", "IBMMQ", "queue_manager: qm\n      channel: c"),
            ("redis_streams", "REDIS_STREAMS", ""),
            ("sqlx", "SQLX", "table: t"),
            ("clickhouse", "CLICKHOUSE", "table: t"),
        ] {
            let yaml = format!(
                "r:\n  input:\n    memory: {{ topic: t }}\n  output:\n    {kind}:\n      url: \"x://u:p@host/db\"\n      username: u\n      password: p\n      {extra}\n"
            );
            let (config, keys) = secrets_of(&yaml);
            let prefix = format!("MQB__R__OUTPUT__{name}__");
            for field in ["URL", "USERNAME", "PASSWORD"] {
                assert!(
                    keys.contains(&format!("{prefix}{field}")),
                    "{kind}: {keys:?}"
                );
            }
            let left = serde_json::to_string(&config).unwrap();
            assert!(!left.contains("u:p@host"), "{kind} kept its url: {left}");
        }
    }

    #[test]
    fn url_only_endpoints_give_up_a_url_with_credentials() {
        for (kind, name) in [("websocket", "WEBSOCKET"), ("zeromq", "ZEROMQ")] {
            let yaml = format!(
                "r:\n  input:\n    memory: {{ topic: t }}\n  output:\n    {kind}:\n      url: \"x://u:p@host\"\n"
            );
            let (_, keys) = secrets_of(&yaml);
            assert_eq!(keys, [format!("MQB__R__OUTPUT__{name}__URL")], "{kind}");
        }
    }

    #[test]
    fn nested_endpoints_are_reached_through_structural_ones() {
        let (_, keys) = secrets_of(
            r#"
r:
  input:
    memory: { topic: t }
  output:
    middlewares:
      - dlq:
          endpoint:
            nats: { url: "nats://h", subject: s, token: tok }
      - encryption:
          key: "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
    fanout:
      - aws: { queue_url: "https://k:s@sqs/q", access_key: a, secret_key: b, session_token: c }
      - switch:
          metadata_key: kind
          cases:
            "a b": { kafka: { url: "h:9092", topic: t, password: p } }
          default: { mqtt: { url: "mqtt://h", topic: t, password: p } }
      - reader: { sqlx: { url: "postgres://h/db", table: t, password: p } }
      - request:
          to: { nats: { url: "nats://h", subject: s, password: p } }
          forward_to: { amqp: { url: "amqp://h", queue: q, password: p } }
"#,
        );
        let out = "MQB__R__OUTPUT__";
        for expected in [
            "MIDDLEWARES__0__DLQ__ENDPOINT__NATS__TOKEN",
            "MIDDLEWARES__1__ENCRYPTION__KEY",
            "FANOUT__0__AWS__QUEUE_URL",
            "FANOUT__0__AWS__ACCESS_KEY",
            "FANOUT__0__AWS__SECRET_KEY",
            "FANOUT__0__AWS__SESSION_TOKEN",
            "FANOUT__1__SWITCH__CASES__A_B__KAFKA__PASSWORD",
            "FANOUT__1__SWITCH__DEFAULT__MQTT__PASSWORD",
            "FANOUT__2__READER__SQLX__PASSWORD",
            "FANOUT__3__REQUEST__TO__NATS__PASSWORD",
            "FANOUT__3__REQUEST__FORWARD_TO__AMQP__PASSWORD",
        ] {
            assert!(
                keys.contains(&format!("{out}{expected}")),
                "{expected}: {keys:#?}"
            );
        }
    }
}
