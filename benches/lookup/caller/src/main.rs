//! One user per request from Postgres: one query per request with sqlx, or `Caller::call` into
//! an mq-bridge route that answers all calls waiting at that moment with one query.
//! Pipelined tokio-postgres and async-graphql's DataLoader are measured for comparison.
//!
//! Needs the lookup bench's Postgres (`docker compose up -d --wait` in `benches/lookup`) and
//! `PGUSER` / `PGPASSWORD` set to the credentials in `benches/lookup/docker-compose.yml`.
//! `cargo run --release -- [requests per cell, 50000] [variants, all] [runs, 3]`, variants as a
//! comma-separated list of:
//!
//! - `direct`: one `= $1` query per request on a sqlx pool of 64 connections.
//! - `mq_bridge`: one `call` per request into the route below.
//! - `pipelined16` / `pipelined64`: one `= $1` query per request on that many tokio-postgres
//!   connections, round robin; concurrent queries on one connection are pipelined.
//! - `dataloader` / `dataloader_in`: async-graphql's DataLoader, one `= ANY($1)` or
//!   `IN ($1…$n)` per batch, on the 64-connection pool.
//! - `direct_pool192`: `direct` on 192 connections (1,024 and 4,096 clients only).
//!
//! Each cell runs `runs` times and prints the median requests/s with the range, p50 and p99.
//! Every answer is checked against its key.

use anyhow::Context;
use async_graphql::dataloader::{DataLoader, Loader};
use mq_bridge::Caller;
use serde::Deserialize;
use std::collections::HashMap;
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

/// User and password come from `PGUSER` / `PGPASSWORD`.
const PG_URL: &str = "postgres://localhost:55432/bench";
const BY_KEY: &str = "SELECT path, id, name, tier FROM users WHERE path = $1";

// ---------------------------------------------------------------------------------------------
// direct: one query per request

async fn direct(connections: u32, clients: usize, per_client: usize) -> Stats {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(connections)
        .test_before_acquire(false)
        .connect(PG_URL)
        .await
        .unwrap();

    let stats = measure(clients, per_client, &pool, |pool, key| async move {
        let user: User = sqlx::query_as(BY_KEY).bind(&key).fetch_one(&pool).await?;
        Ok(user)
    })
    .await;

    pool.close().await;
    stats
}

// ---------------------------------------------------------------------------------------------
// mq-bridge: one call per request, one query per batch of waiting calls

/// The calls waiting at the same moment are read as one batch (up to 256), and `lookup`
/// answers the whole batch with one query (`IN (…)` runs as `= ANY($1)` on Postgres).
/// `parallel` keeps up to 16 batches in flight, sharing the pool of 8 connections.
const ROUTE: &str = r#"
batch_size: 256
concurrency: 4
input:
  middlewares:
    - lookup:
        from:
          sqlx:
            url: "postgres://localhost:55432/bench"
            table: users
            max_connections: 8
            lookup_query: "SELECT path, id, name, tier FROM users WHERE path IN (${payload:path})"
        into: $
    - parallel: { max_in_flight: 16 }
"#;

async fn mq_bridge(clients: usize, per_client: usize) -> Stats {
    let users = Caller::from_str(ROUTE, "users").await.unwrap();

    let stats = measure(clients, per_client, &users, |users, key| async move {
        let user: User = users.call([("path", &key)]).await?;
        Ok(user)
    })
    .await;

    users.stop().await;
    stats
}

// ---------------------------------------------------------------------------------------------
// pipelined: one query per request, several in flight on each connection

struct Pipelines {
    connections: Vec<(tokio_postgres::Client, tokio_postgres::Statement)>,
    next: AtomicUsize,
}

