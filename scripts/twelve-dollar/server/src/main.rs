//! The $12 server challenge API as one mq-bridge route: `http` input, a batch handler on
//! SQLite (sqlx), `response` output. Every received batch runs in one transaction and every
//! reply waits for its commit (group commit).

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use mq_bridge::endpoints::http::HttpRequestExt;
use mq_bridge::models::{Endpoint, EndpointType, HttpConfig};
use mq_bridge::sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use mq_bridge::sqlx::{self, Sqlite, SqlitePool, Transaction};
use mq_bridge::traits::Handler;
use mq_bridge::{anyhow, CanonicalMessage, Handled, HandlerError, Route};
use sha2::Sha256;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// The subquery already yields rows newest first; an ORDER BY in the aggregate re-sorts them (~6 µs).
const FEED: &str = "SELECT json_object('posts', json_group_array(json_object('id',id,'body',body,\
    'created_at',created_at,'author',username,'like_count',like_count))) FROM (\
    SELECT p.id,p.body,p.created_at,u.username,\
    (SELECT count(*) FROM likes l WHERE l.post_id=p.id) AS like_count \
    FROM posts p JOIN users u ON u.id=p.user_id ORDER BY p.created_at DESC, p.id DESC LIMIT 20)";
const POST: &str =
    "SELECT json_object('post', json_object('id',p.id,'body',p.body,'created_at',p.created_at,\
    'author',u.username,'like_count',(SELECT count(*) FROM likes l WHERE l.post_id=p.id))) \
    FROM posts p JOIN users u ON u.id=p.user_id WHERE p.id = ?";
const CREATE: &str =
    "INSERT INTO posts (user_id, body) VALUES (?, ?) RETURNING json_object('post', \
    json_object('id',id,'body',body,'created_at',created_at,'author',?,'like_count',0))";
const LIKE: &str = "INSERT INTO likes (user_id, post_id) SELECT ?1, ?2 \
    WHERE EXISTS (SELECT 1 FROM posts WHERE id = ?2) ON CONFLICT DO NOTHING";
const EXISTS: &str = "SELECT 1 FROM posts WHERE id = ?";

enum Request {
    Health,
    Feed,
    Post(i64),
    Create {
        user: i64,
        name: String,
        body: String,
    },
    Like {
        user: i64,
        post: i64,
    },
    Fail(u16, &'static str),
}

impl Request {
    fn writes(&self) -> bool {
        matches!(self, Request::Create { .. } | Request::Like { .. })
    }
}

struct Reply(u16, Vec<u8>);

fn error(status: u16, message: &str) -> Reply {
    Reply(status, format!("{{\"error\":\"{message}\"}}").into_bytes())
}

struct Api {
    pool: SqlitePool,
    mac: Hmac<Sha256>,
    started: Instant,
}

impl Api {
    fn parse(&self, msg: &CanonicalMessage) -> Request {
        let path = msg.http_path();
        match (msg.http_method(), path) {
            ("GET", "/health") => Request::Health,
            ("GET", "/feed") => Request::Feed,
            ("GET", p) if p.starts_with("/posts/") && !p[7..].contains('/') => {
                match post_id(&p[7..]) {
                    Some(id) => Request::Post(id),
                    None => Request::Fail(400, "invalid post id"),
                }
            }
            ("POST", "/posts") => match self.user(msg) {
                Err(e) => e,
                Ok((user, name)) => match new_body(&msg.payload) {
                    Ok(body) => Request::Create { user, name, body },
                    Err(e) => e,
                },
            },
            ("POST", p) if p.starts_with("/posts/") && p.ends_with("/like") && p.len() > 12 => {
                match self.user(msg) {
                    Err(e) => e,
                    Ok((user, _)) => match post_id(&p[7..p.len() - 5]) {
                        Some(post) => Request::Like { user, post },
                        None => Request::Fail(400, "invalid post id"),
                    },
                }
            }
            _ => Request::Fail(404, "not found"),
        }
    }

