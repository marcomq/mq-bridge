//! An application that needs one user per request: query Postgres itself, or call a route.
//!
//! Needs the lookup bench's Postgres (`docker compose up -d --wait` in `benches/lookup`) and
//! `POSTGRES_URL` pointing at it; the credentials are in `benches/lookup/docker-compose.yml`.
//! Run with `cargo run --release` (optional argument: requests per measurement, default 50000).
//!
//! - `direct`: one `= $1` query per request on a sqlx pool of 64 connections.
//! - `caller`: `Caller::call` per request into the `enrich` route of `routes.yaml`, whose
//!   `lookup` sends one `IN (…)` query for all requests waiting at that moment.
//! - `direct_any` / `call_many`: a client holding 256 keys at once, as one `= ANY($1)` query
//!   or one `Caller::call_many`.
//!
//! Every answer is checked against its key.

use mq_bridge::{Caller, CanonicalMessage, Route};
use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

const ROUTES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/routes.yaml");
const BATCH: usize = 256;

type User = (String, i64, String, String);

/// A user key spread over the table's one million rows.
fn key(n: usize) -> String {
    format!("/u/{:06}", n.wrapping_mul(2_654_435_761) % 1_000_000)
}

fn request(key: &str) -> CanonicalMessage {
    format!(r#"{{"path":"{key}"}}"#).into()
}

fn user_path(answer: &CanonicalMessage) -> String {
    let answer: serde_json::Value = serde_json::from_slice(&answer.payload).unwrap();
    answer["user"]["path"].as_str().unwrap().to_string()
}

fn postgres_url() -> String {
    std::env::var("POSTGRES_URL").expect("set POSTGRES_URL, see benches/lookup/docker-compose.yml")
}

/// `route` from `routes.yaml`, with `${POSTGRES_URL}` taken from the environment.
async fn deploy(route: &str) -> Caller {
    let text = std::fs::read_to_string(ROUTES).unwrap();
    let text = text.replace("${POSTGRES_URL}", &postgres_url());
    Caller::deploy(route, Route::from_str(&text, route).unwrap())
        .await
        .unwrap()
}

async fn pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(64)
        .test_before_acquire(false)
        .connect(&postgres_url())
        .await
        .unwrap()
}

async fn direct(clients: usize, per_client: usize) {
    let pool = pool().await;
    measure("direct", clients, per_client, 1, move |keys| {
        let pool = pool.clone();
        async move {
            let user: User =
                sqlx::query_as("SELECT path, id, name, tier FROM users WHERE path = $1")
                    .bind(&keys[0])
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(user.0, keys[0]);
        }
    })
    .await;
}

async fn caller(route: &str, clients: usize, per_client: usize) {
    let caller = deploy(route).await;
    let handle = caller.clone();
    measure(route, clients, per_client, 1, move |keys| {
        let caller = handle.clone();
        async move {
            let answer = caller.call(request(&keys[0])).await.unwrap();
            assert_eq!(user_path(&answer), keys[0]);
        }
    })
    .await;
    caller.stop().await;
}

async fn direct_any(clients: usize, per_client: usize) {
    let pool = pool().await;
    measure("direct_any", clients, per_client, BATCH, move |keys| {
        let pool = pool.clone();
        async move {
            let users: Vec<User> =
                sqlx::query_as("SELECT path, id, name, tier FROM users WHERE path = ANY($1)")
                    .bind(&keys)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let by_key: HashMap<_, _> = users.into_iter().map(|u| (u.0.clone(), u)).collect();
            assert!(keys.iter().all(|k| by_key.contains_key(k)));
        }
    })
    .await;
}

/// The query the route's `lookup` sends: one `IN ($1 … $n)`, not cached for n > 16.
async fn direct_in(clients: usize, per_client: usize) {
    let pool = pool().await;
    measure("direct_in", clients, per_client, BATCH, move |keys| {
        let pool = pool.clone();
        async move {
            let list: Vec<String> = (1..=keys.len()).map(|i| format!("${i}")).collect();
            let sql = format!(
                "SELECT path, id, name, tier FROM users WHERE path IN ({})",
                list.join(", ")
            );
            let mut select = sqlx::query_as(sqlx::AssertSqlSafe(sql)).persistent(false);
            for key in &keys {
                select = select.bind(key);
            }
            let users: Vec<User> = select.fetch_all(&pool).await.unwrap();
            let by_key: HashMap<_, _> = users.into_iter().map(|u| (u.0.clone(), u)).collect();
            assert!(keys.iter().all(|k| by_key.contains_key(k)));
        }
    })
    .await;
}

async fn call_many(route: &str, clients: usize, per_client: usize) {
    let caller = deploy(route).await;
    let handle = caller.clone();
    let name = format!("call_many {route}");
    measure(&name, clients, per_client, BATCH, move |keys| {
        let caller = handle.clone();
        async move {
            let answers = caller
                .call_many(keys.iter().map(|k| request(k)).collect())
                .await;
            for (key, answer) in keys.iter().zip(answers) {
                assert_eq!(&user_path(&answer.unwrap()), key);
            }
        }
    })
    .await;
    caller.stop().await;
}

/// Runs `clients` tasks that each send `per_client` keys, `chunk` keys per `op`, and prints
/// keys/s plus the latency of one `op`.
async fn measure<F, Fut>(name: &str, clients: usize, per_client: usize, chunk: usize, op: F)
where
    F: Fn(Vec<String>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let start = Instant::now();
    let tasks: Vec<_> = (0..clients)
        .map(|client| {
            let op = op.clone();
            tokio::spawn(async move {
                let mut latencies = Vec::new();
                for first in (0..per_client).step_by(chunk) {
                    let last = (first + chunk).min(per_client);
                    let keys = (first..last)
                        .map(|i| key(client * per_client + i))
                        .collect();
                    let started = Instant::now();
                    op(keys).await;
                    latencies.push(started.elapsed());
                }
                latencies
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let keys_per_s = (clients * per_client) as f64 / start.elapsed().as_secs_f64();
    latencies.sort();
    let at = |q: f64| latencies[((latencies.len() as f64 * q) as usize).min(latencies.len() - 1)];
    println!(
        "| {name} | {clients} | {keys_per_s:.0} | {:.1?} | {:.1?} |",
        at(0.5),
        at(0.99)
    );
}

#[tokio::main]
async fn main() {
    let total: usize = std::env::args()
        .nth(1)
        .map(|n| n.parse().expect("requests per measurement"))
        .unwrap_or(50_000);

    println!("| variant | clients | keys/s | p50 | p99 |\n|---|---|---|---|---|");
    for clients in [1, 16, 64, 256, 1024, 4096] {
        let per_client = (total / clients).clamp(50, 5_000);
        direct(clients, per_client).await;
        caller("enrich", clients, per_client).await;
        caller("enrich_parallel", clients, per_client).await;
    }
    for clients in [1, 4, 16, 64] {
        let per_client = (total / clients).max(BATCH * 8);
        direct_any(clients, per_client).await;
        direct_in(clients, per_client).await;
        call_many("enrich", clients, per_client).await;
        call_many("enrich_parallel", clients, per_client).await;
    }
}