async fn pipelined(connections: usize, clients: usize, per_client: usize) -> Stats {
    let mut config: tokio_postgres::Config = PG_URL.parse().unwrap();
    if let Ok(user) = std::env::var("PGUSER") {
        config.user(user);
    }
    if let Ok(password) = std::env::var("PGPASSWORD") {
        config.password(password);
    }
    let mut opened = Vec::with_capacity(connections);
    for _ in 0..connections {
        let (client, connection) = config.connect(tokio_postgres::NoTls).await.unwrap();
        tokio::spawn(connection);
        let statement = client.prepare(BY_KEY).await.unwrap();
        opened.push((client, statement));
    }
    let pipelines = Arc::new(Pipelines {
        connections: opened,
        next: AtomicUsize::new(0),
    });

    measure(clients, per_client, &pipelines, |p, key| async move {
        let n = p.next.fetch_add(1, Ordering::Relaxed) % p.connections.len();
        let (client, statement) = &p.connections[n];
        let row = client.query_one(statement, &[&key]).await?;
        Ok(User {
            path: row.get(0),
            id: row.get(1),
            name: row.get(2),
            tier: row.get(3),
        })
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// dataloader: one load_one per request, one query per batch

struct UserLoader {
    pool: sqlx::PgPool,
    in_list: bool,
}

impl Loader<String> for UserLoader {
    type Value = User;
    type Error = Arc<sqlx::Error>;

    async fn load(&self, keys: &[String]) -> Result<HashMap<String, User>, Self::Error> {
        let users: Vec<User> = if self.in_list {
            // `IN ($1, …, $n)` is a different statement for every batch size.
            let params: Vec<String> = (1..=keys.len()).map(|i| format!("${i}")).collect();
            let sql = format!(
                "SELECT path, id, name, tier FROM users WHERE path IN ({})",
                params.join(",")
            );
            let mut query = sqlx::query_as(sqlx::AssertSqlSafe(sql.as_str())).persistent(false);
            for key in keys {
                query = query.bind(key);
            }
            query.fetch_all(&self.pool).await?
        } else {
            sqlx::query_as("SELECT path, id, name, tier FROM users WHERE path = ANY($1)")
                .bind(keys)
                .fetch_all(&self.pool)
                .await?
        };
        Ok(users.into_iter().map(|u| (u.path.clone(), u)).collect())
    }
}

async fn dataloader(in_list: bool, clients: usize, per_client: usize) -> Stats {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(64)
        .test_before_acquire(false)
        .connect(PG_URL)
        .await
        .unwrap();
    let loader = Arc::new(
        DataLoader::new(
            UserLoader {
                pool: pool.clone(),
                in_list,
            },
            tokio::spawn,
        )
        .delay(Duration::ZERO)
        .max_batch_size(256),
    );

    let stats = measure(clients, per_client, &loader, |loader, key| async move {
        loader.load_one(key).await?.context("user not found")
    })
    .await;

    pool.close().await;
    stats
}

// ---------------------------------------------------------------------------------------------
// measuring

/// A user key spread over the table's one million rows.
fn key(n: usize) -> String {
    format!("/u/{:06}", n.wrapping_mul(2_654_435_761) % 1_000_000)
}

struct Stats {
    requests_per_s: f64,
    p50: Duration,
    p99: Duration,
}

/// Runs `clients` tasks that each look up `per_client` users one after another with
/// `lookup(ctx, key)`, checks every answer, and returns requests/s and per-request latency.
async fn measure<C, F, Fut>(clients: usize, per_client: usize, ctx: &C, lookup: F) -> Stats
where
    C: Clone + Send + 'static,
    F: Fn(C, String) -> Fut + Copy + Send + 'static,
    Fut: Future<Output = anyhow::Result<User>> + Send + 'static,
{
    let start = Instant::now();
    let tasks: Vec<_> = (0..clients)
        .map(|client| {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                let mut latencies = Vec::with_capacity(per_client);
                for i in 0..per_client {
                    let key = key(client * per_client + i);
                    let started = Instant::now();
                    let user = lookup(ctx.clone(), key.clone()).await.unwrap();
                    latencies.push(started.elapsed());
                    assert_eq!(user.path, key);
                }
                latencies
            })
        })
        .collect();
    let mut latencies: Vec<Duration> = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    let requests_per_s = (clients * per_client) as f64 / start.elapsed().as_secs_f64();
    latencies.sort();
    let at = |q: f64| latencies[((latencies.len() as f64 * q) as usize).min(latencies.len() - 1)];
    Stats {
        requests_per_s,
        p50: at(0.5),
        p99: at(0.99),
    }
}

/// Prints the median of `runs` (requests/s with min–max, p50, p99).
fn report(name: &str, clients: usize, mut runs: Vec<Stats>) {
    let median = |v: &mut Vec<f64>| {
        v.sort_by(f64::total_cmp);
        v[v.len() / 2]
    };
    let mut rate: Vec<f64> = runs.iter().map(|s| s.requests_per_s).collect();
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
        .map(|n| n.parse().expect("requests per cell"))
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

    println!("| variant | clients | requests/s (min–max) | p50 | p99 |\n|---|---|---|---|---|");
    for clients in [1, 16, 64, 256, 1024, 4096] {
        let per_client = (total / clients).clamp(50, 5_000);
        if run("direct") {
            cell!("direct", clients, runs, direct(64, clients, per_client));
        }
        if run("mq_bridge") {
            cell!("mq_bridge", clients, runs, mq_bridge(clients, per_client));
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
        for (name, in_list) in [("dataloader", false), ("dataloader_in", true)] {
            if run(name) {
                cell!(
                    name,
                    clients,
                    runs,
                    dataloader(in_list, clients, per_client)
                );
            }
        }
        if run("direct_pool192") && clients >= 1024 {
            cell!(
                "direct_pool192",
                clients,
                runs,
                direct(192, clients, per_client)
            );
        }
    }
}