    /// Verifies the HS256 bearer token on every request and returns (user id, username).
    fn user(&self, msg: &CanonicalMessage) -> Result<(i64, String), Request> {
        let token = msg
            .metadata
            .get("mqb.src.http_authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(Request::Fail(401, "missing bearer token"))?;
        let invalid = Request::Fail(401, "invalid or expired token");
        let mut parts = token.splitn(3, '.');
        let (Some(header), Some(payload), Some(signature)) =
            (parts.next(), parts.next(), parts.next())
        else {
            return Err(invalid);
        };
        let header: serde_json::Value = URL_SAFE_NO_PAD
            .decode(header)
            .ok()
            .and_then(|h| serde_json::from_slice(&h).ok())
            .ok_or(Request::Fail(401, "invalid or expired token"))?;
        if header.get("alg").and_then(|a| a.as_str()) != Some("HS256") {
            return Err(invalid);
        }
        let Ok(signature) = URL_SAFE_NO_PAD.decode(signature) else {
            return Err(invalid);
        };
        let mut mac = self.mac.clone();
        mac.update(&token.as_bytes()[..token.len() - signature_len(token)]);
        if mac.verify_slice(&signature).is_err() {
            return Err(invalid);
        }
        let Some(claims) = URL_SAFE_NO_PAD
            .decode(payload)
            .ok()
            .and_then(|p| serde_json::from_slice::<serde_json::Value>(&p).ok())
        else {
            return Err(invalid);
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        match claims.get("exp").and_then(|e| e.as_f64()) {
            Some(exp) if exp > now => {}
            _ => return Err(invalid),
        }
        let user = claims.get("sub").and_then(|s| s.as_str()).and_then(post_id);
        let name = claims.get("username").and_then(|n| n.as_str());
        match (user, name) {
            (Some(user), Some(name)) => Ok((user, name.to_owned())),
            _ => Err(Request::Fail(401, "invalid token payload")),
        }
    }

    async fn run(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        request: Request,
    ) -> Result<Reply, sqlx::Error> {
        Ok(match request {
            Request::Health => {
                sqlx::query("SELECT 1").execute(&mut **tx).await?;
                let up = self.started.elapsed().as_secs();
                Reply(
                    200,
                    format!("{{\"status\":\"ok\",\"db\":\"ok\",\"uptime_s\":{up}}}").into_bytes(),
                )
            }
            Request::Feed => {
                let body: String = sqlx::query_scalar(FEED).fetch_one(&mut **tx).await?;
                Reply(200, body.into_bytes())
            }
            Request::Post(id) => match sqlx::query_scalar::<_, String>(POST)
                .bind(id)
                .fetch_optional(&mut **tx)
                .await?
            {
                Some(body) => Reply(200, body.into_bytes()),
                None => error(404, "post not found"),
            },
            Request::Create { user, name, body } => {
                let body: String = sqlx::query_scalar(CREATE)
                    .bind(user)
                    .bind(body)
                    .bind(name)
                    .fetch_one(&mut **tx)
                    .await?;
                Reply(201, body.into_bytes())
            }
            Request::Like { user, post } => {
                let inserted = sqlx::query(LIKE)
                    .bind(user)
                    .bind(post)
                    .execute(&mut **tx)
                    .await?
                    .rows_affected()
                    > 0;
                if !inserted
                    && sqlx::query(EXISTS)
                        .bind(post)
                        .fetch_optional(&mut **tx)
                        .await?
                        .is_none()
                {
                    return Ok(error(404, "post not found"));
                }
                let (status, again) = if inserted { (201, false) } else { (200, true) };
                Reply(
                    status,
                    format!("{{\"liked\":true,\"already_liked\":{again},\"post_id\":{post}}}")
                        .into_bytes(),
                )
            }
            Request::Fail(status, message) => error(status, message),
        })
    }

    async fn batch(&self, requests: Vec<Request>) -> Vec<Reply> {
        let begin = if requests.iter().any(Request::writes) {
            "BEGIN IMMEDIATE"
        } else {
            "BEGIN"
        };
        let mut tx = match self.pool.begin_with(begin).await {
            Ok(tx) => tx,
            Err(e) => {
                eprintln!("begin failed: {e}");
                return requests
                    .iter()
                    .map(|_| error(500, "internal server error"))
                    .collect();
            }
        };
        let mut replies = Vec::with_capacity(requests.len());
        for request in requests {
            replies.push(self.run(&mut tx, request).await.unwrap_or_else(|e| {
                eprintln!("query failed: {e}");
                error(500, "internal server error")
            }));
        }
        if let Err(e) = tx.commit().await {
            eprintln!("commit failed: {e}");
            return replies
                .iter()
                .map(|_| error(500, "internal server error"))
                .collect();
        }
        replies
    }
}

fn signature_len(token: &str) -> usize {
    token.rsplit_once('.').map_or(0, |(_, s)| s.len() + 1)
}

/// A positive integer without sign, fraction or leading garbage.
fn post_id(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok().filter(|&id| id > 0)
}

fn new_body(payload: &[u8]) -> Result<String, Request> {
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| Request::Fail(400, "malformed JSON body"))?;
    let body = value
        .get("body")
        .and_then(|b| b.as_str())
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .ok_or(Request::Fail(400, "body is required"))?;
    if body.chars().count() > 500 {
        return Err(Request::Fail(400, "body must be at most 500 characters"));
    }
    Ok(body.to_owned())
}

fn to_message(reply: Reply) -> CanonicalMessage {
    CanonicalMessage::from_vec(reply.1)
        .with_metadata_kv("content-type", "application/json")
        .with_metadata_kv("http_status_code", reply.0.to_string())
}

#[async_trait]
impl Handler for Api {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        let mut replies = self.handle_many(vec![msg]).await;
        replies.pop().unwrap_or_else(|| Ok(Handled::Ack))
    }

