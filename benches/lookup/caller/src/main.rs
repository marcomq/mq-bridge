//! An application that needs one user per request: query Postgres itself, or call a route.
//!
//! Needs the lookup bench's Postgres (`docker compose up -d --wait` in `benches/lookup`) and
//! `PGUSER` / `PGPASSWORD` set to the credentials in `benches/lookup/docker-compose.yml`.
//! Run with `cargo run --release -- [requests per measurement, default 50000] [variants] [runs]`,
//! variants as a comma-separated list of the names below (default: all, or `all`). Each cell
//! runs `runs` times (default 3) and prints the median keys/s with the range, and median latencies.
//!
//! - `direct`: one `= $1` query per request on a sqlx pool of 64 connections.
//! - `pipelined16` / `pipelined64`: one `= $1` query per request on that many tokio-postgres
//!   connections, round robin; concurrent queries on one connection are pipelined.
//! - `direct_pool192`: the same on 192 connections (1,024 and 4,096 clients; Postgres allows 200).
//! - `dataloader_1ms` / `dataloader_0ms`: `load_one` on async-graphql's `DataLoader` with that
//!   `delay`, `max_batch_size` 256, one `= ANY($1)` per batch on the 64-connection pool.
//! - `dataloader_any_text` / `dataloader_in` / `dataloader_in_cached`: `dataloader_0ms` with
//!   the array as one text literal, or `IN ($1…$n)` prepared per call or cached.
//! - `users` / `users_parallel`: `call` into that route of `routes.yaml`, whose `lookup`
//!   sends one query for all calls waiting at that moment. `users_parallel32` /
//!   `users_parallel_b512` / `users_parallel_cap` vary its lanes,
//!   batch size and input capacity.
//! - `direct_any` / `call_many`: a client holding 256 keys at once, as one `= ANY($1)` query
//!   or one `call_many`.
//!
//! Every answer is checked against its key.

use async_graphql::dataloader::{DataLoader, Loader};
use mq_bridge::Caller;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Deserialize, sqlx::FromRow)]
#[allow(dead_code)]
struct User {
    path: String,
    id: i64,
    name: String,
    tier: String,
}

const ROUTES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/routes.yaml");
/// Same URL as in `routes.yaml`; user and password come from `PGUSER` / `PGPASSWORD`.
const PG_URL: &str = "postgres://localhost:55432/bench";
const MANY: usize = 256;

async fn deploy(route: &str) -> Caller {
    Caller::from_file(ROUTES, route).await.unwrap()
}

/// A user key spread over the table's one million rows.
fn key(n: usize) -> String {
    format!("/u/{:06}", n.wrapping_mul(2_654_435_761) % 1_000_000)
}

async fn pool_of(connections: u32) -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(connections)
        .test_before_acquire(false)
        .connect(PG_URL)
        .await
        .unwrap()
}

async fn pool() -> sqlx::PgPool {
    pool_of(64).await
}

struct Stats {
    keys_per_s: f64,
    p50: Duration,
    p99: Duration,
}

async fn direct(connections: u32, clients: usize, per_client: usize) -> Stats {
    let pool = pool_of(connections).await;
    let handle = pool.clone();
    let stats = measure(clients, per_client, 1, move |keys| {
        let pool = handle.clone();
        async move {
            let user: User =
                sqlx::query_as("SELECT path, id, name, tier FROM users WHERE path = $1")
                    .bind(&keys[0])
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(user.path, keys[0]);
        }
    })
    .await;
    pool.close().await;
    stats
}

