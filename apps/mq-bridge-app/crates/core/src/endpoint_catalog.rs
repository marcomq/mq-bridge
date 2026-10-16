//! Every endpoint scheme and middleware an endpoint URI can name, and how to
//! write each one. The scheme table drives [`crate::endpoint_uri`]; params come
//! from the schemas it types query params with. Backs `mqb list`/`describe`.

use mq_bridge::models::{
    AmqpConfig, AwsConfig, ClickHouseConfig, DirSpoolConfig, FileConfig, GrpcConfig, HttpConfig,
    IbmMqConfig, KafkaConfig, MongoDbConfig, MqttConfig, NatsConfig, ObjectStoreConfig,
    PostgresCdcConfig, RedisStreamsConfig, SqlxConfig, WebSocketConfig, ZeroMqConfig,
};
use serde::Serialize;
use serde_json::Value;

/// Fixed `(name, type, description)` docs for an endpoint built by its own parser.
pub(crate) type FixedParams = &'static [(&'static str, &'static str, &'static str)];

/// How a URI becomes the endpoint's config.
pub(crate) enum Target {
    /// Host and port form the connection URL; query params set config fields.
    Url(fn() -> schemars::Schema, UrlForm),
    /// The URI path is the target (`file`, `dir_spool`).
    Path(fn() -> schemars::Schema),
    /// Built by its own function (structural endpoints, `http-bulk`).
    Parser(
        fn(&url::Url, &str) -> anyhow::Result<mq_bridge::models::Endpoint>,
        FixedParams,
    ),
}

#[derive(Clone, Copy)]
pub(crate) struct UrlForm {
    /// Unknown params ride along on the connection URL. Only where it is a
    /// query-bearing connection string: appending `?k=v` would corrupt kafka's
    /// host list, ibmmq's `host(port)` or a broker address, so there it is an error.
    pub driver_options: bool,
    /// Config field the URI path fills, e.g. `nats://host/<subject>`.
    pub path_field: Option<&'static str>,
    /// Prefix rewrites from a CLI-only scheme to the one the driver expects.
    pub rewrites: &'static [(&'static str, &'static str)],
    /// Runs on `(uri, url)` after the rewrites; `None` leaves `url` unset.
    pub fixup: Option<fn(&str, String) -> Option<String>>,
}

const PLAIN: UrlForm = UrlForm {
    driver_options: false,
    path_field: None,
    rewrites: &[],
    fixup: None,
};
const DRIVER: UrlForm = UrlForm {
    driver_options: true,
    ..PLAIN
};

/// One URI scheme: the parser dispatches on it, and `list`/`describe` print it.
pub(crate) struct Scheme {
    scheme: &'static str,
    aliases: &'static [&'static str],
    pub endpoint: &'static str,
    category: &'static str,
    summary: &'static str,
    uri: &'static str,
    example: &'static str,
    pub target: Target,
}

/// The entry whose scheme or alias is `scheme`.
pub(crate) fn find_scheme(scheme: &str) -> Option<Scheme> {
    schemes()
        .into_iter()
        .find(|s| s.scheme == scheme || s.aliases.contains(&scheme))
}

/// The config schema a built-in endpoint type's query params are typed with,
/// or `None` when its URI is built by its own parser.
pub fn config_schema(endpoint: &str) -> Option<schemars::Schema> {
    let scheme = schemes().into_iter().find(|s| s.endpoint == endpoint)?;
    match scheme.target {
        Target::Url(schema, _) | Target::Path(schema) => Some(schema()),
        Target::Parser(..) => None,
    }
}