    async fn handle_many(&self, msgs: Vec<CanonicalMessage>) -> Vec<Result<Handled, HandlerError>> {
        let requests = msgs.iter().map(|m| self.parse(m)).collect();
        self.batch(requests)
            .await
            .into_iter()
            .map(|reply| Ok(Handled::Publish(to_message(reply))))
            .collect()
    }

    fn prefers_batches(&self) -> bool {
        std::env::var("NOBATCH").is_err()
    }
}

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn main() -> anyhow::Result<()> {
    let workers: usize = env("WORKERS", "1").parse().expect("WORKERS");
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(serve())
}

async fn serve() -> anyhow::Result<()> {
    // ROUTE_YAML runs route.yaml (config only) in this binary, for profiling the engine path.
    if let Ok(path) = std::env::var("ROUTE_YAML") {
        let handle = Route::from_file(path, "feed_api")?.run("feed_api").await?;
        tokio::signal::ctrl_c().await?;
        handle.stop().await;
        return Ok(());
    }
    let options = SqliteConnectOptions::new()
        .filename(env("SQLITE_PATH", "feed.db"))
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .pragma("mmap_size", "1073741824");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    let api = Api {
        pool,
        mac: Hmac::new_from_slice(env("JWT_SECRET", "twelve-dollar-challenge").as_bytes())
            .expect("hmac key"),
        started: Instant::now(),
    };
    let addr = format!("{}:{}", env("HOST", "127.0.0.1"), env("PORT", "3000"));
    let mut http = HttpConfig::new(addr);
    // INLINE=1/0 forces the inline path on or off; unset keeps the engine default.
    if let Ok(inline) = std::env::var("INLINE") {
        http = http.with_inline_response_fast_path(inline == "1");
    }
    let input = Endpoint::new(EndpointType::Http(http));
    let batch: usize = env("BATCH_SIZE", "512").parse().expect("BATCH_SIZE");
    let route = Route::new(input, Endpoint::new_response())
        .with_batch_size(batch)
        .with_handler(api);
    let handle = route.run("feed_api").await?;
    tokio::signal::ctrl_c().await?;
    handle.stop().await;
    Ok(())
}