async fn pipelined(connections: usize, clients: usize, per_client: usize) -> Stats {
    let mut config: tokio_postgres::Config = PG_URL.parse().unwrap();
    if let Ok(user) = std::env::var("PGUSER") {
        config.user(user);
    }
    if let Ok(password) = std::env::var("PGPASSWORD") {
        config.password(password);
    }
    let mut conns = Vec::with_capacity(connections);
    for _ in 0..connections {
        let (client, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        let stmt = client
            .prepare("SELECT path, id, name, tier FROM users WHERE path = $1")
            .await
            .unwrap();
        conns.push((client, stmt));
    }
    let conns = Arc::new(conns);
    let next = Arc::new(AtomicUsize::new(0));
    measure(clients, per_client, 1, move |keys| {
        let (conns, next) = (conns.clone(), next.clone());
        async move {
            let (client, stmt) = &conns[next.fetch_add(1, Ordering::Relaxed) % conns.len()];
            let row = client.query_one(stmt, &[&keys[0]]).await.unwrap();
            assert_eq!(row.get::<_, &str>(0), keys[0]);
        }
    })
    .await
}

/// How a DataLoader batch asks Postgres.
#[derive(Clone, Copy)]
enum Form {
    /// `= ANY($1)` with a text array, one cached statement.
    Any,
    /// `= ANY($1::text[])` with the array as one text literal, as a driver without arrays can.
    AnyText,
    /// `IN ($1…$n)`, cached per list length or prepared per call (as the route's lookup).
    In { cached: bool },
}

struct UserLoader {
    pool: sqlx::PgPool,
    form: Form,
}

/// `{"a","b"}`: a Postgres array literal of text keys.
fn array_literal(keys: &[String]) -> String {
    let quoted: Vec<String> = keys
        .iter()
        .map(|k| format!("\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("{{{}}}", quoted.join(","))
}

impl Loader<String> for UserLoader {
    type Value = User;
    type Error = Arc<sqlx::Error>;

    async fn load(&self, keys: &[String]) -> Result<HashMap<String, User>, Self::Error> {
        const ANY: &str = "SELECT path, id, name, tier FROM users WHERE path = ANY($1)";
        const ANY_TEXT: &str =
            "SELECT path, id, name, tier FROM users WHERE path = ANY($1::text[])";
        let users: Vec<User> = match self.form {
            Form::Any => sqlx::query_as(ANY).bind(keys).fetch_all(&self.pool).await?,
            Form::AnyText => {
                sqlx::query_as(ANY_TEXT)
                    .bind(array_literal(keys))
                    .fetch_all(&self.pool)
                    .await?
            }
            Form::In { cached } => {
                let params: Vec<String> = (1..=keys.len()).map(|i| format!("${i}")).collect();
                let sql = format!(
                    "SELECT path, id, name, tier FROM users WHERE path IN ({})",
                    params.join(",")
                );
                let mut query =
                    sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str())).persistent(cached);
                for key in keys {
                    query = query.bind(key);
                }
                query.fetch_all(&self.pool).await?
            }
        };
        Ok(users.into_iter().map(|u| (u.path.clone(), u)).collect())
    }
}

async fn dataloader(delay: Duration, form: Form, clients: usize, per_client: usize) -> Stats {
    let pool = pool().await;
    let loader = Arc::new(
        DataLoader::new(
            UserLoader {
                pool: pool.clone(),
                form,
            },
            tokio::spawn,
        )
        .delay(delay)
        .max_batch_size(256),
    );
    let stats = measure(clients, per_client, 1, move |keys| {
        let loader = loader.clone();
        async move {
            let user = loader.load_one(keys[0].clone()).await.unwrap().unwrap();
            assert_eq!(user.path, keys[0]);
        }
    })
    .await;
    pool.close().await;
    stats
}

async fn call(route: &str, clients: usize, per_client: usize) -> Stats {
    let users = deploy(route).await;
    let handle = users.clone();
    let stats = measure(clients, per_client, 1, move |keys| {
        let users = handle.clone();
        async move {
            let user: User = users.call([("path", &keys[0])]).await.unwrap();
            assert_eq!(user.path, keys[0]);
        }
    })
    .await;
    users.stop().await;
    stats
}

async fn direct_any(clients: usize, per_client: usize) -> Stats {
    let pool = pool().await;
    measure(clients, per_client, MANY, move |keys| {
        let pool = pool.clone();
        async move {
            let users: Vec<User> =
                sqlx::query_as("SELECT path, id, name, tier FROM users WHERE path = ANY($1)")
                    .bind(&keys)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let found: HashSet<&str> = users.iter().map(|u| u.path.as_str()).collect();
            assert!(keys.iter().all(|k| found.contains(k.as_str())));
        }
    })
    .await
}

async fn call_many(route: &str, clients: usize, per_client: usize) -> Stats {
    let users = deploy(route).await;
    let handle = users.clone();
    let stats = measure(clients, per_client, MANY, move |keys| {
        let users = handle.clone();
        async move {
            let found: Vec<anyhow::Result<User>> =
                users.call_many(keys.iter().map(|k| [("path", k)])).await;
            for (key, user) in keys.iter().zip(found) {
                assert_eq!(&user.unwrap().path, key);
            }
        }
    })
    .await;
    users.stop().await;
    stats
}

/// Runs `clients` tasks that each send `per_client` keys, `chunk` keys per `op`, and returns
/// keys/s plus the latency of one `op`.
async fn measure<F, Fut>(clients: usize, per_client: usize, chunk: usize, op: F) -> Stats
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
    Stats {
        keys_per_s,
        p50: at(0.5),
        p99: at(0.99),
    }
}

