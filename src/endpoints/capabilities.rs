//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! What each endpoint type can do, as one table. `docs/CAPABILITIES.md` is rendered from
//! it, and tests compare it with the rules the engine applies.

use crate::models::EndpointType;

/// One cell of the capability table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capability {
    Yes,
    No,
    /// Only with this option set.
    With(&'static str),
    /// Unless this option is set.
    Unless(&'static str),
    /// Decided by the configuration or by the endpoints it wraps; see the notes.
    Depends,
    /// The endpoint is not used in that direction.
    NotApplicable,
}

/// How a connection of the endpoint is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    /// A `tls` block in the config.
    TlsBlock,
    /// Through the URL only: its scheme or its query parameters.
    Url,
    /// Always HTTPS, handled by the provider's client.
    Provider,
    /// No encryption is available.
    Unencrypted,
    /// Nothing leaves the host, or the endpoint only wraps other endpoints.
    Local,
}

/// The capabilities of one endpoint type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct EndpointCapabilities {
    /// The type name, as used in a config file.
    pub name: &'static str,
    /// Usable as a route input.
    pub input: bool,
    /// Usable as a route output.
    pub output: bool,
    /// Input: a message lost in a crash is delivered again. `No` makes a route at-most-once.
    pub acknowledges: Capability,
    /// Input: acknowledgements are applied in the order the batches were read.
    pub ordered_commit: Capability,
    /// Output: a send is answered with a response message.
    pub replies: Capability,
    /// Output: batches are written one after another, in order.
    pub ordered_publish: Capability,
    pub transport: Transport,
    pub input_note: &'static str,
    pub output_note: &'static str,
    /// How it authenticates, and limits of its encryption.
    pub security_note: &'static str,
}

use Capability::{Depends, No, NotApplicable as Na, Unless, With, Yes};
use Transport::{Local, Provider, TlsBlock, Unencrypted, Url};

const fn row(name: &'static str, transport: Transport) -> EndpointCapabilities {
    EndpointCapabilities {
        name,
        input: true,
        output: true,
        acknowledges: Yes,
        ordered_commit: Yes,
        replies: No,
        ordered_publish: No,
        transport,
        input_note: "",
        output_note: "",
        security_note: "",
    }
}

const fn output_only(name: &'static str, replies: Capability) -> EndpointCapabilities {
    EndpointCapabilities {
        input: false,
        acknowledges: Na,
        ordered_commit: Na,
        replies,
        ..row(name, Local)
    }
}

const fn input_only(name: &'static str, transport: Transport) -> EndpointCapabilities {
    EndpointCapabilities {
        output: false,
        replies: Na,
        ordered_publish: Na,
        ..row(name, transport)
    }
}

