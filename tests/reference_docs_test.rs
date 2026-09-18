//! Parses every config shape documented in docs/REFERENCE.md.
//!
//! The reference file is the place people (and LLMs) look up what exists and how to spell
//! it, so a snippet that does not deserialize is a real bug.
//!
//! The snippets are read out of docs/REFERENCE.md itself rather than copied here, so a doc edit
//! is tested as written and cannot drift from a stale duplicate. A fenced block opts in by
//! tagging its info string — ```yaml middleware, ```yaml endpoint or ```yaml route; a plain
//! ```yaml block is prose and is skipped. Only cases a fence cannot express (the `null`
//! spelling, behavioural rules) stay hand-written below.

use mq_bridge::models::{Endpoint, Middleware};
use std::collections::HashMap;

const REFERENCE: &str = include_str!("../docs/REFERENCE.md");

/// A fenced code block lifted out of docs/REFERENCE.md.
struct Fence {
    /// 1-based line of the opening fence, for failure messages.
    line: usize,
    info: String,
    body: String,
}

/// Splits docs/REFERENCE.md into its fenced code blocks.
fn fenced_blocks() -> Vec<Fence> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String, Vec<&str>)> = None;

    for (index, line) in REFERENCE.lines().enumerate() {
        match (line.strip_prefix("```"), open.as_mut()) {
            (Some(_), Some(_)) => {
                let (line_no, info, body) = open.take().expect("checked open above");
                blocks.push(Fence {
                    line: line_no,
                    info,
                    body: body.join("\n"),
                });
            }
            (Some(info), None) => open = Some((index + 1, info.trim().to_string(), Vec::new())),
            (None, Some((_, _, body))) => body.push(line),
            (None, None) => {}
        }
    }

    if let Some((line_no, info, _)) = open {
        panic!("docs/REFERENCE.md has an unterminated ```{info} fence opened at line {line_no}");
    }
    blocks
}

/// The blocks tagged with `tag`.
fn tagged(tag: &str) -> Vec<Fence> {
    fenced_blocks()
        .into_iter()
        .filter(|fence| fence.info == tag)
        .collect()
}

