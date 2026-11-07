//! Route throughput with a middleware chain on the input or the output side.
//!
//! A `memory → null` route drains a prefilled channel, so the number is the engine's
//! own cost per message. The same chain is placed on the input (it runs in the
//! reader task) and on the output (it runs in the workers), at concurrency 1 and 8.
//!
//! Run with: `cargo bench --bench chain_matrix_bench --features test-utils,filter,aggregate`

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mq_bridge::models::{Endpoint, EndpointType, Middleware, Route};
use mq_bridge::CanonicalMessage;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const ROWS: usize = 20_000;
const BATCH: usize = 128;

fn corpus() -> Vec<CanonicalMessage> {
    const COUNTRIES: [&str; 5] = ["US", "GB", "DE", "IN", "JP"];
    (0..ROWS)
        .map(|i| {
            let payload = format!(
                r#"{{"id":"{i}","user":"u{}","first_name":"Ada","country":"{}","amount":"{}.{:02}","n":{},"created_at":"2020-01-01T00:00:00Z","active":"{}"}}"#,
                i % 1000,
                COUNTRIES[i % COUNTRIES.len()],
                i % 10_000,
                i % 100,
                i % 97,
                i % 2 == 0,
            );
            CanonicalMessage::new(payload.into_bytes(), None)
        })
        .collect()
}

fn chains() -> Vec<(&'static str, &'static str)> {
    vec![
        ("none", ""),
        (
            "transform_schema",
            r#"- transform:
    schema:
      type: object
      properties:
        id: { type: integer }
        amount: { type: number }
        active: { type: boolean }"#,
        ),
        (
            "transform_expr",
            r#"- transform:
    expression: "{id: id, country: country, amount: number(amount), tier: 'x'}""#,
        ),
        (
            "lookup_static",
            r#"- lookup:
    from: { static: { body: '{"v":"${payload:id}"}', raw: true } }
    into: prev"#,
        ),
        (
            "aggregate_fields",
            r#"- aggregate:
    consistency: single_writer
    key: "${payload:user}"
    into: stats
    fields:
      count: count
      total: sum(n)"#,
        ),
        (
            "aggregate_expr",
            r#"- aggregate:
    consistency: single_writer
    key: "${payload:user}"
    into: stats
    expression: "{ total: (state.total ?? 0) + n, count: (state.count ?? 0) + 1 }""#,
        ),
        (
            "transform_schema_par",
            r#"- transform:
    schema:
      type: object
      properties:
        id: { type: integer }
        amount: { type: number }
        active: { type: boolean }
- parallel: {}"#,
        ),
        (
            "transform_expr_par",
            r#"- transform:
    expression: "{id: id, country: country, amount: number(amount), tier: 'x'}"
- parallel: {}"#,
        ),
        (
            "lookup_static_par",
            r#"- lookup:
    from: { static: { body: '{"v":"${payload:id}"}', raw: true } }
    into: prev
- parallel: {}"#,
        ),
    ]
}

fn middlewares(yaml: &str) -> Vec<Middleware> {
    if yaml.is_empty() {
        return Vec::new();
    }
    let value: serde_json::Value = serde_yaml_ng::from_str(yaml).expect("chain is YAML");
    serde_json::from_value(value).unwrap_or_else(|e| panic!("chain parses: {e}\n{yaml}"))
}

static RUN: AtomicUsize = AtomicUsize::new(0);

async fn drain_once(chain: &[Middleware], on_input: bool, concurrency: usize) -> Duration {
    let topic = format!("chain_matrix_{}", RUN.fetch_add(1, Ordering::Relaxed));
    let mut input = Endpoint::new_memory(&topic, 4);
    let mut output = Endpoint::new(EndpointType::Null);
    for middleware in chain {
        if on_input {
            input = input.add_middleware(middleware.clone());
        } else {
            output = output.add_middleware(middleware.clone());
        }
    }
    let channel = input.channel().expect("memory channel");
    channel.fill_messages(corpus()).await.expect("filled");
    channel.close();

    let route = Route::new(input, output)
        .with_concurrency(concurrency)
        .with_batch_size(BATCH);
    let start = Instant::now();
    route
        .run_until_err(&topic, None, None)
        .await
        .expect("route drains");
    start.elapsed()
}

fn bench_matrix(c: &mut Criterion) {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    let mut group = c.benchmark_group("chain_matrix");
    group.throughput(Throughput::Elements(ROWS as u64));
    group.warm_up_time(Duration::from_millis(200));
    group.measurement_time(Duration::from_millis(800));
    group.sample_size(10);

    for (name, yaml) in chains() {
        let chain = middlewares(yaml);
        for (side, on_input) in [("in", true), ("out", false)] {
            // `parallel` is input-only.
            if (name == "none" || name.ends_with("_par")) && !on_input {
                continue;
            }
            for concurrency in [1usize, 8] {
                let id = BenchmarkId::new(format!("{name}/{side}"), concurrency);
                group.bench_function(id, |b| {
                    b.to_async(&runtime).iter_custom(|iters| {
                        let chain = chain.clone();
                        async move {
                            let mut total = Duration::ZERO;
                            for _ in 0..iters {
                                total += drain_once(&chain, on_input, concurrency).await;
                            }
                            total
                        }
                    });
                });
            }
        }
    }
    group.finish();
}

criterion_group!(benches, bench_matrix);
criterion_main!(benches);