fn schemes() -> Vec<Scheme> {
    use Target::{Parser, Path, Url};
    vec![
        Scheme {
            scheme: "kafka",
            aliases: &[],
            endpoint: "kafka",
            category: "broker",
            summary: "Apache Kafka topic",
            uri: "kafka://host:port[,host:port]?topic=<topic>",
            example: "kafka://localhost:9092?topic=orders",
            target: Url(
                || schemars::schema_for!(KafkaConfig),
                UrlForm {
                    // rdkafka's bootstrap.servers is a bare host:port list, no scheme.
                    rewrites: &[("kafka://", "")],
                    fixup: Some(crate::endpoint_uri::kafka_url),
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "nats",
            aliases: &[],
            endpoint: "nats",
            category: "broker",
            summary: "NATS subject, JetStream unless no_jetstream=true",
            uri: "nats://host:port/<subject>",
            example: "nats://localhost:4222/orders",
            target: Url(
                || schemars::schema_for!(NatsConfig),
                UrlForm {
                    path_field: Some("subject"),
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "amqp",
            aliases: &["amqps", "rabbitmq", "rabbitmqs"],
            endpoint: "amqp",
            category: "broker",
            summary: "AMQP 0.9.1 (RabbitMQ) queue or exchange",
            uri: "amqp://user:pass@host:port/<vhost>?queue=<queue>",
            example: "amqp://guest:guest@localhost:5672/%2f?queue=orders",
            target: Url(
                || schemars::schema_for!(AmqpConfig),
                UrlForm {
                    rewrites: &[("rabbitmqs://", "amqps://"), ("rabbitmq://", "amqp://")],
                    ..DRIVER
                },
            ),
        },
        Scheme {
            scheme: "mqtt",
            aliases: &["mqtts"],
            endpoint: "mqtt",
            category: "broker",
            summary: "MQTT topic",
            uri: "mqtt://host:port?topic=<topic>",
            example: "mqtt://localhost:1883?topic=sensors/temp",
            target: Url(
                || schemars::schema_for!(MqttConfig),
                UrlForm {
                    rewrites: &[("mqtts://", "ssl://"), ("mqtt://", "tcp://")],
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "redis",
            aliases: &["rediss", "redis+unix", "valkey", "valkeys", "valkey+unix"],
            endpoint: "redis_streams",
            category: "broker",
            summary: "Redis or Valkey stream",
            uri: "redis://[user:pass@]host:port[/<db>]?stream=<stream>",
            example: "redis://localhost:6379?stream=orders",
            target: Url(
                || schemars::schema_for!(RedisStreamsConfig),
                UrlForm {
                    // The endpoint's TLS handling keys on `rediss://`.
                    rewrites: &[
                        ("valkeys://", "rediss://"),
                        ("valkey+unix://", "redis+unix://"),
                        ("valkey://", "redis://"),
                    ],
                    ..DRIVER
                },
            ),
        },
        Scheme {
            scheme: "ibmmq",
            aliases: &["ibm-mq"],
            endpoint: "ibmmq",
            category: "broker",
            summary: "IBM MQ queue or topic (client library loaded at runtime)",
            uri: "ibmmq://host:port?queue_manager=<qm>&channel=<channel>&queue=<queue>",
            example: "ibmmq://localhost:1414?queue_manager=QM1&channel=DEV.APP.SVRCONN&queue=DEV.QUEUE.1",
            target: Url(
                || schemars::schema_for!(IbmMqConfig),
                UrlForm {
                    rewrites: &[("ibm-mq://", "ibmmq://")],
                    fixup: Some(crate::endpoint_uri::ibmmq_url),
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "aws",
            aliases: &["aws-sqs"],
            endpoint: "aws",
            category: "broker",
            summary: "AWS SQS queue or SNS topic; the host is a placeholder",
            uri: "aws://_/?queue_url=<url-encoded queue URL>|topic_arn=<arn>",
            example: "aws://_/?queue_url=https%3A%2F%2Fsqs.eu-central-1.amazonaws.com%2F123%2Forders&region=eu-central-1",
            target: Url(
                || schemars::schema_for!(AwsConfig),
                UrlForm {
                    fixup: Some(crate::endpoint_uri::no_url),
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "zeromq",
            aliases: &["zmq"],
            endpoint: "zeromq",
            category: "broker",
            summary: "ZeroMQ socket; zeromq://tcp://host:port names the transport",
            uri: "zeromq://host:port?socket_type=<type>",
            example: "zeromq://localhost:5555?socket_type=push",
            target: Url(
                || schemars::schema_for!(ZeroMqConfig),
                UrlForm {
                    rewrites: &[("zeromq://", "tcp://"), ("zmq://", "tcp://")],
                    fixup: Some(crate::endpoint_uri::zeromq_url),
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "postgres",
            aliases: &["postgresql"],
            endpoint: "sqlx",
            category: "database",
            summary: "PostgreSQL table: insert sink, or cursor-polling source",
            uri: "postgres://user:pass@host:port/<db>?table=<table>",
            example: "postgres://app:secret@localhost:5432/shop?table=orders",
            target: Url(|| schemars::schema_for!(SqlxConfig), DRIVER),
        },
        Scheme {
            scheme: "mysql",
            aliases: &["mariadb"],
            endpoint: "sqlx",
            category: "database",
            summary: "MySQL or MariaDB table: insert sink, or cursor-polling source",
            uri: "mysql://user:pass@host:port/<db>?table=<table>",
            example: "mysql://app:secret@localhost:3306/shop?table=orders",
            target: Url(|| schemars::schema_for!(SqlxConfig), DRIVER),
        },
        Scheme {
            scheme: "sqlite",
            aliases: &[],
            endpoint: "sqlx",
            category: "database",
            summary: "SQLite table: insert sink, or cursor-polling source",
            uri: "sqlite://<file>?table=<table>",
            example: "sqlite://shop.db?table=orders",
            target: Url(|| schemars::schema_for!(SqlxConfig), DRIVER),
        },
        Scheme {
            scheme: "postgres-cdc",
            aliases: &["pgcdc"],
            endpoint: "postgres_cdc",
            category: "database",
            summary: "PostgreSQL change data capture over logical replication",
            uri: "postgres-cdc://user:pass@host:port/<db>?publication=<pub>&slot_name=<slot>",
            example: "postgres-cdc://app:secret@localhost:5432/shop?publication=orders_pub&slot_name=orders_slot",
            target: Url(
                || schemars::schema_for!(PostgresCdcConfig),
                UrlForm {
                    rewrites: &[
                        ("postgres-cdc://", "postgres://"),
                        ("pgcdc://", "postgres://"),
                    ],
                    ..DRIVER
                },
            ),
        },
        Scheme {
            scheme: "mongodb",
            aliases: &["mongodb+srv"],
            endpoint: "mongodb",
            category: "database",
            summary: "MongoDB collection; a source reads existing documents, then watches",
            uri: "mongodb://[user:pass@]host:port?database=<db>&collection=<collection>",
            example: "mongodb://localhost:27017?database=shop&collection=orders",
            target: Url(|| schemars::schema_for!(MongoDbConfig), DRIVER),
        },
        Scheme {
            scheme: "clickhouse",
            aliases: &["clickhouses"],
            endpoint: "clickhouse",
            category: "database",
            summary: "ClickHouse table over its HTTP interface",
            uri: "clickhouse://host:port?table=<table>",
            example: "clickhouse://localhost:8123?table=events",
            target: Url(
                || schemars::schema_for!(ClickHouseConfig),
                UrlForm {
                    rewrites: &[("clickhouses://", "https://"), ("clickhouse://", "http://")],
                    ..DRIVER
                },
            ),
        },
        Scheme {
            scheme: "file",
            aliases: &[],
            endpoint: "file",
            category: "storage",
            summary: "Local file; .gz/.zst/.lz4 sets the compression. `-` is stdin/stdout",
            uri: "file://<path>?format=<format>",
            example: "file:///tmp/orders.jsonl?format=json",
            target: Path(|| schemars::schema_for!(FileConfig)),
        },
        Scheme {
            scheme: "spool",
            aliases: &["dir-spool", "dirspool"],
            endpoint: "dir_spool",
            category: "storage",
            summary: "Crash-safe directory FIFO queue",
            uri: "spool://<directory>",
            example: "spool:///var/spool/orders",
            target: Path(|| schemars::schema_for!(DirSpoolConfig)),
        },
        Scheme {
            scheme: "s3",
            aliases: &["s3a"],
            endpoint: "object_store",
            category: "storage",
            summary: "Amazon S3 (or compatible) objects under a prefix",
            uri: "s3://<bucket>/<prefix>?format=<format>",
            example: "s3://lake/orders/?format=parquet",
            target: Url(|| schemars::schema_for!(ObjectStoreConfig), PLAIN),
        },
        Scheme {
            scheme: "gs",
            aliases: &["gcs"],
            endpoint: "object_store",
            category: "storage",
            summary: "Google Cloud Storage objects under a prefix",
            uri: "gs://<bucket>/<prefix>?format=<format>",
            example: "gs://lake/orders/?format=parquet",
            target: Url(
                || schemars::schema_for!(ObjectStoreConfig),
                UrlForm {
                    rewrites: &[("gcs://", "gs://")],
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "az",
            aliases: &["azure", "abfs", "abfss"],
            endpoint: "object_store",
            category: "storage",
            summary: "Azure Blob Storage objects under a prefix",
            uri: "az://<container>/<prefix>?format=<format>",
            example: "az://lake/orders/?format=parquet",
            target: Url(|| schemars::schema_for!(ObjectStoreConfig), PLAIN),
        },
        Scheme {
            scheme: "local-store",
            aliases: &[],
            endpoint: "object_store",
            category: "storage",
            summary: "Local directory with object-store semantics (one object per batch)",
            uri: "local-store://<directory>?format=<format>",
            example: "local-store:///tmp/lake/?format=parquet",
            target: Url(
                || schemars::schema_for!(ObjectStoreConfig),
                UrlForm {
                    rewrites: &[("local-store://", "file://")],
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "http",
            aliases: &["https"],
            endpoint: "http",
            category: "http",
            summary: "HTTP: a source listens on the address, a sink sends requests to it",
            uri: "http://host:port/<path>",
            example: "http://localhost:8080/orders",
            target: Url(|| schemars::schema_for!(HttpConfig), DRIVER),
        },
        Scheme {
            scheme: "ws",
            aliases: &["wss"],
            endpoint: "websocket",
            category: "http",
            summary: "WebSocket: a source listens, a sink connects",
            uri: "ws://host:port/<path>",
            example: "ws://localhost:9000/stream",
            target: Url(|| schemars::schema_for!(WebSocketConfig), DRIVER),
        },
        Scheme {
            scheme: "grpc",
            aliases: &["grpcs"],
            endpoint: "grpc",
            category: "http",
            summary: "gRPC client or server",
            uri: "grpc://host:port",
            example: "grpc://localhost:50051",
            target: Url(
                || schemars::schema_for!(GrpcConfig),
                UrlForm {
                    rewrites: &[("grpcs://", "https://"), ("grpc://", "http://")],
                    ..PLAIN
                },
            ),
        },
        Scheme {
            scheme: "http-bulk",
            aliases: &[],
            endpoint: "http_bulk",
            category: "http",
            summary: "Bulk JSON documents over HTTP (search engines); config is too nested for params",
            uri: "http-bulk:?config_file=<path>|config=<YAML or JSON>[&url=<url>]",
            example: "http-bulk:?config_file=bulk.yaml",
            target: Parser(
                crate::endpoint_uri::http_bulk_endpoint,
                &[
                    (
                        "config_file",
                        "string",
                        "File holding the http_bulk fields (YAML or JSON)",
                    ),
                    (
                        "config",
                        "string",
                        "The http_bulk fields inline, as YAML or JSON",
                    ),
                    ("url", "string", "Replaces the target URL of the config"),
                ],
            ),
        },
        Scheme {
            scheme: "memory",
            aliases: &[],
            endpoint: "memory",
            category: "structural",
            summary: "In-process channel, for tests and wiring routes together",
            uri: "memory://<topic>",
            example: "memory://orders",
            target: Parser(
                crate::endpoint_uri::memory_endpoint,
                &[
                    ("capacity", "integer", "Channel capacity"),
                    (
                        "subscribe_mode",
                        "boolean",
                        "Every subscriber gets every message",
                    ),
                ],
            ),
        },
        Scheme {
            scheme: "null",
            aliases: &[],
            endpoint: "null",
            category: "structural",
            summary: "Sink that discards everything",
            uri: "null:",
            example: "null:",
            target: Parser(crate::endpoint_uri::null_endpoint, &[]),
        },
        Scheme {
            scheme: "static",
            aliases: &[],
            endpoint: "static",
            category: "structural",
            summary: "Source that endlessly produces one fixed message, or a sink that answers with it",
            uri: "static:?body=<body>[&raw=true][&<metadata key>=<value>]",
            example: "static:?body=%7B%22id%22%3A1%7D&raw=true",
            target: Parser(
                crate::endpoint_uri::static_endpoint,
                &[
                    (
                        "body",
                        "string",
                        "Message body; may hold ${namespace:selector} placeholders",
                    ),
                    (
                        "body_file",
                        "string",
                        "Read the body from this file instead",
                    ),
                    (
                        "raw",
                        "boolean",
                        "Send the body verbatim rather than JSON-encoded",
                    ),
                    (
                        "<any other>",
                        "string",
                        "Becomes a metadata entry of the message",
                    ),
                ],
            ),
        },
        Scheme {
            scheme: "response",
            aliases: &[],
            endpoint: "response",
            category: "structural",
            summary: "Replies to the caller of the current request; the input needs a reply channel",
            uri: "response:",
            example: "response:",
            target: Parser(crate::endpoint_uri::response_endpoint, &[]),
        },
        Scheme {
            scheme: "fanout",
            aliases: &[],
            endpoint: "fanout",
            category: "structural",
            summary: "Sends every message to each branch, in the order written",
            uri: "fanout:?to=<uri>&mirror=<uri>&…",
            example: "fanout:?to=nats://localhost:4222/orders&mirror=file:///tmp/audit.jsonl",
            target: Parser(
                crate::endpoint_uri::fanout_endpoint,
                &[
                    (
                        "to",
                        "endpoint URI",
                        "Branch that may answer the caller (repeatable)",
                    ),
                    (
                        "mirror",
                        "endpoint URI",
                        "Branch whose response and failures are discarded (repeatable)",
                    ),
                ],
            ),
        },
        Scheme {
            scheme: "request",
            aliases: &[],
            endpoint: "request",
            category: "structural",
            summary: "Sends to one endpoint and forwards its response to another",
            uri: "request:?to=<uri>[&forward_to=<uri>]",
            example: "request:?to=http://localhost:8080/enrich&forward_to=nats://localhost:4222/enriched",
            target: Parser(
                crate::endpoint_uri::request_endpoint,
                &[
                    (
                        "to",
                        "endpoint URI",
                        "Required. Endpoint the message is sent to",
                    ),
                    (
                        "forward_to",
                        "endpoint URI",
                        "Gets the response; without it the response is discarded",
                    ),
                ],
            ),
        },
        Scheme {
            scheme: "switch",
            aliases: &[],
            endpoint: "switch",
            category: "structural",
            summary: "Routes each message to one branch, by a metadata value or by predicates",
            uri: "switch:?metadata_key=<key>&case.<value>=<uri>&default=<uri> | switch:?when=<expr>&to=<uri>&…&default=<uri>",
            example: "switch:?metadata_key=kind&case.order=nats://localhost:4222/orders&default=null:",
            target: Parser(
                crate::endpoint_uri::switch_endpoint,
                &[
                    (
                        "metadata_key",
                        "string",
                        "Metadata key whose value picks the case",
                    ),
                    (
                        "case.<value>",
                        "endpoint URI",
                        "Branch for messages whose metadata value is <value>",
                    ),
                    (
                        "when",
                        "expression",
                        "Predicate for the next `to`; first match wins. Write and/or, not &&/||",
                    ),
                    ("to", "endpoint URI", "Branch for the preceding `when`"),
                    ("default", "endpoint URI", "Branch when nothing matches"),
                ],
            ),
        },
    ]
}

/// What `mqb list --json` prints and the MCP `describe` tool returns.
#[derive(Serialize)]
pub struct Catalog {
    pub endpoints: Vec<EndpointDoc>,
    pub middlewares: Vec<MiddlewareDoc>,
}

#[derive(Serialize)]
pub struct EndpointDoc {
    pub scheme: String,
    pub aliases: Vec<String>,
    /// The endpoint type, as a YAML/JSON config names it.
    pub endpoint: String,
    pub category: String,
    pub summary: String,
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub example: Option<String>,
    /// What an unknown query param does: `driver_option` keeps it on the
    /// connection URL, `rejected` fails the URI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_params: Option<&'static str>,
    pub params: Vec<Param>,
}

#[derive(Serialize)]
pub struct MiddlewareDoc {
    pub name: String,
    pub summary: String,
    /// How the middleware is appended to an endpoint URI.
    pub spec: String,
    pub params: Vec<Param>,
}

#[derive(Serialize)]
pub struct Param {
    pub name: String,
    /// `string`, `integer`, `number`, `boolean`, `json` (a JSON literal), or
    /// `endpoint URI` for a nested endpoint.
    #[serde(rename = "type")]
    pub ty: String,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The scheme names the parser accepts, for its unsupported-scheme error.
pub(crate) fn scheme_names() -> (Vec<&'static str>, Vec<&'static str>) {
    let (structural, rest): (Vec<_>, Vec<_>) = schemes()
        .into_iter()
        .partition(|s| s.category == "structural");
    let names = |list: Vec<Scheme>| {
        list.iter()
            .flat_map(|s| std::iter::once(s.scheme).chain(s.aliases.iter().copied()))
            .collect()
    };
    (names(rest), names(structural))
}

/// Built-in schemes, then registered extension endpoints, then every middleware.
pub fn catalog() -> anyhow::Result<Catalog> {
    let mut endpoints: Vec<EndpointDoc> = schemes().into_iter().map(endpoint_doc).collect();
    for (name, schema) in mq_bridge::extensions::endpoint_config_schemas() {
        let scheme = name.replace('_', "-");
        if endpoints.iter().any(|e| e.scheme == scheme) {
            continue;
        }
        endpoints.push(EndpointDoc {
            uri: format!("{scheme}://<address>?<param>=<value>"),
            summary: summary(&schema),
            params: params(&schema, &schema, &[]),
            scheme,
            aliases: Vec::new(),
            endpoint: name,
            category: "extension".into(),
            example: None,
            unknown_params: None,
        });
    }

    let root = serde_json::to_value(schemars::schema_for!(mq_bridge::models::Middleware))?;
    let mut middlewares: Vec<MiddlewareDoc> = crate::endpoint_uri::middleware_tags(&root)
        .into_iter()
        // In a URI a plugin middleware is named directly, never as `custom`.
        .filter(|tag| tag != "custom")
        .filter_map(|tag| {
            let variant = crate::endpoint_uri::middleware_variant(&root, &tag)?;
            let bare = crate::endpoint_uri::accepts_bare_string(&root, variant);
            let params = params(&root, variant, &[]);
            Some(MiddlewareDoc {
                spec: spec(&tag, bare, &params),
                summary: variant_summary(&root, &tag, variant),
                params,
                name: tag,
            })
        })
        .collect();
    for (name, schema) in mq_bridge::extensions::middleware_config_schemas() {
        if middlewares.iter().any(|m| m.name == name) {
            continue;
        }
        let params = schema
            .as_ref()
            .map(|s| params(s, s, &[]))
            .unwrap_or_default();
        middlewares.push(MiddlewareDoc {
            summary: schema.as_ref().map(summary).unwrap_or_default(),
            spec: spec(&name, false, &params),
            params,
            name,
        });
    }
    Ok(Catalog {
        endpoints,
        middlewares,
    })
}

fn endpoint_doc(s: Scheme) -> EndpointDoc {
    let from_schema = |schema: fn() -> schemars::Schema, skip: &[&str]| {
        let root = serde_json::to_value(schema()).unwrap_or_default();
        params(&root, &root, skip)
    };
    let (params, unknown_params) = match s.target {
        Target::Url(schema, form) => (
            from_schema(schema, &["url"]),
            Some(if form.driver_options {
                "driver_option"
            } else {
                "rejected"
            }),
        ),
        Target::Path(schema) => (from_schema(schema, &["path"]), Some("rejected")),
        Target::Parser(_, fixed) => (
            fixed
                .iter()
                .map(|(name, ty, description)| Param {
                    name: name.to_string(),
                    ty: ty.to_string(),
                    required: description.starts_with("Required."),
                    default: None,
                    values: Vec::new(),
                    description: Some(description.to_string()),
                })
                .collect(),
            None,
        ),
    };
    EndpointDoc {
        scheme: s.scheme.into(),
        aliases: s.aliases.iter().map(|a| a.to_string()).collect(),
        endpoint: s.endpoint.into(),
        category: s.category.into(),
        summary: s.summary.into(),
        uri: s.uri.into(),
        example: Some(s.example.into()),
        unknown_params,
        params,
    }
}

/// The entries `name` refers to: a scheme, alias or endpoint type, and a middleware.
pub fn matching(catalog: Catalog, name: &str) -> Catalog {
    let name = name.trim().to_ascii_lowercase();
    let tag = name.replace('-', "_");
    Catalog {
        endpoints: catalog
            .endpoints
            .into_iter()
            .filter(|e| e.scheme == name || e.aliases.contains(&name) || e.endpoint == tag)
            .collect(),
        middlewares: catalog
            .middlewares
            .into_iter()
            .filter(|m| m.name == tag || m.name == name)
            .collect(),
    }
}

fn spec(name: &str, bare: bool, params: &[Param]) -> String {
    match params.first() {
        _ if bare && params.is_empty() => format!("|{name}?<value>"),
        _ if bare => format!("|{name}?<value> or |{name}?<param>=<value>&…"),
        Some(first) => format!("|{name}?{}=<value>&…", first.name),
        None => format!("|{name}"),
    }
}

fn deref<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
    node.get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| crate::endpoint_uri::resolve_ref(root, r))
        .unwrap_or(node)
}

/// The first paragraph of a schema's description, on one line.
fn first_paragraph(text: &str) -> String {
    text.split("\n\n")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary(schema: &Value) -> String {
    schema
        .get("description")
        .and_then(Value::as_str)
        .map(first_paragraph)
        .unwrap_or_default()
}

/// A middleware variant's own doc comment, else its config struct's.
fn variant_summary(root: &Value, tag: &str, variant: &Value) -> String {
    let own = root["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|member| member.get("properties").and_then(|p| p.get(tag)).is_some())
        .map(summary)
        .unwrap_or_default();
    if own.is_empty() {
        summary(deref(root, variant))
    } else {
        own
    }
}

/// Every config field reachable from `node`, through `$ref`, `allOf` and the
/// branches of `anyOf`/`oneOf` (a field only a branch has is never required).
fn params(root: &Value, node: &Value, skip: &[&str]) -> Vec<Param> {
    let mut out = Vec::new();
    collect(root, node, true, skip, &mut out, &mut Default::default());
    out
}

fn collect(
    root: &Value,
    node: &Value,
    binding: bool,
    skip: &[&str],
    out: &mut Vec<Param>,
    visited: &mut std::collections::HashSet<String>,
) {
    let Some(obj) = node.as_object() else { return };
    if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
        if visited.insert(reference.to_string())
            && let Some(target) = crate::endpoint_uri::resolve_ref(root, reference)
        {
            collect(root, target, binding, skip, out, visited);
        }
        return;
    }
    let required: Vec<&str> = obj
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (name, sub) in obj
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if skip.contains(&name.as_str()) || out.iter().any(|p: &Param| &p.name == name) {
            continue;
        }
        out.push(param(
            root,
            name,
            sub,
            binding && required.contains(&name.as_str()),
        ));
    }
    for (key, binds) in [("allOf", binding), ("anyOf", false), ("oneOf", false)] {
        for sub in obj.get(key).and_then(Value::as_array).into_iter().flatten() {
            collect(root, sub, binds, skip, out, visited);
        }
    }
}

fn param(root: &Value, name: &str, sub: &Value, required: bool) -> Param {
    let target = deref(root, sub);
    let description = [sub, target]
        .iter()
        .find_map(|s| s.get("description").and_then(Value::as_str))
        .map(first_paragraph);
    let mut values = Vec::new();
    enum_values(root, sub, &mut values);
    let ty = match crate::endpoint_uri::field_type(root, sub) {
        crate::endpoint_uri::FieldType::Bool => "boolean",
        crate::endpoint_uri::FieldType::Integer => "integer",
        crate::endpoint_uri::FieldType::Number => "number",
        crate::endpoint_uri::FieldType::Object => "json",
        crate::endpoint_uri::FieldType::StringLike => "string",
    };
    Param {
        name: name.to_string(),
        ty: ty.to_string(),
        required,
        default: sub.get("default").filter(|d| !d.is_null()).cloned(),
        values,
        description,
    }
}

/// The string values an enum field accepts, through `$ref` and `Option`.
fn enum_values(root: &Value, sub: &Value, out: &mut Vec<String>) {
    let sub = deref(root, sub);
    let mut push = |v: &Value| {
        if let Some(s) = v.as_str()
            && !out.iter().any(|o| o == s)
        {
            out.push(s.to_string());
        }
    };
    if let Some(constant) = sub.get("const") {
        push(constant);
    }
    for v in sub
        .get("enum")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        push(v);
    }
    for key in ["anyOf", "oneOf"] {
        for member in sub.get(key).and_then(Value::as_array).into_iter().flatten() {
            enum_values(root, member, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The catalog is written beside the parser, so each example must still parse.
    #[test]
    fn every_example_parses() {
        for s in schemes() {
            if s.scheme == "http-bulk" {
                continue;
            }
            crate::endpoint_uri::endpoint_from_uri(s.example)
                .unwrap_or_else(|e| panic!("{}: {:#}", s.example, e));
            for alias in s.aliases {
                let uri = s.example.replacen(s.scheme, alias, 1);
                crate::endpoint_uri::endpoint_from_uri(&uri)
                    .unwrap_or_else(|e| panic!("{uri}: {e:#}"));
            }
        }
    }

    /// Endpoint types without a URI scheme of their own: config-only, or
    /// extensions, which `catalog()` lists from the registry.
    const NO_SCHEME: &[&str] = &[
        "custom",
        "ref",
        "reader",
        "sequence",
        "stream_buffer",
        "sled",
        "pulsar",
        "elasticsearch",
        "meilisearch",
        "openai_chat",
        "openai_embeddings",
        "qdrant",
        "typesense",
    ];

    // A new endpoint type fails here until `schemes()` describes it.
    #[test]
    fn every_endpoint_type_is_catalogued() {
        fn tags(node: &Value, out: &mut Vec<String>) {
            for key in ["oneOf", "anyOf"] {
                for member in node
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    out.extend(
                        member
                            .get("properties")
                            .and_then(Value::as_object)
                            .into_iter()
                            .flat_map(|p| p.keys().cloned()),
                    );
                    out.extend(
                        member
                            .get("const")
                            .and_then(Value::as_str)
                            .map(String::from),
                    );
                    for v in member
                        .get("enum")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        out.extend(v.as_str().map(String::from));
                    }
                    tags(member, out);
                }
            }
        }
        let root =
            serde_json::to_value(schemars::schema_for!(mq_bridge::models::EndpointType)).unwrap();
        let mut found = Vec::new();
        tags(&root, &mut found);
        assert!(found.contains(&"kafka".to_string()), "{found:?}");
        let catalogued: Vec<_> = schemes().iter().map(|s| s.endpoint).collect();
        for tag in found {
            assert!(
                catalogued.contains(&tag.as_str()) || NO_SCHEME.contains(&tag.as_str()),
                "endpoint type '{tag}' is missing from schemes() in endpoint_catalog.rs \
                 (or from NO_SCHEME when the CLI has no URI for it)"
            );
        }
    }

    // A new scheme fails here until the hand-written connectors table names it.
    #[test]
    fn every_scheme_is_in_the_connectors_table() {
        let readme = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../../docs/book/connectors/README.md"
        ))
        .unwrap();
        for s in schemes().iter().filter(|s| s.category != "structural") {
            let mut names = std::iter::once(&s.scheme).chain(s.aliases);
            assert!(
                names.any(|name| readme.contains(&format!("`{name}:"))),
                "scheme '{}' is missing from docs/book/connectors/README.md",
                s.scheme
            );
        }
    }

    #[test]
    fn schema_params_are_typed_and_skip_uri_fields() {
        let catalog = catalog().unwrap();
        let kafka = matching(catalog, "kafka").endpoints.remove(0);
        assert!(kafka.params.iter().all(|p| p.name != "url"));
        let topic = kafka.params.iter().find(|p| p.name == "topic").unwrap();
        assert_eq!(topic.ty, "string");
        assert_eq!(kafka.unknown_params, Some("rejected"));
    }

    #[test]
    fn describe_matches_aliases_types_and_middlewares() {
        assert_eq!(
            matching(catalog().unwrap(), "rabbitmq").endpoints[0].scheme,
            "amqp"
        );
        assert_eq!(matching(catalog().unwrap(), "sqlx").endpoints.len(), 3);
        let weak = matching(catalog().unwrap(), "weak-join").middlewares;
        assert_eq!(weak[0].name, "weak_join");
        let retry = matching(catalog().unwrap(), "retry").middlewares.remove(0);
        let attempts = retry
            .params
            .iter()
            .find(|p| p.name == "max_attempts")
            .unwrap();
        assert_eq!(
            (attempts.ty.as_str(), attempts.default.clone()),
            ("integer", Some(3.into()))
        );
    }
}
