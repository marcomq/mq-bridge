//! Per-message cost of the `filter` and `transform` middlewares.
//!
//! Both are measured on the same seven-column row the CSV/ETL benchmarks use, so a
//! result here is directly comparable with `csv_to_json_bench`. The cases are chosen to
//! separate the paths that avoid building a JSON tree from the ones that cannot:
//!
//! - `eq_fast` is the single equality the fast predicate has always recognised.
//! - `compare` and `conjunction` are the shapes a real filter is usually written in.
//! - `schema` is the typing transform the published CSV benchmark runs.
//! - `expression` is the zen path, which has no fast route at all.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mq_bridge::models::TransformMiddleware;
use mq_bridge::test_utils::bench::{filter_matches, transform_messages};
use mq_bridge::CanonicalMessage;

const ROWS: usize = 20_000;

fn corpus() -> Vec<CanonicalMessage> {
    const COUNTRIES: [&str; 5] = ["US", "GB", "DE", "IN", "JP"];
    const TIERS: [&str; 3] = ["free", "pro", "enterprise"];
    (0..ROWS)
        .map(|i| {
            let payload = format!(
                r#"{{"id":"{}","first_name":"Ada","country":"{}","amount":"{}.{:02}","created_at":"2020-01-01T00:00:00Z","active":"{}","attributes":"{{\"tier\":\"{}\",\"score\":{}}}"}}"#,
                i,
                COUNTRIES[i % COUNTRIES.len()],
                i % 10_000,
                i % 100,
                i % 2 == 0,
                TIERS[i % TIERS.len()],
                i % 100,
            );
            CanonicalMessage::new(payload.into_bytes(), None)
        })
        .collect()
}

fn bench_filter(c: &mut Criterion) {
    let messages = corpus();
    let cases = [
        ("eq_fast", "country == 'US'"),
        ("compare", "amount > 5000"),
        ("conjunction", "country == 'US' and amount > 5000"),
        ("disjunction", "country == 'US' or country == 'DE'"),
        ("nested", "attributes != '' and id != '0'"),
    ];

    let mut group = c.benchmark_group("filter");
    group.throughput(Throughput::Elements(ROWS as u64));
    for (name, expression) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &expression, |b, expr| {
            b.iter(|| filter_matches(expr, &messages).expect("filter evaluates"));
        });
    }
    group.finish();
}

fn schema_transform() -> TransformMiddleware {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "id": { "type": "integer" },
            "amount": { "type": "number" },
            "active": { "type": "boolean" },
            "attributes": { "type": "string", "contentMediaType": "application/json" },
        }
    });
    TransformMiddleware {
        schema: Some(schema),
        ..Default::default()
    }
}

fn expression_transform() -> TransformMiddleware {
    TransformMiddleware {
        expression: Some(
            "{id: id, country: country, amount: number(amount), tier: 'x'}".to_string(),
        ),
        ..Default::default()
    }
}

fn bench_transform(c: &mut Criterion) {
    let messages = corpus();
    // `schema_passthrough` names only fields the payload already satisfies, so it
    // isolates the span walk from the cost of coercing a field.
    let passthrough = TransformMiddleware {
        schema: Some(serde_json::json!({
            "type": "object",
            "properties": { "country": { "type": "string" }, "first_name": { "type": "string" } }
        })),
        ..Default::default()
    };
    let cases = [
        ("schema", schema_transform()),
        ("schema_passthrough", passthrough),
        ("expression", expression_transform()),
    ];

    let mut group = c.benchmark_group("transform");
    group.throughput(Throughput::Elements(ROWS as u64));
    for (name, config) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), &config, |b, config| {
            b.iter(|| transform_messages(config, &messages).expect("transform applies"));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_filter, bench_transform);
criterion_main!(benches);