const CAPABILITIES: &[EndpointCapabilities] = &[
    EndpointCapabilities {
        ordered_commit: No,
        security_note: "User and password in the URL or as fields. `accept_invalid_certs` is not available.",
        ..row("amqp", TlsBlock)
    },
    EndpointCapabilities {
        ordered_commit: No,
        security_note: "IAM credentials from the config or the environment.",
        ..row("aws", Provider)
    },
    EndpointCapabilities {
        replies: With("lookup_query"),
        input_note: "A cursor source; needs `cursor_column`.",
        output_note: "The reply is the first row; `clickhouse.found` says whether there was one.",
        security_note: "User and password.",
        ..row("clickhouse", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: Depends,
        ordered_commit: Depends,
        replies: Depends,
        ordered_publish: Depends,
        input_note: "Decided by the registered factory.",
        output_note: "Decided by the registered factory.",
        security_note: "Decided by the registered factory.",
        ..row("custom", Local)
    },
    EndpointCapabilities {
        ordered_commit: No,
        ordered_publish: Yes,
        ..row("dir_spool", Local)
    },
    EndpointCapabilities {
        ordered_publish: Depends,
        output_note: "Replies when one of its endpoints does; the first in list order wins. Ordered when one of them is.",
        ..output_only("fanout", Depends)
    },
    EndpointCapabilities {
        ordered_publish: Yes,
        security_note: "At-rest encryption with `encryption`.",
        ..row("file", Local)
    },
    EndpointCapabilities {
        ordered_commit: No,
        replies: Depends,
        input_note: "A server or a streaming client. A server answers the caller, who resends on failure.",
        output_note: "The dynamic client returns the RPC's reply.",
        security_note: "A server checks client certificates with `tls.ca_file` and has no other authentication; reflection is always on. A client sends `api_key` or `bearer_token`.",
        ..row("grpc", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: Unless("fire_and_forget: true"),
        ordered_commit: No,
        replies: Yes,
        input_note: "A listener. The caller gets the outcome and resends on failure.",
        output_note: "The reply is the HTTP response, with the status in `http_status_code`.",
        security_note: "A listener has `basic_auth` and checks client certificates with `tls.ca_file`. A client sends `basic_auth` or custom headers.",
        ..row("http", TlsBlock)
    },
    EndpointCapabilities {
        replies: Depends,
        ordered_publish: With("operation"),
        input_note: "A paged read of a search index.",
        output_note: "Replies with `query` set.",
        security_note: "`auth` and custom headers.",
        ..row("http_bulk", TlsBlock)
    },
    EndpointCapabilities {
        security_note: "Its own `tls` block (`cipher_spec`, key repository). User and password.",
        ..row("ibmmq", TlsBlock)
    },
    EndpointCapabilities {
        security_note: "SASL user and password; they force `sasl_ssl`.",
        ..row("kafka", TlsBlock)
    },
    EndpointCapabilities {
        ordered_commit: No,
        replies: With("request_reply: true"),
        input_note: "A nacked message is retried inside the consumer and is lost if the process dies.",
        security_note: "In-process, or a local socket that checks the peer's user id.",
        ..row("memory", Local)
    },
    EndpointCapabilities {
        ordered_commit: Depends,
        replies: Depends,
        input_note: "`consumer` commits in any order; `snapshot`, `capture_new` and `capture_all` in order.",
        output_note: "Replies with `request_reply`, `report_outcome` or `find`.",
        security_note: "TLS and credentials can also be set in the URL (`tls=true`, `tlsCAFile=`).",
        ..row("mongodb", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: Unless("qos: 0"),
        ordered_commit: No,
        security_note: "User and password.",
        ..row("mqtt", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: Unless("no_jetstream: true"),
        ordered_commit: No,
        replies: With("request_reply: true"),
        security_note: "User and password, or a token.",
        ..row("nats", TlsBlock)
    },
    output_only("null", No),
    EndpointCapabilities {
        security_note: "Credentials as the provider's client reads them.",
        ..row("object_store", Provider)
    },
    EndpointCapabilities {
        security_note: "User and password in the URL.",
        ..input_only("postgres_cdc", TlsBlock)
    },
    EndpointCapabilities {
        output_note: "The reply is the message read from the wrapped input.",
        ..output_only("reader", Yes)
    },
    EndpointCapabilities {
        ordered_commit: No,
        security_note: "TLS with a `rediss://` URL or `tls.required`. User and password. `accept_invalid_certs` is not available.",
        ..row("redis_streams", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: Depends,
        ordered_commit: Depends,
        replies: Depends,
        ordered_publish: Depends,
        input_note: "Follows the referenced endpoint.",
        output_note: "Follows the referenced endpoint; a publisher registered in code is not checked.",
        ..row("ref", Local)
    },
    EndpointCapabilities {
        ordered_publish: Depends,
        output_note: "Passes up what its `forward_to` returns. Ordered when `to` or `forward_to` is.",
        ..output_only("request", Depends)
    },
    EndpointCapabilities {
        output_note: "The message itself, as the reply to the route's caller.",
        ..output_only("response", Yes)
    },
    EndpointCapabilities {
        acknowledges: Depends,
        input_note: "Follows the endpoint of the current phase; commits always in order.",
        ..input_only("sequence", Local)
    },
    EndpointCapabilities {
        ordered_commit: No,
        ..row("sled", Local)
    },
    EndpointCapabilities {
        ordered_commit: Depends,
        replies: With("lookup_query"),
        input_note: "A queue table commits in any order; a `cursor_column` or `publication` source in order.",
        output_note: "The reply is the first row; `sqlx.found` says whether there was one.",
        security_note: "TLS can also be set in the URL (`sslmode=`, `ssl-mode=`). MySQL with `tls.ca_file` checks the chain but not the host name, and takes a client certificate from the URL only.",
        ..row("sqlx", TlsBlock)
    },
    EndpointCapabilities {
        ordered_commit: No,
        replies: Yes,
        input_note: "Emits the configured body.",
        output_note: "The reply is the configured body.",
        ..row("static", Local)
    },
    row("stream_buffer", Local),
    EndpointCapabilities {
        ordered_publish: Depends,
        output_note: "Replies when the chosen destination does. Ordered when one of them is.",
        ..output_only("switch", Depends)
    },
    EndpointCapabilities {
        ordered_commit: No,
        input_note: "A listener. A reply goes back on the same connection; nothing is redelivered.",
        security_note: "A listener checks client certificates with `tls.ca_file` and has no other authentication and no connection limit.",
        ..row("websocket", TlsBlock)
    },
    EndpointCapabilities {
        acknowledges: No,
        ordered_commit: No,
        replies: Depends,
        output_note: "Replies on a `req` socket.",
        security_note: "No encryption and no authentication. Use it on a trusted network only.",
        ..row("zeromq", Unencrypted)
    },
];

/// The capabilities of every endpoint type, sorted by name.
pub fn capabilities() -> &'static [EndpointCapabilities] {
    CAPABILITIES
}

impl EndpointType {
    /// What this endpoint type can do. A cell that depends on the configuration says so
    /// instead of reading it.
    pub fn capabilities(&self) -> &'static EndpointCapabilities {
        let name = self.name();
        CAPABILITIES
            .iter()
            .find(|row| row.name == name)
            .expect("every endpoint type has a capability row")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::*;
    use serde::de::DeserializeOwned;

    const DOC: &str = include_str!("../../docs/CAPABILITIES.md");
    const BEGIN: &str = "<!-- generated: begin -->";
    const END: &str = "<!-- generated: end -->";

    fn cell(capability: Capability) -> String {
        match capability {
            Yes => "yes".to_string(),
            No => "no".to_string(),
            With(option) => format!("with `{option}`"),
            Unless(option) => format!("unless `{option}`"),
            Depends => "depends".to_string(),
            Na => "–".to_string(),
        }
    }

    fn transport(transport: Transport) -> &'static str {
        match transport {
            TlsBlock => "`tls` block",
            Url => "URL",
            Provider => "always (provider)",
            Unencrypted => "none",
            Local => "–",
        }
    }

    fn yes_no(value: bool) -> &'static str {
        if value {
            "yes"
        } else {
            "no"
        }
    }

    fn render() -> String {
        let mut out = String::new();
        out.push_str("## Direction\n\n| Endpoint | Input | Output |\n|---|---|---|\n");
        for row in CAPABILITIES {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                row.name,
                yes_no(row.input),
                yes_no(row.output)
            ));
        }
        out.push_str(
            "\n## Inputs\n\n| Endpoint | Acknowledges | Commits in order | Notes |\n|---|---|---|---|\n",
        );
        for row in CAPABILITIES.iter().filter(|row| row.input) {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                row.name,
                cell(row.acknowledges),
                cell(row.ordered_commit),
                row.input_note
            ));
        }
        out.push_str(
            "\n## Outputs\n\n| Endpoint | Replies | Ordered publish | Notes |\n|---|---|---|---|\n",
        );
        for row in CAPABILITIES.iter().filter(|row| row.output) {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                row.name,
                cell(row.replies),
                cell(row.ordered_publish),
                row.output_note
            ));
        }
        out.push_str(
            "\n## Encryption and authentication\n\n| Endpoint | TLS | Notes |\n|---|---|---|\n",
        );
        for row in CAPABILITIES {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                row.name,
                transport(row.transport),
                row.security_note
            ));
        }
        out
    }

    /// Run with `MQB_WRITE_CAPABILITIES=1` to rewrite the tables after changing a row.
    #[test]
    fn the_capability_doc_is_the_rendered_table() {
        let begin = DOC.find(BEGIN).expect("begin marker") + BEGIN.len();
        let end = DOC.find(END).expect("end marker");
        let rendered = format!("\n\n{}\n", render());
        if std::env::var_os("MQB_WRITE_CAPABILITIES").is_some() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/CAPABILITIES.md");
            let doc = format!("{}{rendered}{}", &DOC[..begin], &DOC[end..]);
            std::fs::write(path, doc).unwrap();
            return;
        }
        // A Windows checkout may convert the doc to CRLF.
        assert_eq!(
            DOC[begin..end].replace("\r\n", "\n"),
            rendered,
            "docs/CAPABILITIES.md is stale; see the comment on this test"
        );
    }

    #[test]
    fn rows_are_sorted_and_unique() {
        let names: Vec<_> = CAPABILITIES.iter().map(|row| row.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(names, sorted);
    }

    #[test]
    fn the_reply_column_is_the_rule_a_request_endpoint_applies() {
        let (never, with_option) = crate::endpoints::reply_rules();
        for row in CAPABILITIES.iter().filter(|row| row.output) {
            let option = with_option.iter().find(|(name, _)| *name == row.name);
            let expected = match option {
                Some((_, option)) => With(option),
                None if never.contains(&row.name) => No,
                None => row.replies,
            };
            assert_eq!(row.replies, expected, "{}", row.name);
            assert!(
                matches!(row.replies, Yes | Depends)
                    || option.is_some()
                    || never.contains(&row.name),
                "{} is missing from the reply rules",
                row.name
            );
        }
    }

    #[test]
    fn the_acknowledges_column_is_what_makes_a_route_at_most_once() {
        let at_most_once = |endpoint_type| crate::route::source_is_at_most_once(&endpoint_type);
        let nats = NatsConfig {
            no_jetstream: true,
            ..Default::default()
        };
        let mqtt = MqttConfig {
            qos: Some(0),
            ..Default::default()
        };
        let http = HttpConfig {
            fire_and_forget: true,
            ..Default::default()
        };
        assert!(at_most_once(EndpointType::ZeroMq(Default::default())));
        assert!(at_most_once(EndpointType::Nats(nats)));
        assert!(at_most_once(EndpointType::Mqtt(mqtt)));
        assert!(at_most_once(EndpointType::Http(http)));

        let lossy: Vec<_> = CAPABILITIES
            .iter()
            .filter(|row| matches!(row.acknowledges, No | Unless(_)))
            .map(|row| (row.name, row.acknowledges))
            .collect();
        assert_eq!(
            lossy,
            [
                ("http", Unless("fire_and_forget: true")),
                ("mqtt", Unless("qos: 0")),
                ("nats", Unless("no_jetstream: true")),
                ("zeromq", No),
            ]
        );
    }

    /// Works for a config that denies unknown fields, which `FileConfig` does not.
    fn has_tls_block<T: DeserializeOwned>() -> bool {
        match serde_json::from_value::<T>(serde_json::json!({ "tls": {} })) {
            Ok(_) => true,
            Err(error) => !error.to_string().contains("unknown field `tls`"),
        }
    }

    #[test]
    fn the_tls_column_names_the_configs_that_have_a_tls_block() {
        let probed = [
            ("amqp", has_tls_block::<AmqpConfig>()),
            ("aws", has_tls_block::<AwsConfig>()),
            ("clickhouse", has_tls_block::<ClickHouseConfig>()),
            ("dir_spool", has_tls_block::<DirSpoolConfig>()),
            ("grpc", has_tls_block::<GrpcConfig>()),
            ("http", has_tls_block::<HttpConfig>()),
            ("http_bulk", has_tls_block::<HttpBulkConfig>()),
            ("ibmmq", has_tls_block::<IbmMqConfig>()),
            ("kafka", has_tls_block::<KafkaConfig>()),
            ("memory", has_tls_block::<MemoryConfig>()),
            ("mongodb", has_tls_block::<MongoDbConfig>()),
            ("mqtt", has_tls_block::<MqttConfig>()),
            ("nats", has_tls_block::<NatsConfig>()),
            ("object_store", has_tls_block::<ObjectStoreConfig>()),
            ("postgres_cdc", has_tls_block::<PostgresCdcConfig>()),
            ("redis_streams", has_tls_block::<RedisStreamsConfig>()),
            ("sled", has_tls_block::<SledConfig>()),
            ("sqlx", has_tls_block::<SqlxConfig>()),
            ("websocket", has_tls_block::<WebSocketConfig>()),
            ("zeromq", has_tls_block::<ZeroMqConfig>()),
        ];
        for (name, has_block) in probed {
            let row = CAPABILITIES.iter().find(|row| row.name == name).unwrap();
            assert_eq!(row.transport == TlsBlock, has_block, "{name}");
        }
        let unprobed: Vec<_> = CAPABILITIES
            .iter()
            .filter(|row| row.transport == TlsBlock)
            .filter(|row| !probed.iter().any(|(name, _)| *name == row.name))
            .map(|row| row.name)
            .collect();
        assert!(unprobed.is_empty(), "not probed: {unprobed:?}");
    }

    fn core_endpoint(name: &str) -> Endpoint {
        let null = || Endpoint::new(EndpointType::Null);
        let memory = || Endpoint::new_memory("capability_probe", 1);
        Endpoint::new(match name {
            "null" => EndpointType::Null,
            "memory" => memory().endpoint_type,
            "fanout" => EndpointType::Fanout(vec![null()]),
            "response" => EndpointType::Response(Default::default()),
            "reader" => EndpointType::Reader(Box::new(memory())),
            "request" => EndpointType::Request(RequestForwardConfig {
                to: Box::new(memory()),
                forward_to: Box::new(null()),
            }),
            "sequence" => EndpointType::Sequence(SequenceConfig {
                endpoints: vec![memory()],
                cursor_id: None,
                checkpoint_store: None,
            }),
            "switch" => EndpointType::Switch(SwitchConfig {
                metadata_key: "kind".to_string(),
                cases: [("a".to_string(), null())].into(),
                when: Vec::new(),
                default: Some(Box::new(null())),
            }),
            other => panic!("no probe for '{other}'"),
        })
    }

    /// The direction of the structural types, which is where a direction is refused.
    #[test]
    fn the_direction_columns_are_what_the_route_checks_accept() {
        for name in [
            "null", "memory", "fanout", "response", "reader", "request", "sequence", "switch",
        ] {
            let endpoint = core_endpoint(name);
            let row = endpoint.endpoint_type.capabilities();
            let as_input = crate::endpoints::check_consumer("probe", &endpoint, None);
            let as_output = crate::endpoints::check_publisher("probe", &endpoint, None);
            assert_eq!(row.input, as_input.is_ok(), "{name} as input: {as_input:?}");
            assert_eq!(
                row.output,
                as_output.is_ok(),
                "{name} as output: {as_output:?}"
            );
        }
    }
}