/// Indents every non-empty line, so a doc snippet can be embedded under a YAML key.
fn indent(yaml: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    yaml.trim()
        .lines()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Parses a `middlewares:` list exactly as it appears in the reference — middleware is only
/// ever deserialized as part of an endpoint, never standalone.
fn middlewares(entries_yaml: &str, line: usize) -> Vec<Middleware> {
    let doc = format!(
        "middlewares:\n{}\nmemory: {{ topic: \"reference_probe\" }}\n",
        indent(entries_yaml, 2)
    );
    let endpoint: Endpoint = serde_yaml_ng::from_str(&doc).unwrap_or_else(|e| {
        panic!("docs/REFERENCE.md middleware block at line {line} does not parse: {e}\n{doc}")
    });
    endpoint.middlewares
}

fn endpoint(yaml: &str) -> Endpoint {
    serde_yaml_ng::from_str(yaml).unwrap_or_else(|e| {
        panic!("docs/REFERENCE.md endpoint snippet does not parse: {e}\n{yaml}")
    })
}

/// Endpoint blocks in the reference are shown in place, under the `input:` / `output:` key
/// they would occupy in a route. A block may hold several such fragments, separated by a
/// blank line (e.g. the publisher and consumer sides of `stream_buffer`).
fn endpoints(body: &str, line: usize) -> Vec<Endpoint> {
    let mut found = Vec::new();
    for chunk in body.split("\n\n") {
        if chunk.trim().is_empty() {
            continue;
        }
        let fragment: HashMap<String, Endpoint> =
            serde_yaml_ng::from_str(chunk).unwrap_or_else(|e| {
                panic!(
                    "docs/REFERENCE.md endpoint block at line {line} does not parse: {e}\n{chunk}"
                )
            });
        for (key, value) in fragment {
            assert!(
                key == "input" || key == "output",
                "docs/REFERENCE.md endpoint block at line {line} uses key '{key}'; \
                 expected the fragment to sit under 'input' or 'output'"
            );
            found.push(value);
        }
    }
    assert!(
        !found.is_empty(),
        "docs/REFERENCE.md endpoint block at line {line} yielded no endpoints"
    );
    found
}

/// Every middleware documented in the reference, parsed from the doc itself.
#[test]
fn documented_middleware_snippets_parse() {
    let blocks = tagged("yaml middleware");
    assert!(
        blocks.len() >= 15,
        "expected the reference to tag a `yaml middleware` block per middleware section, found {}",
        blocks.len()
    );
    for fence in blocks {
        let parsed = middlewares(&fence.body, fence.line);
        assert!(
            !parsed.is_empty(),
            "docs/REFERENCE.md middleware block at line {} yielded no middleware",
            fence.line
        );
    }
}

/// Every structural endpoint documented in the reference, parsed from the doc itself.
#[test]
fn documented_structural_endpoint_snippets_parse() {
    let blocks = tagged("yaml endpoint");
    assert!(
        blocks.len() >= 6,
        "expected the reference to tag a `yaml endpoint` block per structural endpoint section, found {}",
        blocks.len()
    );
    for fence in blocks {
        endpoints(&fence.body, fence.line);
    }
}

/// The complete named-route examples, parsed from the doc itself.
#[test]
fn documented_route_snippets_parse() {
    let blocks = tagged("yaml route");
    assert!(
        blocks.len() >= 5,
        "expected the reference to tag its complete route examples, found {}",
        blocks.len()
    );
    for fence in blocks {
        let routes: HashMap<String, mq_bridge::models::Route> =
            serde_yaml_ng::from_str(&fence.body).unwrap_or_else(|e| {
                panic!(
                    "docs/REFERENCE.md route block at line {} does not parse: {e}\n{}",
                    fence.line, fence.body
                )
            });
        assert!(
            !routes.is_empty(),
            "docs/REFERENCE.md route block at line {} yielded no routes",
            fence.line
        );
    }
}

/// `null` is the one endpoint whose YAML spelling is a trap. A bare YAML null is the form
/// that works; `null: {}` does *not* parse. This pins what the reference documents.
#[test]
fn documented_null_endpoint_spelling_parses() {
    assert_eq!(endpoint("null").endpoint_type.name(), "null");
    assert_eq!(endpoint("null: null").endpoint_type.name(), "null");

    let rejected: Result<Endpoint, _> = serde_yaml_ng::from_str("null: {}");
    assert!(
        rejected.is_err(),
        "`null: {{}}` must stay documented as invalid"
    );
}

/// The reference states that omitting `output` defaults it to `null`.
#[test]
fn omitted_output_defaults_to_null_as_documented() {
    let route: mq_bridge::models::Route = serde_yaml_ng::from_str(
        r#"
input:
  memory: { topic: "in" }
"#,
    )
    .unwrap();
    assert_eq!(route.output.endpoint_type.name(), "null");
}

/// The reference distinguishes "wrong side, warns and is skipped" from "wrong side, hard
/// error". Both behaviours are pinned here so the table stays accurate.
#[tokio::test]
async fn wrong_side_middleware_matches_documented_behaviour() {
    use mq_bridge::models::{EndpointType, WeakJoinMiddleware};

    async fn publisher_for(middlewares: Vec<Middleware>) -> anyhow::Result<()> {
        let mut output = Endpoint::new_memory("reference_docs_wrong_side", 10);
        output.middlewares = middlewares;
        let inner = match &output.endpoint_type {
            EndpointType::Memory(cfg) => {
                mq_bridge::endpoints::memory::MemoryPublisher::new_async(cfg)
                    .await
                    .unwrap()
            }
            _ => unreachable!(),
        };
        mq_bridge::middleware::apply_middlewares_to_publisher(
            Box::new(inner),
            &output,
            "reference_docs_route",
        )
        .await
        .map(|_| ())
    }

    // Documented as a hard error on the publisher side.
    let weak_join = publisher_for(vec![Middleware::WeakJoin(WeakJoinMiddleware {
        group_by: "correlation_id".to_string(),
        expected_count: 2,
        timeout_ms: 1000,
        branch_by: None,
        required: Vec::new(),
        on_timeout: Default::default(),
    })])
    .await;
    assert!(
        weak_join.is_err(),
        "weak_join on an output must fail at startup, as documented"
    );

    // Same, for `id`: the table lists it alongside weak_join as a hard error.
    let id = publisher_for(vec![Middleware::Id("${payload:order_id}".to_string())]).await;
    assert!(
        id.is_err(),
        "id on an output must fail at startup, as documented"
    );

    // Documented as warn-and-skip on the consumer side: the route still starts.
    let mut input = Endpoint::new_memory("reference_docs_wrong_side_in", 10);
    input.middlewares = vec![Middleware::Retry(Default::default())];
    let consumer = match &input.endpoint_type {
        EndpointType::Memory(cfg) => mq_bridge::endpoints::memory::MemoryConsumer::new_async(cfg)
            .await
            .unwrap(),
        _ => unreachable!(),
    };
    assert!(
        mq_bridge::middleware::apply_middlewares_to_consumer(
            Box::new(consumer),
            &input,
            "reference_docs_route",
        )
        .await
        .is_ok(),
        "retry on an input must warn and be skipped, not fail"
    );
}

/// The ordering rule the reference leads with: for publishers the last middleware in the
/// list is the outermost layer. Pinned here because every config that combines `dlq` with
/// anything else depends on it, and it is the reverse of the consumer side.
#[tokio::test]
async fn publisher_middleware_wraps_last_entry_outermost() {
    use mq_bridge::models::{
        DeadLetterQueueMiddleware, EndpointType, FaultMode, RandomPanicMiddleware,
    };
    use mq_bridge::CanonicalMessage;

    // `enabled` must be set explicitly: the derived Default is false, while the serde
    // default used when parsing YAML is true.
    let always_fail = RandomPanicMiddleware {
        mode: FaultMode::JsonFormatError,
        enabled: true,
        ..Default::default()
    };

    let dlq_endpoint = Endpoint::new_memory("reference_docs_dlq", 10);
    let mut output = Endpoint::new_memory("reference_docs_main", 10);
    output.middlewares = vec![
        Middleware::RandomPanic(always_fail),
        Middleware::Dlq(Box::new(DeadLetterQueueMiddleware {
            endpoint: dlq_endpoint.clone(),
        })),
    ];

    let inner = match &output.endpoint_type {
        EndpointType::Memory(cfg) => mq_bridge::endpoints::memory::MemoryPublisher::new_async(cfg)
            .await
            .unwrap(),
        _ => unreachable!(),
    };

    let publisher = mq_bridge::middleware::apply_middlewares_to_publisher(
        Box::new(inner),
        &output,
        "reference_docs_route",
    )
    .await
    .unwrap();

    // With `dlq` last (outermost) it catches the failure instead of the error escaping.
    publisher
        .send(CanonicalMessage::from("payload"))
        .await
        .expect("dlq listed last should capture the failure");

    assert_eq!(
        dlq_endpoint.channel().unwrap().drain_messages().len(),
        1,
        "the failed message should have been dead-lettered"
    );
}

/// `pack` / `unpack` config shapes, including the boolean toggle, deserialized the way a
/// route file and an inline JSON endpoint each present them.
#[test]
fn pack_config_round_trips_through_yaml_and_json() {
    let parsed = middlewares(
        "- pack: { max_messages: 250, max_bytes: 1024, compression: lz4, drop_message_id: true }",
        0,
    );
    let Middleware::Pack(cfg) = &parsed[0] else {
        panic!("expected pack, got {parsed:?}");
    };
    assert_eq!(cfg.max_messages, 250);
    assert_eq!(cfg.max_bytes, 1024);
    assert!(cfg.drop_message_id, "yaml `true` must reach the config");

    // The toggle is negated so the safe behaviour — ids survive — is what a missing
    // key gives, without depending on a default function.
    let defaults = middlewares("- pack: {}", 0);
    let Middleware::Pack(cfg) = &defaults[0] else {
        panic!("expected pack");
    };
    assert_eq!(cfg.max_messages, 1000);
    assert!(!cfg.drop_message_id, "omitted means ids are kept");

    let json: Middleware =
        serde_json::from_str(r#"{"pack":{"drop_message_id":true,"compression":"zstd"}}"#)
            .expect("json pack config");
    let Middleware::Pack(cfg) = &json else {
        panic!("expected pack");
    };
    assert!(cfg.drop_message_id, "json `true` must reach the config");

    let parsed = middlewares("- unpack: { max_messages: 5000 }", 0);
    let Middleware::Unpack(cfg) = &parsed[0] else {
        panic!("expected unpack, got {parsed:?}");
    };
    assert_eq!(cfg.max_messages, Some(5000));
}