/// Prints the median of `runs` (keys/s with min–max, p50, p99).
fn report(name: &str, clients: usize, mut runs: Vec<Stats>) {
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let mut rate: Vec<f64> = runs.iter().map(|s| s.keys_per_s).collect();
    let (min, max) = (
        rate.iter().cloned().fold(f64::MAX, f64::min),
        rate.iter().cloned().fold(0.0, f64::max),
    );
    let rate = median(&mut rate);
    runs.sort_by_key(|s| s.p50);
    let p50 = runs[runs.len() / 2].p50;
    runs.sort_by_key(|s| s.p99);
    let p99 = runs[runs.len() / 2].p99;
    println!("| {name} | {clients} | {rate:.0} ({min:.0}–{max:.0}) | {p50:.1?} | {p99:.1?} |");
}

/// Runs a cell `$runs` times, building a fresh future each time, and reports the median.
macro_rules! cell {
    ($name:expr, $clients:expr, $runs:expr, $run:expr) => {{
        let mut all = Vec::new();
        for _ in 0..$runs {
            all.push($run.await);
        }
        report($name, $clients, all);
    }};
}

#[tokio::main]
async fn main() {
    let total: usize = std::env::args()
        .nth(1)
        .map(|n| n.parse().expect("requests per measurement"))
        .unwrap_or(50_000);
    let only: Option<Vec<String>> = std::env::args()
        .nth(2)
        .map(|names| names.split(',').map(str::to_owned).collect());
    let only = only.filter(|o| o != &["all"]);
    let runs: usize = std::env::args()
        .nth(3)
        .map(|n| n.parse().expect("runs per cell"))
        .unwrap_or(3);
    let run = |name: &str| only.as_ref().is_none_or(|o| o.iter().any(|n| n == name));

    println!("| variant | clients | keys/s (min–max) | p50 | p99 |\n|---|---|---|---|---|");
    for clients in [1, 16, 64, 256, 1024, 4096] {
        let per_client = (total / clients).clamp(50, 5_000);
        if run("direct") {
            cell!("direct", clients, runs, direct(64, clients, per_client));
        }
        if run("direct_pool192") && clients >= 1024 {
            cell!(
                "direct_pool192",
                clients,
                runs,
                direct(192, clients, per_client)
            );
        }
        for (name, connections) in [("pipelined16", 16), ("pipelined64", 64)] {
            if run(name) {
                cell!(
                    name,
                    clients,
                    runs,
                    pipelined(connections, clients, per_client)
                );
            }
        }
        let (ms, zero) = (Duration::from_millis(1), Duration::ZERO);
        for (name, delay, form) in [
            ("dataloader_1ms", ms, Form::Any),
            ("dataloader_0ms", zero, Form::Any),
            ("dataloader_any_text", zero, Form::AnyText),
            ("dataloader_in", zero, Form::In { cached: false }),
            ("dataloader_in_cached", zero, Form::In { cached: true }),
        ] {
            if run(name) {
                cell!(
                    name,
                    clients,
                    runs,
                    dataloader(delay, form, clients, per_client)
                );
            }
        }
        for route in [
            "users",
            "users_parallel",
            "users_parallel32",
            "users_parallel_b512",
            "users_parallel_cap",
        ] {
            if run(route) {
                cell!(route, clients, runs, call(route, clients, per_client));
            }
        }
    }
    for clients in [1, 4, 16, 64] {
        let per_client = (total / clients).max(MANY * 8);
        if run("direct_any") {
            cell!("direct_any", clients, runs, direct_any(clients, per_client));
        }
        for route in ["users", "users_parallel"] {
            let name = format!("call_many {route}");
            if run(&name) {
                cell!(&name, clients, runs, call_many(route, clients, per_client));
            }
        }
    }
}
