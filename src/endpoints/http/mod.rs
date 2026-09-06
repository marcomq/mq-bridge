//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

use self::stream::{
    handle_streamable_request, stream_request_format, stream_response_format,
    HttpReceiveStreamConfig, PublishResponseStreamError,
};
use crate::canonical_message::tracing_support::LazyMessageIds;
use crate::models::{Compression, HttpConfig, HttpServerProtocol, TlsConfig};
use crate::support::compression_pool::{gzip_http, lz4_pooled, zstd_pooled};
use crate::traits::{
    BoxFuture, ConsumerError, MessageConsumer, MessagePublisher, ReceivedBatch, Sent,
};
use crate::traits::{CommitFunc, MessageDisposition, PublisherError, SentBatch};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use base64::{engine::general_purpose, Engine as _};
use bytes::Bytes;
use http_body_util::BodyExt;
use http_body_util::StreamBody;
use hyper::{
    body::{Frame, Incoming},
    server::conn::{http1, http2},
    Request, Response, StatusCode,
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::any::Any;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::net::{TcpListener, TcpSocket};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

type HttpSourceMessage = (CanonicalMessage, CommitFunc);

#[derive(Clone, Default)]
struct HttpConnInfo {
    cipher_suite: Option<String>,
    protocol_version: Option<String>,
}

/// Sharded concurrency limiter. A single `Semaphore` shared by every worker
/// turns its permit atomic into a cross-core hot spot at high core counts; we
/// split the budget across N independent semaphores so cores stop contending on
/// one cache line. The aggregate permit count is preserved, so the resource
/// bound is unchanged.
#[derive(Clone)]
struct ConcurrencyLimiter {
    shards: Arc<Vec<Arc<tokio::sync::Semaphore>>>,
}

impl ConcurrencyLimiter {
    fn new(total_permits: usize) -> Self {
        let total = total_permits.max(1);
        let shard_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(total)
            .clamp(1, 16);
        let base = total / shard_count;
        let extra = total % shard_count;
        let shards = (0..shard_count)
            .map(|i| Arc::new(tokio::sync::Semaphore::new(base + usize::from(i < extra))))
            .collect();
        Self {
            shards: Arc::new(shards),
        }
    }

    /// Acquire a permit, preferring the calling thread's shard. Each worker thread
    /// is pinned to one shard on first use (assigned round-robin), so the common
    /// case touches a shard no other core writes to.
    ///
    /// Under skewed load (traffic concentrated on fewer threads than shards) a
    /// saturated local shard would otherwise park while idle shards still hold
    /// free permits, wasting the aggregate budget. To avoid that, first sweep all
    /// shards non-blocking, round-robin from the local index, and only block on the
    /// local shard if every shard is currently full.
    async fn acquire(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError> {
        let shard_count = self.shards.len();
        let local = shard_index() % shard_count;
        for offset in 0..shard_count {
            let idx = (local + offset) % shard_count;
            if let Ok(permit) = self.shards[idx].clone().try_acquire_owned() {
                return Ok(permit);
            }
        }
        // Every shard is saturated: block on the local shard to preserve
        // backpressure and keep the wake-up on a shard no other core writes to.
        self.shards[local].clone().acquire_owned().await
    }
}

/// Stable per-thread shard index, assigned round-robin the first time a thread
/// asks. The shared counter is touched once per thread, never per request.
fn shard_index() -> usize {
    thread_local! {
        static SHARD: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    }
    static NEXT: AtomicU64 = AtomicU64::new(0);
    SHARD.with(|shard| match shard.get() {
        Some(idx) => idx,
        None => {
            let idx = NEXT.fetch_add(1, Ordering::Relaxed) as usize;
            shard.set(Some(idx));
            idx
        }
    })
}

struct RequestMetadataView<'a> {
    method: &'a hyper::Method,
    path: &'a str,
    query: Option<&'a str>,
    version: hyper::Version,
    headers: &'a hyper::HeaderMap,
    conn_info: &'a HttpConnInfo,
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, anyhow::Error>;
use hyper::service::Service;
fn full<T: Into<Bytes>>(chunk: T) -> BoxBody {
    http_body_util::Full::new(chunk.into())
        .map_err(|_| anyhow::anyhow!("Infallible"))
        .boxed()
}

fn streamed<S>(stream: S) -> BoxBody
where
    S: futures::Stream<Item = Result<Frame<Bytes>, anyhow::Error>> + Send + Sync + 'static,
{
    StreamBody::new(stream).boxed()
}

/// A source that listens for incoming HTTP requests using hyper.
pub struct HttpConsumer {
    request_rx: tokio::sync::mpsc::Receiver<HttpSourceMessage>,
    route_id: u64,
    shared_server: Arc<SharedHttpServer>,
    buffer_size: usize,
    url: String,
    bound_addr: Option<SocketAddr>,
    /// Drain mode: only then does an idle recv time out into an empty batch.
    exit_on_empty: bool,
}

impl HttpConsumer {
    pub fn bound_addr(&self) -> Option<SocketAddr> {
        self.bound_addr
    }
}

impl Drop for HttpConsumer {
    fn drop(&mut self) {
        let Ok(mut registry) = http_server_registry().lock() else {
            return;
        };
        let should_shutdown = self.shared_server.router.unregister_route(self.route_id);
        if !should_shutdown {
            return;
        }

        registry.retain(|_, server| !Arc::ptr_eq(server, &self.shared_server));
        let _ = self.shared_server.shutdown_tx.send(());
    }
}

#[derive(Clone)]
struct HttpConsumerState {
    path: Option<String>,
    tx: tokio::sync::mpsc::Sender<HttpSourceMessage>,
    inline_publisher: Option<Arc<dyn MessagePublisher>>,
    /// True when `inline_publisher` is a bare [`ResponsePublisher`], enabling the
    /// metadata-free echo fast path. Precomputed to keep the type check off the hot path.
    inline_echo: bool,
    message_id_header: String,
    request_timeout: std::time::Duration,
    fire_and_forget: bool,
    receive_streamable: bool,
    basic_auth: Option<(String, String)>,
    compression_enabled: bool,
    compression_threshold_bytes: usize,
    custom_headers: HashMap<String, String>,
    concurrency_limit: ConcurrencyLimiter,
    method: Option<hyper::Method>,
}

/// Immutable snapshot of the registered HTTP routes.
///
/// The hot path ([`SharedHttpRouter::match_route`]) reads this without locking;
/// the rare register/unregister operations rebuild it and swap it in atomically.
#[derive(Default)]
struct RouteTable {
    routes: Vec<(u64, Arc<HttpConsumerState>)>,
}

/// Routes every request for a shared listener to the consumer registered for its
/// path+method.
///
/// Reads are lock-free: `match_route` loads an [`ArcSwap`] snapshot. Only the
/// infrequent register/unregister rebuilds take `writers`, a lock the request
/// path never touches. This replaces the previous per-request `Mutex<HashMap>`,
/// which scaled *negatively* with core count (see `benches/router_bench.rs`).
struct SharedHttpRouter {
    snapshot: ArcSwap<RouteTable>,
    writers: Mutex<()>,
}

impl Default for SharedHttpRouter {
    fn default() -> Self {
        Self {
            snapshot: ArcSwap::from_pointee(RouteTable::default()),
            writers: Mutex::new(()),
        }
    }
}

impl SharedHttpRouter {
    fn register_route(&self, route_id: u64, state: Arc<HttpConsumerState>) -> anyhow::Result<()> {
        let _writers = self
            .writers
            .lock()
            .map_err(|_| anyhow!("HTTP route registry lock poisoned"))?;

        let current = self.snapshot.load();
        for (_, existing) in current.routes.iter() {
            if routes_conflict(existing, &state) {
                return Err(anyhow!(
                    "Conflicting HTTP consumer registration for path {:?} and method {:?}",
                    state.path,
                    state.method
                ));
            }
        }

        let mut routes = current.routes.clone();
        routes.push((route_id, state));
        self.snapshot.store(Arc::new(RouteTable { routes }));
        Ok(())
    }

    fn unregister_route(&self, route_id: u64) -> bool {
        let Ok(_writers) = self.writers.lock() else {
            return false;
        };
        let mut routes = self.snapshot.load().routes.clone();
        routes.retain(|(id, _)| *id != route_id);
        let is_empty = routes.is_empty();
        self.snapshot.store(Arc::new(RouteTable { routes }));
        is_empty
    }

    fn match_route(&self, path: &str, method: &hyper::Method) -> anyhow::Result<RouteMatchResult> {
        let table = self.snapshot.load();

        let mut matched_path = false;
        let mut best: Option<&Arc<HttpConsumerState>> = None;
        let mut best_specificity = (0, 0);

        for (_, state) in table.routes.iter() {
            if !route_matches_path(state, path) {
                continue;
            }

            matched_path = true;
            if route_matches_method(state, method) {
                let specificity = route_specificity(state);
                if best.is_none() || specificity > best_specificity {
                    best_specificity = specificity;
                    best = Some(state);
                }
            }
        }

        Ok(match best {
            // One `Arc` clone is unavoidable: the matched state must outlive the
            // request's `.await` points (body read, channel hand-off, ack). It is
            // a single atomic bump — cheap next to the work that follows — and is
            // no longer serialized behind a lock.
            Some(state) => RouteMatchResult::Matched(Arc::clone(state)),
            None if matched_path => {
                let mut methods = table
                    .routes
                    .iter()
                    .filter(|(_, state)| route_matches_path(state, path))
                    .filter_map(|(_, state)| state.method.clone())
                    .collect::<Vec<_>>();
                methods.sort_by(|left, right| left.as_str().cmp(right.as_str()));
                methods.dedup();
                RouteMatchResult::MethodNotAllowed(methods)
            }
            None => RouteMatchResult::NotFound,
        })
    }
}

enum RouteMatchResult {
    Matched(Arc<HttpConsumerState>),
    MethodNotAllowed(Vec<hyper::Method>),
    NotFound,
}

struct SharedHttpServer {
    router: Arc<SharedHttpRouter>,
    shutdown_tx: tokio::sync::watch::Sender<()>,
    bound_addr: Option<SocketAddr>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct HttpServerKey {
    listen_addr: String,
    tls: TlsConfig,
    workers: usize,
    server_protocol: HttpServerProtocol,
}

static HTTP_SERVER_REGISTRY: OnceLock<Mutex<HashMap<HttpServerKey, Arc<SharedHttpServer>>>> =
    OnceLock::new();
static HTTP_ROUTE_ID: AtomicU64 = AtomicU64::new(1);

fn http_server_registry() -> &'static Mutex<HashMap<HttpServerKey, Arc<SharedHttpServer>>> {
    HTTP_SERVER_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn normalize_http_path(path: Option<&str>) -> Option<String> {
    path.map(str::trim)
        .filter(|path| !path.is_empty())
        .map(|path| {
            if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{}", path)
            }
        })
}

/// Guesses an HTTP `Content-Type` from a file path or bare file extension.
///
/// This accepts values like `"index.html"`, `"/assets/app.js"`, `".svg"`, or `"png"`.
/// Unknown extensions default to `application/octet-stream`.
pub fn guess_content_type(path_or_extension: &str) -> &'static str {
    let input = path_or_extension.trim();
    let extension = Path::new(input)
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .or_else(|| input.strip_prefix('.'))
        .unwrap_or(input)
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();

    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" | "jsonld" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "yaml" | "yml" => "application/yaml; charset=utf-8",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "tar" => "application/x-tar",
        "7z" => "application/x-7z-compressed",
        "rar" => "application/vnd.rar",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "png" => "image/png",
        "apng" => "image/apng",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" | "text" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "tsv" => "text/tab-separated-values; charset=utf-8",
        "ics" => "text/calendar; charset=utf-8",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "ogv" => "video/ogg",
        _ => "application/octet-stream",
    }
}

/// Metadata key holding the request method (e.g. `GET`).
pub const HTTP_METHOD: &str = "http_method";
/// Metadata key holding the request path (e.g. `/users`).
pub const HTTP_PATH: &str = "http_path";
/// Metadata key holding the raw request query string, without the leading `?`.
pub const HTTP_QUERY: &str = "http_query";
/// Metadata key holding the negotiated HTTP version (e.g. `HTTP/1.1`).
pub const HTTP_VERSION: &str = "http_version";
/// Metadata key that, when set on a reply, overrides the response status code.
pub const HTTP_STATUS_CODE: &str = "http_status_code";

/// Reads the request line and headers a handler receives from an [`http`]
/// consumer, which exposes them as message metadata. These accessors save a
/// handler from hardcoding the metadata keys and the `metadata.get(..)` dance:
///
/// ```no_run
/// use mq_bridge::endpoints::http::HttpRequestExt;
/// use mq_bridge::CanonicalMessage;
///
/// fn route(msg: &CanonicalMessage) -> i64 {
///     match (msg.http_method(), msg.http_path()) {
///         ("GET", "/sum") => msg.query_int("a").unwrap_or(0) + msg.query_int("b").unwrap_or(0),
///         _ => 0,
///     }
/// }
/// ```
///
/// [`http`]: crate::endpoints::http
pub trait HttpRequestExt {
    /// The request method, or `""` if absent.
    fn http_method(&self) -> &str;
    /// The request path, or `""` if absent.
    fn http_path(&self) -> &str;
    /// The raw query string (no leading `?`), or `""` if absent.
    fn http_query(&self) -> &str;
    /// The raw, still-percent-encoded value of a query parameter, if present.
    fn query_param(&self, key: &str) -> Option<&str>;
    /// A query parameter parsed as an `i64`, if present and numeric.
    fn query_int(&self, key: &str) -> Option<i64>;
    /// Whether the client advertised `Accept-Encoding: gzip`.
    fn accepts_gzip(&self) -> bool;
}

impl HttpRequestExt for CanonicalMessage {
    fn http_method(&self) -> &str {
        self.metadata
            .get(HTTP_METHOD)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn http_path(&self) -> &str {
        self.metadata
            .get(HTTP_PATH)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn http_query(&self) -> &str {
        self.metadata
            .get(HTTP_QUERY)
            .map(String::as_str)
            .unwrap_or("")
    }

    fn query_param(&self, key: &str) -> Option<&str> {
        self.http_query().split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (k == key).then_some(v)
        })
    }

    fn query_int(&self, key: &str) -> Option<i64> {
        self.query_param(key)?.parse().ok()
    }

    fn accepts_gzip(&self) -> bool {
        self.metadata
            .get("accept-encoding")
            .is_some_and(|v| v.to_ascii_lowercase().contains("gzip"))
    }
}

#[cfg(test)]
mod request_ext_tests {
    use super::*;

    fn request(method: &str, path: &str, query: &str) -> CanonicalMessage {
        CanonicalMessage::new(Vec::new(), None)
            .with_metadata_kv(HTTP_METHOD, method)
            .with_metadata_kv(HTTP_PATH, path)
            .with_metadata_kv(HTTP_QUERY, query)
    }

    #[test]
    fn reads_request_line() {
        let msg = request("GET", "/sum", "a=2&b=40");
        assert_eq!(msg.http_method(), "GET");
        assert_eq!(msg.http_path(), "/sum");
        assert_eq!(msg.http_query(), "a=2&b=40");
    }

    #[test]
    fn missing_metadata_reads_as_empty() {
        let msg = CanonicalMessage::new(Vec::new(), None);
        assert_eq!(msg.http_method(), "");
        assert_eq!(msg.query_param("a"), None);
        assert_eq!(msg.query_int("a"), None);
    }

    #[test]
    fn parses_query_params_by_exact_key() {
        let msg = request("GET", "/", "a=2&ab=9&b=40");
        assert_eq!(msg.query_param("a"), Some("2"));
        assert_eq!(msg.query_param("ab"), Some("9"));
        assert_eq!(msg.query_int("b"), Some(40));
        assert_eq!(msg.query_param("missing"), None);
        // A prefix of an existing key must not match it.
        assert_eq!(msg.query_param("c"), None);
    }

    #[test]
    fn detects_gzip_support() {
        let yes = request("GET", "/", "").with_metadata_kv("accept-encoding", "br, GZIP");
        let no = request("GET", "/", "").with_metadata_kv("accept-encoding", "deflate");
        assert!(yes.accepts_gzip());
        assert!(!no.accepts_gzip());
        assert!(!request("GET", "/", "").accepts_gzip());
    }
}

fn routes_conflict(left: &HttpConsumerState, right: &HttpConsumerState) -> bool {
    left.path == right.path
        && (left.method == right.method || left.method.is_none() || right.method.is_none())
}

fn route_matches_path(state: &HttpConsumerState, path: &str) -> bool {
    match &state.path {
        Some(route_path) => route_path == path,
        None => true,
    }
}

fn route_matches_method(state: &HttpConsumerState, method: &hyper::Method) -> bool {
    match &state.method {
        Some(route_method) => route_method == method,
        None => true,
    }
}

fn route_specificity(state: &HttpConsumerState) -> (u8, u8) {
    (
        u8::from(state.path.is_some()),
        u8::from(state.method.is_some()),
    )
}

fn request_accepts_text(headers: &hyper::HeaderMap) -> bool {
    let accept_values = headers.get_all("accept");
    if accept_values.iter().next().is_none() {
        return true;
    }

    accept_values.iter().any(|value| {
        value.to_str().ok().is_some_and(|raw| {
            raw.split(',').any(|item| {
                let media_type = item
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase();
                matches!(media_type.as_str(), "*/*" | "text/*" | "text/plain")
            })
        })
    })
}

/// Returns true if the request's `Accept-Encoding` advertises `coding` (RFC 9110 §12.5.3). A
/// missing header is treated as "identity only" — we do not compress, since RFC 9110 lets the
/// server send identity by default and a client that didn't advertise a coding may be unable to
/// decode it. An explicit `<coding>;q=0` (or `*;q=0`) disables it. `allow_wildcard` controls
/// whether a bare `*` counts as acceptance: true for the universally-decodable `gzip`, false for
/// `lz4` (a non-standard coding a generic `*` client almost certainly cannot decode).
fn request_accepts(headers: &hyper::HeaderMap, coding: &str, allow_wildcard: bool) -> bool {
    headers.get_all("accept-encoding").iter().any(|value| {
        value.to_str().ok().is_some_and(|raw| {
            raw.split(',').any(|item| {
                let mut parts = item.split(';').map(str::trim);
                let item_coding = parts.next().unwrap_or_default().to_ascii_lowercase();
                if item_coding != coding && !(allow_wildcard && item_coding == "*") {
                    return false;
                }
                // Honor an explicit q=0, which excludes the coding.
                !parts.any(|p| {
                    p.strip_prefix("q=")
                        .and_then(|q| q.parse::<f32>().ok())
                        .is_some_and(|q| q == 0.0)
                })
            })
        })
    })
}

/// The best codec the client can decode (`zstd` > `gzip` > `lz4`), or `None` (identity) when it
/// accepts none — so a plain client is never sent a body it cannot read. `lz4`/`zstd` require the
/// explicit token (a bare `*` may predate them); `gzip` is accepted by `*`.
fn negotiate_response_compression(headers: &hyper::HeaderMap) -> Compression {
    if request_accepts(headers, "zstd", false) {
        Compression::Zstd
    } else if request_accepts(headers, "gzip", true) {
        Compression::Gzip
    } else if request_accepts(headers, "lz4", false) {
        Compression::Lz4
    } else {
        Compression::None
    }
}

fn http_version_str(version: hyper::Version) -> &'static str {
    match version {
        hyper::Version::HTTP_09 => "HTTP/0.9",
        hyper::Version::HTTP_10 => "HTTP/1.0",
        hyper::Version::HTTP_11 => "HTTP/1.1",
        hyper::Version::HTTP_2 => "HTTP/2.0",
        hyper::Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/?",
    }
}

/// Credential-bearing request headers. They reach metadata under
/// [`SENSITIVE_HEADER_METADATA_PREFIX`] rather than under their own name, so they stop at the
/// route handler instead of being forwarded.
fn is_sensitive_request_header(name: &str) -> bool {
    const SENSITIVE: [&str; 6] = [
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "x-api-key",
        "x-auth-token",
    ];
    SENSITIVE.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Metadata namespace for credential-bearing request headers, e.g. `mqb.src.http_authorization`,
/// keeping the header name verbatim after the prefix.
///
/// Sits under [`SOURCE_METADATA_PREFIX`](crate::canonical_message::SOURCE_METADATA_PREFIX), which
/// is what makes carrying the value safe: every publisher drops this prefix when serializing
/// metadata, so the route handler sees the header the client actually sent while no sink, and no
/// response header, ever does. See the `sensitive_header_*` tests.
pub const SENSITIVE_HEADER_METADATA_PREFIX: &str = "mqb.src.http_";

fn request_metadata_matches(request: &RequestMetadataView<'_>, key: &str, value: &str) -> bool {
    match key {
        HTTP_METHOD => request.method.as_str() == value,
        HTTP_PATH => request.path == value,
        HTTP_QUERY => request.query.unwrap_or("") == value,
        HTTP_VERSION => http_version_str(request.version) == value,
        "tls_cipher_suite" => request.conn_info.cipher_suite.as_deref() == Some(value),
        "tls_protocol_version" => request.conn_info.protocol_version.as_deref() == Some(value),
        _ => request
            .headers
            .get(key)
            .and_then(|header| header.to_str().ok())
            .is_some_and(|original| original == value),
    }
}

fn has_content_type_header(headers: &HashMap<String, String>) -> bool {
    headers
        .keys()
        .any(|key| key.eq_ignore_ascii_case("content-type"))
}

fn text_error_response(
    status: StatusCode,
    body: impl Into<Bytes>,
    accepts_text: bool,
    custom_headers: Option<&HashMap<String, String>>,
) -> Response<BoxBody> {
    let mut builder = Response::builder().status(status);

    if let Some(custom_headers) = custom_headers {
        for (header_name, header_value) in custom_headers {
            builder = builder.header(header_name.as_str(), header_value.as_str());
        }

        if accepts_text && !has_content_type_header(custom_headers) {
            builder = builder.header("content-type", "text/plain; charset=utf-8");
        }
    } else if accepts_text {
        builder = builder.header("content-type", "text/plain; charset=utf-8");
    }

    builder.body(full(body)).unwrap()
}

/// A `hyper::Service` that can be nested into other services (like Axum).
/// It forwards requests into the `mq-bridge` ecosystem.
#[derive(Clone)]
pub struct HttpBridgeService {
    router: Arc<SharedHttpRouter>,
    conn_info: HttpConnInfo,
}

impl Service<Request<Incoming>> for HttpBridgeService {
    type Response = Response<BoxBody>;
    type Error = anyhow::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let router = self.router.clone();
        let conn_info = self.conn_info.clone();
        Box::pin(handle_request(router, conn_info, req))
    }
}

impl HttpConsumer {
    pub async fn new(config: &HttpConfig) -> anyhow::Result<Self> {
        Self::new_with_inline_publisher(config, None).await
    }

    pub async fn new_with_inline_publisher(
        config: &HttpConfig,
        inline_publisher: Option<Arc<dyn MessagePublisher>>,
    ) -> anyhow::Result<Self> {
        let (request_rx, state, buffer_size) =
            setup_http_state_and_channel(config, inline_publisher)?;
        let listen_address = &config.url;
        let addr: SocketAddr = listen_address
            .parse()
            .with_context(|| format!("Invalid listen address: {}", listen_address))?;

        let tls_config = config.tls.clone();
        let workers = if config.workers.unwrap_or(0) == 0 {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        } else {
            config.workers.unwrap()
        };
        let route_id = HTTP_ROUTE_ID.fetch_add(1, Ordering::Relaxed);
        let server_key = HttpServerKey {
            listen_addr: addr.to_string(),
            tls: tls_config.clone(),
            workers,
            server_protocol: config.server_protocol,
        };
        let shared_server =
            get_or_create_shared_http_server(&server_key, &tls_config, route_id, Arc::new(state))
                .await?;

        Ok(Self {
            request_rx,
            route_id,
            shared_server: shared_server.clone(),
            buffer_size,
            url: build_consumer_target_url(config, shared_server.bound_addr),
            bound_addr: shared_server.bound_addr,
            exit_on_empty: false,
        })
    }
}

/// Helper to set up the shared state and communication channel for the HTTP consumer.
fn setup_http_state_and_channel(
    config: &HttpConfig,
    inline_publisher: Option<Arc<dyn MessagePublisher>>,
) -> anyhow::Result<(
    tokio::sync::mpsc::Receiver<HttpSourceMessage>,
    HttpConsumerState,
    usize,
)> {
    // Default buffer for the per-route receive channel. 100 was small enough to
    // reject bursts with 503s before the pipeline could drain them; 1024 absorbs
    // typical bursts while staying bounded. Override via `internal_buffer_size`.
    let buffer_size = config.internal_buffer_size.unwrap_or(1024).max(1);
    let (request_tx, request_rx) = tokio::sync::mpsc::channel::<HttpSourceMessage>(buffer_size);

    let message_id_header = config
        .message_id_header
        .clone()
        .unwrap_or_else(|| "message-id".to_string());
    let request_timeout =
        std::time::Duration::from_millis(config.request_timeout_ms.unwrap_or(30000));
    let compression_threshold_bytes = config.compression_threshold_bytes.unwrap_or(1024);

    let method = config
        .method
        .as_deref()
        .map(|m| {
            hyper::Method::from_bytes(m.as_bytes())
                .map_err(|_| anyhow::anyhow!("Invalid config.method: '{}'", m))
        })
        .transpose()?;

    // Validate custom header names/values once at startup so a malformed entry is a
    // configuration error instead of a per-request panic in the response builders
    // (which unwrap after `builder.header(...)`).
    for (name, value) in &config.custom_headers {
        hyper::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| anyhow::anyhow!("Invalid custom_headers name '{name}': {e}"))?;
        hyper::header::HeaderValue::from_str(value)
            .map_err(|e| anyhow::anyhow!("Invalid custom_headers value for '{name}': {e}"))?;
    }

    // The `compression` codec is publisher-only and ignored on a consumer, so a codec
    // set without `compression_enabled` would silently disable response compression.
    if config.compression != crate::models::Compression::None
        && config.compression_enabled.is_none()
    {
        tracing::warn!(
            codec = ?config.compression,
            "http consumer: `compression` is a publisher-only codec and is ignored here; \
             set `compression_enabled: true` to compress responses"
        );
    }

    let inline_echo = inline_publisher.as_ref().is_some_and(|publisher| {
        publisher
            .as_any()
            .is::<crate::endpoints::structural::response::ResponsePublisher>()
    });

    let state = HttpConsumerState {
        path: normalize_http_path(config.path.as_deref()),
        tx: request_tx,
        inline_publisher,
        inline_echo,
        message_id_header,
        request_timeout,
        fire_and_forget: config.fire_and_forget,
        receive_streamable: config.receive_streamable,
        basic_auth: config.basic_auth.clone(),
        compression_enabled: config.consumer_compression_enabled(),
        compression_threshold_bytes,
        custom_headers: config.custom_headers.clone(),
        concurrency_limit: ConcurrencyLimiter::new(config.concurrency_limit.unwrap_or(100)),
        method,
    };
    Ok((request_rx, state, buffer_size))
}

fn build_consumer_target_url(config: &HttpConfig, bound_addr: Option<SocketAddr>) -> String {
    let base = config
        .url
        .parse::<SocketAddr>()
        .ok()
        .and_then(|configured_addr| {
            if configured_addr.port() == 0 {
                bound_addr.map(|bound_addr| {
                    SocketAddr::new(configured_addr.ip(), bound_addr.port()).to_string()
                })
            } else {
                None
            }
        })
        .unwrap_or_else(|| config.url.clone());
    let mut url = config.tls.normalize_url(&base);
    if let Some(path) = normalize_http_path(config.path.as_deref()) {
        url.push_str(&path);
    }
    url
}

async fn get_or_create_shared_http_server(
    key: &HttpServerKey,
    tls_config: &TlsConfig,
    route_id: u64,
    state: Arc<HttpConsumerState>,
) -> anyhow::Result<Arc<SharedHttpServer>> {
    let addr: SocketAddr = key
        .listen_addr
        .parse()
        .with_context(|| format!("Invalid listen address: {}", key.listen_addr))?;
    let uses_ephemeral_port = addr.port() == 0;

    if !uses_ephemeral_port {
        if let Ok(registry) = http_server_registry().lock() {
            for (existing_key, server) in registry.iter() {
                if existing_key.listen_addr != key.listen_addr {
                    continue;
                }
                if existing_key == key {
                    server.router.register_route(route_id, state.clone())?;
                    return Ok(server.clone());
                }
                return Err(anyhow!(
                    "HTTP consumer {} is already registered with different TLS or worker settings",
                    key.listen_addr
                ));
            }
        }
    }

    let (listeners, bound_addr) = bind_http_listeners(addr, key.workers).await?;
    let registry_key = if uses_ephemeral_port {
        let Some(bound_addr) = bound_addr else {
            return Err(anyhow!("Failed to determine bound HTTP listener address"));
        };
        HttpServerKey {
            listen_addr: bound_addr.to_string(),
            tls: key.tls.clone(),
            workers: key.workers,
            server_protocol: key.server_protocol,
        }
    } else {
        key.clone()
    };
    let listeners = Arc::new(listeners);
    let router = Arc::new(SharedHttpRouter::default());
    let service = HttpBridgeService {
        router: router.clone(),
        conn_info: HttpConnInfo::default(),
    };
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());

    if tls_config.required {
        if !tls_config.is_tls_server_configured() {
            return Err(anyhow!(
                "HTTP server TLS enabled but no cert/key provided in HttpConfig"
            ));
        }
        info!(
            "Starting shared HTTPS source on {} with {} workers",
            addr, key.workers
        );
        spawn_tls_server(
            listeners,
            service,
            shutdown_rx,
            tls_config,
            key.workers,
            key.server_protocol,
        )
        .await?;
    } else {
        info!(
            "Starting shared HTTP source on {} with {} workers",
            addr, key.workers
        );
        spawn_http_server(
            listeners,
            service,
            shutdown_rx,
            key.workers,
            key.server_protocol,
        )
        .await?;
    }

    let server = Arc::new(SharedHttpServer {
        router,
        shutdown_tx,
        bound_addr,
    });

    let mut registry = http_server_registry()
        .lock()
        .map_err(|_| anyhow!("HTTP server registry lock poisoned"))?;
    for (existing_key, existing) in registry.iter() {
        if existing_key.listen_addr != registry_key.listen_addr {
            continue;
        }
        if existing_key == &registry_key {
            let _ = server.shutdown_tx.send(());
            existing.router.register_route(route_id, state.clone())?;
            return Ok(existing.clone());
        }
        let _ = server.shutdown_tx.send(());
        return Err(anyhow!(
            "HTTP consumer {} is already registered with different TLS or worker settings",
            key.listen_addr
        ));
    }
    server.router.register_route(route_id, state)?;
    registry.insert(registry_key, server.clone());
    Ok(server)
}

/// Whether to bind one `SO_REUSEPORT` listener per worker. Defaults to on for
/// Unix targets (where the kernel hashes connections across the per-worker
/// accept queues, eliminating shared-accept-queue contention) and is overridable
/// via `MQ_BRIDGE_HTTP_REUSEPORT` (`0`/`false`/`off`/`no` to force the shared
/// listener).
fn http_reuseport_enabled() -> bool {
    // `SO_REUSEPORT` only exists on Unix, and `bind_reuseport_listener` only applies the
    // socket option there; the env override must not be able to enable per-worker binds on
    // a non-Unix target (where they would bind with SO_REUSEADDR semantics only).
    #[cfg(unix)]
    {
        match std::env::var("MQ_BRIDGE_HTTP_REUSEPORT") {
            Ok(value) => !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "off" | "no"
            ),
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

fn bind_reuseport_listener(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    #[cfg(all(unix, not(target_os = "solaris"), not(target_os = "illumos")))]
    socket.set_reuseport(true)?;
    socket.bind(addr)?;
    socket.listen(1024)
}

/// Bind the HTTP listener(s) for a shared server.
///
/// With `SO_REUSEPORT` we give each accept worker its own listener so the kernel
/// distributes new connections across independent accept queues — there is no
/// shared-accept-queue contention between workers (the model pyronova's
/// thread-per-core design relies on). When `SO_REUSEPORT` is unavailable, the
/// server is single-worker, or a per-worker bind fails, we fall back to a single
/// shared listener (the previous behavior) so it degrades gracefully. Workers
/// index into the returned vec modulo its length, so a 1-element vec means every
/// worker shares one listener exactly as before.
async fn bind_http_listeners(
    addr: SocketAddr,
    workers: usize,
) -> anyhow::Result<(Vec<TcpListener>, Option<SocketAddr>)> {
    let bind_shared = || async {
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("Failed to bind to {}", addr))?;
        let bound = listener.local_addr().ok();
        anyhow::Ok((vec![listener], bound))
    };

    if workers <= 1 || !http_reuseport_enabled() {
        return bind_shared().await;
    }

    // The first reuseport listener resolves the (possibly ephemeral) port that
    // every sibling must then bind to.
    let first = match bind_reuseport_listener(addr) {
        Ok(listener) => listener,
        Err(err) => {
            warn!("SO_REUSEPORT bind failed ({err}); using a single shared HTTP listener");
            return bind_shared().await;
        }
    };
    let bound = first.local_addr().ok();
    let sibling_addr = bound.unwrap_or(addr);

    let mut listeners = Vec::with_capacity(workers);
    listeners.push(first);
    for _ in 1..workers {
        match bind_reuseport_listener(sibling_addr) {
            Ok(listener) => listeners.push(listener),
            Err(err) => {
                // Some workers got a private listener; the rest share via the
                // modulo index. Still correct, just fewer independent queues.
                warn!(
                    "SO_REUSEPORT sibling bind failed ({err}); {} of {} workers will have a private listener",
                    listeners.len(),
                    workers
                );
                break;
            }
        }
    }
    info!(
        "HTTP server using SO_REUSEPORT: {} listener(s) for {} workers",
        listeners.len(),
        workers
    );
    Ok((listeners, bound))
}

async fn spawn_http_server(
    listeners: Arc<Vec<TcpListener>>,
    service: HttpBridgeService,
    shutdown_rx: tokio::sync::watch::Receiver<()>,
    workers: usize,
    server_protocol: HttpServerProtocol,
) -> anyhow::Result<()> {
    for i in 0..workers {
        let listeners = listeners.clone();
        let listener_idx = i % listeners.len();
        let service = service.clone();
        let mut shutdown_rx = shutdown_rx.clone();

        tokio::spawn(async move {
            trace!("HTTP worker {} started", i);
            let listener = &listeners[listener_idx];
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        trace!("HTTP worker {} shutting down", i);
                        break;
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((socket, _)) => {
                                let _ = socket.set_nodelay(true);
                                let mut conn_service = service.clone();
                                conn_service.conn_info = HttpConnInfo::default();
                                tokio::spawn(async move {
                                    let io = TokioIo::new(socket);
                                    let conn = match server_protocol {
                                        HttpServerProtocol::Auto => {
                                            let mut builder = AutoBuilder::new(TokioExecutor::new());
                                            // `pipeline_flush(true)` coalesces the responses of
                                            // HTTP/1.1 pipelined requests into a single buffered
                                            // write, which raises throughput on pipelined workloads
                                            // (e.g. the TechEmpower plaintext test) and is a no-op
                                            // for non-pipelined traffic.
                                            builder.http1().keep_alive(true).pipeline_flush(true);
                                            builder.http2().max_concurrent_streams(200);
                                            builder
                                                .serve_connection_with_upgrades(io, conn_service)
                                                .await
                                        }
                                        HttpServerProtocol::Http1Only => {
                                            let mut builder = http1::Builder::new();
                                            builder.keep_alive(true).pipeline_flush(true);
                                            builder
                                                .serve_connection(io, conn_service)
                                                .await
                                                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> {
                                                    Box::new(err)
                                                })
                                        }
                                        HttpServerProtocol::Http2Only => {
                                            let mut builder = http2::Builder::new(TokioExecutor::new());
                                            builder.max_concurrent_streams(200);
                                            builder
                                                .serve_connection(io, conn_service)
                                                .await
                                                .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> {
                                                    Box::new(err)
                                                })
                                        }
                                    };
                                    if let Err(e) = conn {
                                        trace!("Connection error: {}", e);
                                    }
                                });
                            }
                            Err(e) => {
                                match e.kind() {
                                    std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                                    | std::io::ErrorKind::TimedOut => {
                                        trace!("Transient accept error in worker {}: {}", i, e);
                                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                                    }
                                    _ if e.raw_os_error() == Some(24) => { // EMFILE (Too many open files)
                                        warn!("HTTP worker {}: FD limit reached, cooling down...", i);
                                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    }
                                    _ => {
                                        warn!("Accept error in worker {}: {}. Retrying...", i, e);
                                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    Ok(())
}

async fn spawn_tls_server(
    listeners: Arc<Vec<TcpListener>>,
    service: HttpBridgeService,
    shutdown_rx: tokio::sync::watch::Receiver<()>,
    tls_config: &TlsConfig,
    workers: usize,
    server_protocol: HttpServerProtocol,
) -> anyhow::Result<()> {
    let rustls_server_config = create_rustls_server_config(tls_config, server_protocol)
        .context("Failed to create rustls server config")?;
    let acceptor = TlsAcceptor::from(rustls_server_config);

    for i in 0..workers {
        let listeners = listeners.clone();
        let listener_idx = i % listeners.len();
        let service = service.clone();
        let acceptor = acceptor.clone();
        let mut shutdown_rx = shutdown_rx.clone();

        tokio::spawn(async move {
            trace!("TLS worker {} started", i);
            let listener = &listeners[listener_idx];
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        trace!("TLS worker {} shutting down", i);
                        break;
                    }
                    result = listener.accept() => {
                        match result {
                            Ok((socket, _)) => {
                                let acceptor = acceptor.clone();
                                let mut conn_service = service.clone();
                                tokio::spawn(async move {
                                    match acceptor.accept(socket).await {
                                        Ok(stream) => {
                                            let mut conn_info = HttpConnInfo::default();
                                            let (_, session) = stream.get_ref();
                                            conn_info.cipher_suite = session.negotiated_cipher_suite().map(|c| format!("{:?}", c.suite()));
                                            conn_info.protocol_version = session.protocol_version().map(|v| format!("{:?}", v));
                                            conn_service.conn_info = conn_info;

                                            let io = TokioIo::new(stream);
                                            let conn = match server_protocol {
                                                HttpServerProtocol::Auto => {
                                                    let mut builder = AutoBuilder::new(TokioExecutor::new());
                                                    // See the plaintext note above: coalesce pipelined
                                                    // HTTP/1.1 responses into one buffered write.
                                                    builder.http1().keep_alive(true).pipeline_flush(true);
                                                    builder.http2().max_concurrent_streams(200);
                                                    builder
                                                        .serve_connection_with_upgrades(io, conn_service)
                                                        .await
                                                }
                                                HttpServerProtocol::Http1Only => {
                                                    let mut builder = http1::Builder::new();
                                                    builder.keep_alive(true).pipeline_flush(true);
                                                    builder
                                                        .serve_connection(io, conn_service)
                                                        .await
                                                        .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> {
                                                            Box::new(err)
                                                        })
                                                }
                                                HttpServerProtocol::Http2Only => {
                                                    let mut builder = http2::Builder::new(TokioExecutor::new());
                                                    builder.max_concurrent_streams(200);
                                                    builder
                                                        .serve_connection(io, conn_service)
                                                        .await
                                                        .map_err(|err| -> Box<dyn std::error::Error + Send + Sync> {
                                                            Box::new(err)
                                                        })
                                                }
                                            };

                                            if let Err(e) = conn {
                                                // Benign errors like "connection closed" are expected
                                                trace!("TLS Connection error: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            debug!("TLS handshake error: {}", e);
                                        }
                                    }
                                });
                            }
                            Err(e) => {
                                match e.kind() {
                                    std::io::ErrorKind::WouldBlock
                                    | std::io::ErrorKind::Interrupted
                                    | std::io::ErrorKind::TimedOut => {
                                        trace!("Transient accept error in TLS worker {}: {}", i, e);
                                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                                    }
                                    _ if e.raw_os_error() == Some(24) => { // EMFILE
                                        warn!("TLS worker {}: FD limit reached, cooling down...", i);
                                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                    }
                                    _ => {
                                        warn!("Accept error in TLS worker {}: {}. Retrying...", i, e);
                                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    Ok(())
}

#[async_trait]
impl MessageConsumer for HttpConsumer {
    // Each request is acked/replied independently (no shared cursor), so commits
    // can run concurrently and out of order.
    fn commit_requires_order(&self) -> bool {
        false
    }
    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        let max_messages = max_messages.max(1);

        // Pull a whole batch in one synchronized operation: `recv_many` blocks
        // for the first message, then drains everything already queued, up to the
        // limit. This is cheaper than `recv().await` plus a `try_recv()` loop (one
        // channel operation instead of N) and coalesces bursts more tightly — the
        // single per-route receiver is otherwise a per-message wake-up point.
        let mut batch: Vec<HttpSourceMessage> = Vec::with_capacity(max_messages);
        let received = self.request_rx.recv_many(&mut batch, max_messages);
        // Drain mode: a brief idle timeout yields an empty batch so --drain can fire.
        let Some(count) = crate::traits::drain_gated(self.exit_on_empty, received).await else {
            return Ok(ReceivedBatch::empty());
        };
        if count == 0 {
            return Err(anyhow!("HTTP source channel closed").into());
        }

        let (messages, commits): (Vec<_>, Vec<_>) = batch.into_iter().unzip();

        // Create a batch commit that handles all collected messages
        let batch_commit: crate::traits::BatchCommitFunc =
            Box::new(move |dispositions: Vec<MessageDisposition>| {
                Box::pin(async move {
                    tracing::trace!(
                        count = dispositions.len(),
                        "Committing batch of HTTP messages"
                    );
                    // Commit each message with its corresponding disposition
                    let mut results = Vec::with_capacity(commits.len());
                    for (commit, disposition) in commits.into_iter().zip(dispositions) {
                        results.push(commit(disposition).await);
                    }
                    results.into_iter().collect::<anyhow::Result<()>>()
                }) as crate::traits::BoxFuture<'static, anyhow::Result<()>>
            });

        Ok(ReceivedBatch {
            messages,
            commit: batch_commit,
        })
    }

    async fn status(&self) -> crate::traits::EndpointStatus {
        crate::traits::EndpointStatus {
            healthy: true,
            target: self.url.clone(),
            pending: Some(self.request_rx.len()),
            capacity: Some(self.buffer_size),
            details: serde_json::json!({ "bound_addr": self.bound_addr }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[tracing::instrument(level = "trace", skip_all)]
async fn handle_request(
    router: Arc<SharedHttpRouter>,
    conn_info: HttpConnInfo,
    req: Request<Incoming>,
) -> anyhow::Result<Response<BoxBody>> {
    let accepts_text = request_accepts_text(req.headers());
    match handle_request_internal(router, conn_info, req, accepts_text).await {
        Ok(res) => Ok(res),
        Err(e) => {
            tracing::error!("Internal error handling HTTP request: {}", e);
            Ok(text_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Internal error: {}", e),
                accepts_text,
                None,
            ))
        }
    }
}

async fn handle_request_internal(
    router: Arc<SharedHttpRouter>,
    conn_info: HttpConnInfo,
    req: Request<Incoming>,
    accepts_text: bool,
) -> anyhow::Result<Response<BoxBody>> {
    let state = match router.match_route(req.uri().path(), req.method())? {
        RouteMatchResult::Matched(state) => state,
        RouteMatchResult::MethodNotAllowed(allowed_methods) => {
            let mut headers = HashMap::new();
            if !allowed_methods.is_empty() {
                headers.insert(
                    "Allow".to_string(),
                    allowed_methods
                        .iter()
                        .map(hyper::Method::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            return Ok(text_error_response(
                StatusCode::METHOD_NOT_ALLOWED,
                format!("Method {} not allowed", req.method()),
                accepts_text,
                Some(&headers),
            ));
        }
        RouteMatchResult::NotFound => {
            return Ok(text_error_response(
                StatusCode::NOT_FOUND,
                "No HTTP consumer registered for this path",
                accepts_text,
                None,
            ));
        }
    };

    // Acquire a permit to limit concurrent requests and prevent resource exhaustion.
    // This applies backpressure to Hyper and prevents task explosion during retry storms.
    let permit = state
        .concurrency_limit
        .acquire()
        .await
        .map_err(|e| anyhow!(e))?;

    if let Some((expected_user, expected_pass)) = configured_basic_auth(state.basic_auth.as_ref()) {
        if let Some(auth_header) = req.headers().get("authorization") {
            match auth_header.to_str() {
                Ok(auth_str) => {
                    if let Some(encoded) = auth_str.strip_prefix("Basic ") {
                        if let Ok(decoded) = general_purpose::STANDARD.decode(encoded) {
                            if let Ok(credentials) = String::from_utf8(decoded) {
                                let (user, pass) = if let Some(colon_pos) = credentials.find(':') {
                                    (&credentials[..colon_pos], &credentials[colon_pos + 1..])
                                } else {
                                    ("", "")
                                };
                                // Constant-time compares (and non-short-circuiting `&`) so
                                // response timing can't reveal how much of the credential matched.
                                let user_ok =
                                    constant_time_eq(user.as_bytes(), expected_user.as_bytes());
                                let pass_ok =
                                    constant_time_eq(pass.as_bytes(), expected_pass.as_bytes());
                                if user_ok & pass_ok {
                                    // Auth successful, proceed
                                } else {
                                    return Ok(text_error_response(
                                        StatusCode::UNAUTHORIZED,
                                        "Invalid credentials",
                                        accepts_text,
                                        None,
                                    ));
                                }
                            } else {
                                return Ok(text_error_response(
                                    StatusCode::BAD_REQUEST,
                                    "Invalid authorization header encoding",
                                    accepts_text,
                                    None,
                                ));
                            }
                        } else {
                            return Ok(text_error_response(
                                StatusCode::BAD_REQUEST,
                                "Invalid base64 encoding in authorization header",
                                accepts_text,
                                None,
                            ));
                        }
                    } else {
                        return Ok(text_error_response(
                            StatusCode::UNAUTHORIZED,
                            "Missing Basic authentication scheme",
                            accepts_text,
                            Some(&basic_auth_challenge_headers()),
                        ));
                    }
                }
                Err(_) => {
                    return Ok(text_error_response(
                        StatusCode::BAD_REQUEST,
                        "Invalid authorization header encoding",
                        accepts_text,
                        None,
                    ));
                }
            }
        } else {
            return Ok(text_error_response(
                StatusCode::UNAUTHORIZED,
                "Missing authorization header",
                accepts_text,
                Some(&basic_auth_challenge_headers()),
            ));
        }
    }

    let (parts, body) = req.into_parts();

    // Pure-echo fast path: skip metadata construction; the response is fully
    // determined by the request. Gate is header-only so the body is untouched.
    if inline_echo_fast_path_applies(&state, &parts.headers) {
        let response_compression = if state.compression_enabled {
            negotiate_response_compression(&parts.headers)
        } else {
            Compression::None
        };
        return inline_echo_response(
            &state,
            &parts.headers,
            body,
            response_compression,
            accepts_text,
            permit,
        )
        .await;
    }

    let request_metadata_view = RequestMetadataView {
        method: &parts.method,
        path: parts.uri.path(),
        query: parts.uri.query(),
        version: parts.version,
        headers: &parts.headers,
        conn_info: &conn_info,
    };

    let mut message_id = None;
    if let Some(header_value) = parts.headers.get(state.message_id_header.as_str()) {
        if let Ok(s) = header_value.to_str() {
            if let Ok(uuid) = Uuid::parse_str(s) {
                message_id = Some(uuid.as_u128());
            } else if let Ok(n) = u128::from_str_radix(s.trim_start_matches("0x"), 16) {
                message_id = Some(n);
            } else if let Ok(n) = s.parse::<u128>() {
                message_id = Some(n);
            }
        }
    }

    // The encoding to use for the response, gated on what the client advertised it can decode.
    let response_compression = if state.compression_enabled {
        negotiate_response_compression(&parts.headers)
    } else {
        Compression::None
    };

    // Extract metadata before consuming body
    let mut metadata = HashMap::with_capacity(parts.headers.len() + 6);
    let mut content_encoding = None;

    metadata.extend([
        (HTTP_METHOD.to_string(), parts.method.to_string()),
        (HTTP_PATH.to_string(), parts.uri.path().to_string()),
        (
            HTTP_QUERY.to_string(),
            parts.uri.query().unwrap_or("").to_string(),
        ),
        (
            HTTP_VERSION.to_string(),
            http_version_str(parts.version).to_string(),
        ),
    ]);

    if let Some(cs) = conn_info.cipher_suite.as_ref() {
        metadata.insert("tls_cipher_suite".to_string(), cs.clone());
    }
    if let Some(pv) = conn_info.protocol_version.as_ref() {
        metadata.insert("tls_protocol_version".to_string(), pv.clone());
    }

    for (key, value) in &parts.headers {
        if let Ok(v_str) = value.to_str() {
            if key.as_str().eq_ignore_ascii_case("content-encoding") {
                content_encoding = Some(v_str.to_string());
            }
            let k_str = key.as_str();
            // `mqb.src.*` is reserved for framework provenance, so a request header may never
            // supply one: without this a client could forge its own `mqb.src.http_authorization`.
            if k_str == HTTP_METHOD
                || k_str == HTTP_PATH
                || k_str == HTTP_QUERY
                || k_str == HTTP_VERSION
                || k_str.eq_ignore_ascii_case("tls_cipher_suite")
                || k_str.eq_ignore_ascii_case("tls_protocol_version")
                || crate::canonical_message::is_source_metadata_key(k_str)
            {
                continue;
            }
            if is_sensitive_request_header(k_str) {
                metadata.insert(
                    format!("{SENSITIVE_HEADER_METADATA_PREFIX}{k_str}"),
                    v_str.to_string(),
                );
                continue;
            }
            metadata.insert(k_str.to_string(), v_str.to_string());
        }
    }

    if state.receive_streamable {
        if content_encoding.is_some() {
            return Ok(text_error_response(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Compressed streamable HTTP requests are not supported",
                accepts_text,
                Some(&state.custom_headers),
            ));
        }

        let request_format = stream_request_format(&parts.headers);
        let response_format = stream_response_format(&parts.headers);
        return handle_streamable_request(
            body,
            metadata,
            HttpReceiveStreamConfig {
                tx: state.tx.clone(),
                inline_publisher: state.inline_publisher.clone(),
                fire_and_forget: state.fire_and_forget,
                request_timeout: state.request_timeout,
                custom_headers: state.custom_headers.clone(),
            },
            request_format,
            response_format,
            accepts_text,
            permit,
        )
        .await;
    }

    // Read body with a timeout to prevent hanging on abandoned client connections.
    // This prevents "zombie" tasks from saturating the runtime during retry storms.
    let body_collect_timeout = state.request_timeout;
    let limited = http_body_util::Limited::new(body, MAX_HTTP_BODY_BYTES as usize);
    let body_bytes = match tokio::time::timeout(body_collect_timeout, limited.collect()).await {
        Ok(Ok(b)) => b.to_bytes(),
        Ok(Err(e)) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                return Ok(text_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Request body exceeds maximum of {MAX_HTTP_BODY_BYTES} bytes"),
                    accepts_text,
                    None,
                ));
            }
            return Ok(text_error_response(
                StatusCode::BAD_REQUEST,
                format!("Failed to read body: {}", e),
                accepts_text,
                None,
            ));
        }
        Err(_) => {
            return Ok(text_error_response(
                StatusCode::REQUEST_TIMEOUT,
                "Timed out reading request body",
                accepts_text,
                None,
            ));
        }
    };

    // Decompress if needed. A malformed or oversized client-controlled body is a client
    // error (4xx), not a server failure — surface it as such instead of letting it bubble
    // up to handle_request's 500 handler.
    let payload = match decompress_if_needed(body_bytes, content_encoding.as_deref()) {
        Ok(payload) => payload,
        Err(e) => {
            return Ok(text_error_response(
                StatusCode::BAD_REQUEST,
                format!("Failed to decompress request body: {}", e),
                accepts_text,
                None,
            ));
        }
    };

    let mut message = CanonicalMessage::new_bytes(payload, message_id);
    trace!(
        message_id = format!("{:032x}", message.message_id),
        "Received HTTP request"
    );

    message.metadata = metadata;

    if let Some(inline_publisher) = state.inline_publisher.as_ref() {
        let timeout_duration = state.request_timeout;
        drop(permit);
        tracing::trace!(
            timeout_ms = timeout_duration.as_millis(),
            "HTTP handler waiting for inline publisher response"
        );
        let disposition =
            match tokio::time::timeout(timeout_duration, inline_publisher.send(message)).await {
                Ok(Ok(Sent::Response(response))) => MessageDisposition::Reply(response),
                Ok(Ok(Sent::Ack)) => MessageDisposition::Ack,
                Ok(Err(err)) => {
                    tracing::warn!("HTTP inline publisher failed: {}", err);
                    MessageDisposition::Nack
                }
                Err(_) => {
                    tracing::warn!(
                        "HTTP handler: inline request timed out after {} ms",
                        timeout_duration.as_millis()
                    );
                    return Ok(text_error_response(
                        StatusCode::GATEWAY_TIMEOUT,
                        "Request timed out",
                        accepts_text,
                        Some(&state.custom_headers),
                    ));
                }
            };

        return make_response(
            disposition,
            response_compression,
            state.compression_threshold_bytes,
            &state.custom_headers,
            accepts_text,
            Some(&request_metadata_view),
        );
    }

    let fire_and_forget = state.fire_and_forget;
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel::<MessageDisposition>();
    let commit = Box::new(move |disposition: MessageDisposition| {
        Box::pin(async move {
            if ack_tx.send(disposition).is_err() && !fire_and_forget {
                trace!("HTTP handler was no longer waiting for commit disposition");
            }
            Ok(())
        }) as BoxFuture<'static, anyhow::Result<()>>
    });

    // Use a timeout for the internal channel send to prevent connection task hang
    // if the bridge pipeline is overloaded. 503 Service Unavailable is returned
    // to allow the client to retry later without holding onto server resources.
    let send_timeout = std::time::Duration::from_millis(2000).min(state.request_timeout / 2);
    match tokio::time::timeout(send_timeout, state.tx.send((message, commit))).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => {
            tracing::error!("Failed to send request to bridge (channel closed): {}", e);
            return Ok(text_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal pipeline closed",
                accepts_text,
                None,
            ));
        }
        Err(_) => {
            tracing::warn!("HTTP handler: internal channel full, request rejected");
            return Ok(text_error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "Server overloaded",
                accepts_text,
                Some(&state.custom_headers),
            ));
        }
    }

    // Release the concurrency permit here, after the message has been handed off to the
    // pipeline. The permit protects against connection/body-read explosion, not against
    // slow downstream processing. Holding it while waiting on ack_rx would cause a
    // deadlock: all permits occupied by requests waiting on downstream, no capacity
    // left for new requests, retry storm fills the channel → everything stalls.
    drop(permit);

    if state.fire_and_forget {
        let mut builder = Response::builder().status(StatusCode::ACCEPTED);
        for (header_name, header_value) in &state.custom_headers {
            builder = builder.header(header_name.as_str(), header_value.as_str());
        }
        return Ok(builder
            .body(full("Message accepted for processing"))
            .unwrap());
    }

    let timeout_duration = state.request_timeout;
    tracing::trace!(
        timeout_ms = timeout_duration.as_millis(),
        "HTTP handler waiting for disposition"
    );
    match tokio::time::timeout(timeout_duration, ack_rx).await {
        Ok(Ok(disposition)) => make_response(
            disposition,
            response_compression,
            state.compression_threshold_bytes,
            &state.custom_headers,
            accepts_text,
            None,
        ),
        Ok(Err(_)) => {
            tracing::error!("HTTP handler: pipeline closed before disposition arrived");
            Ok(text_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Pipeline closed",
                accepts_text,
                Some(&state.custom_headers),
            ))
        }
        Err(_) => {
            tracing::warn!(
                "HTTP handler: request timed out after {} ms",
                timeout_duration.as_millis()
            );
            Ok(text_error_response(
                StatusCode::GATEWAY_TIMEOUT,
                "Request timed out",
                accepts_text,
                Some(&state.custom_headers),
            ))
        }
    }
}

/// Header-only gate for [`inline_echo_response`]. Disqualifies the cases where
/// the slow path would diverge from "200 + echoed content-type + custom headers
/// + (gzipped) body": request `content-encoding`/`transfer-encoding`/SSE
///   content-type (streaming or re-encoding) or an `http_status_code` override.
fn inline_echo_fast_path_applies(state: &HttpConsumerState, headers: &hyper::HeaderMap) -> bool {
    if !state.inline_echo || state.receive_streamable {
        return false;
    }
    if headers.contains_key(hyper::header::CONTENT_ENCODING)
        || headers.contains_key(hyper::header::TRANSFER_ENCODING)
        || headers.contains_key(HTTP_STATUS_CODE)
    {
        return false;
    }
    if let Some(content_type) = headers.get(hyper::header::CONTENT_TYPE) {
        if content_type
            .to_str()
            .is_ok_and(|value| value.contains("text/event-stream"))
        {
            return false;
        }
    }
    true
}

/// Pure-echo response without the per-request metadata HashMap. Only call when
/// [`inline_echo_fast_path_applies`] returned true.
async fn inline_echo_response(
    state: &HttpConsumerState,
    request_headers: &hyper::HeaderMap,
    body: Incoming,
    response_compression: Compression,
    accepts_text: bool,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> anyhow::Result<Response<BoxBody>> {
    let limited = http_body_util::Limited::new(body, MAX_HTTP_BODY_BYTES as usize);
    let body_bytes = match tokio::time::timeout(state.request_timeout, limited.collect()).await {
        Ok(Ok(collected)) => collected.to_bytes(),
        Ok(Err(e)) => {
            if e.downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                return Ok(text_error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("Request body exceeds maximum of {MAX_HTTP_BODY_BYTES} bytes"),
                    accepts_text,
                    None,
                ));
            }
            return Ok(text_error_response(
                StatusCode::BAD_REQUEST,
                format!("Failed to read body: {}", e),
                accepts_text,
                None,
            ));
        }
        Err(_) => {
            return Ok(text_error_response(
                StatusCode::REQUEST_TIMEOUT,
                "Timed out reading request body",
                accepts_text,
                None,
            ));
        }
    };
    // Permit only guards the read, as in the slow path.
    drop(permit);

    let mut builder = Response::builder().status(StatusCode::OK);

    // Echo the request content-type (or octet-stream), matching the slow path.
    if let Some(content_type) = request_headers.get(hyper::header::CONTENT_TYPE) {
        builder = builder.header(hyper::header::CONTENT_TYPE, content_type);
    } else {
        builder = builder.header(hyper::header::CONTENT_TYPE, "application/octet-stream");
    }

    let (payload_out, encoding) = compress_if_needed(
        body_bytes,
        response_compression,
        state.compression_threshold_bytes,
    )?;
    if let Some(token) = encoding {
        builder = builder.header("Content-Encoding", token);
    }

    for (header_name, header_value) in &state.custom_headers {
        builder = builder.header(header_name.as_str(), header_value.as_str());
    }

    Ok(builder.body(full(payload_out)).unwrap())
}

fn make_response(
    disposition: MessageDisposition,
    response_compression: Compression,
    compression_threshold_bytes: usize,
    custom_headers: &HashMap<String, String>,
    accepts_text: bool,
    request_metadata: Option<&RequestMetadataView<'_>>,
) -> anyhow::Result<Response<BoxBody>> {
    match disposition {
        MessageDisposition::Reply(mut msg) => {
            let status = msg
                .metadata
                .remove(HTTP_STATUS_CODE)
                .and_then(|s| s.parse::<u16>().ok())
                .and_then(|code| StatusCode::from_u16(code).ok())
                .unwrap_or(StatusCode::OK);

            let mut builder = Response::builder().status(status);

            let mut has_content_type = false;
            let mut is_streaming = false;
            // An application may pre-encode the body itself (e.g. serving a cached,
            // already-gzipped payload). When it sets `content-encoding` on the reply
            // we honor that: forward the header and skip the server's own compression
            // pass, so the body is never double-encoded.
            let mut preset_encoding: Option<String> = None;
            for (key, value) in &msg.metadata {
                if crate::canonical_message::is_source_metadata_key(key) {
                    continue; // source/provenance keys must not leak as response headers
                }
                let is_content_type = key.eq_ignore_ascii_case("content-type");
                // Request-echo suppression drops reply metadata that byte-matches the incoming
                // request header of the same name (so echo routes don't re-emit `x-request-id`).
                // `content-type` is exempt: it describes the response, so it must always be sent
                // even when it equals the request's — otherwise the reply falls back to
                // `application/octet-stream`.
                if !is_content_type
                    && request_metadata
                        .is_some_and(|metadata| request_metadata_matches(metadata, key, value))
                {
                    continue;
                }
                if is_content_type {
                    has_content_type = true;
                    if value.contains("text/event-stream") {
                        is_streaming = true;
                    }
                } else if key.eq_ignore_ascii_case("transfer-encoding") && value.contains("chunked")
                {
                    is_streaming = true;
                } else if key.eq_ignore_ascii_case("content-encoding")
                    && !value.trim().eq_ignore_ascii_case("identity")
                {
                    preset_encoding = Some(value.clone());
                }

                if !key.eq_ignore_ascii_case("content-encoding")
                    && !key.eq_ignore_ascii_case("transfer-encoding")
                    && !key.eq_ignore_ascii_case("content-length")
                {
                    builder = builder.header(key.as_str(), value.as_str());
                }
            }

            if !has_content_type && status == StatusCode::OK {
                builder = builder.header("content-type", "application/octet-stream");
            }

            // Compress payload only if the application did not pre-encode it, and
            // only when enabled, beneficial, and the client advertised it can
            // decode gzip (RFC 9110 §12.5.3).
            let (payload_out, encoding) = if let Some(preset) = preset_encoding {
                builder = builder.header("Content-Encoding", preset);
                (msg.payload, None)
            } else {
                compress_if_needed(
                    msg.payload,
                    response_compression,
                    compression_threshold_bytes,
                )?
            };

            if let Some(token) = encoding {
                builder = builder.header("Content-Encoding", token);
            }

            // Add custom headers to response
            for (header_name, header_value) in custom_headers {
                builder = builder.header(header_name.as_str(), header_value.as_str());
            }

            if is_streaming {
                let stream = futures::stream::once(async move {
                    Ok::<_, anyhow::Error>(Frame::data(payload_out))
                });
                builder
                    .body(streamed(stream))
                    .map_err(|e| anyhow::anyhow!("Failed to build reply response: {}", e))
            } else {
                builder
                    .body(full(payload_out))
                    .map_err(|e| anyhow::anyhow!("Failed to build reply response: {}", e))
            }
        }
        MessageDisposition::Ack => {
            let mut builder = Response::builder().status(StatusCode::ACCEPTED);
            for (header_name, header_value) in custom_headers {
                builder = builder.header(header_name.as_str(), header_value.as_str());
            }
            Ok(builder.body(full("Message processed")).unwrap())
        }
        MessageDisposition::Nack => Ok(text_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Message processing failed",
            accepts_text,
            Some(custom_headers),
        )),
    }
}

// Helper functions to build rustls configurations from TlsConfig.
// These are placed here because the methods were not found on the TlsConfig struct itself.
// NOTE: These helpers assume `TlsConfig` has fields like `cert_file`, `key_file`, `ca_file`,
// `client_cert_file`, and `client_key_file`. You may need to adjust field names if your
// `TlsConfig` struct in `src/models.rs` is different.

/// Creates a `rustls::ServerConfig` for the HTTPS server.
fn create_rustls_server_config(
    tls_config: &TlsConfig,
    server_protocol: HttpServerProtocol,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    // Ensure a process-level rustls CryptoProvider is installed when building server config.
    // This avoids a runtime panic if the provider wasn't set elsewhere.

    let cert_file = tls_config
        .cert_file
        .as_ref()
        .context("TLS cert_file not provided for server")?;
    let key_file = tls_config
        .key_file
        .as_ref()
        .context("TLS key_file not provided for server")?;

    let certs = load_certs(cert_file)?;
    let key = load_private_key(key_file)?;

    let config_builder =
        rustls::ServerConfig::builder_with_provider(crate::endpoints::get_crypto_provider()?)
            .with_safe_default_protocol_versions()?;

    let mut config = if let Some(ca_file) = &tls_config.ca_file {
        // mTLS: verify client certificates using the provided CA
        let mut client_auth_roots = rustls::RootCertStore::empty();
        let mut pem = BufReader::new(File::open(ca_file).with_context(|| {
            format!(
                "Failed to open CA file for client verification: {}",
                ca_file
            )
        })?);
        for cert in rustls_pemfile::certs(&mut pem) {
            client_auth_roots.add(cert?)?;
        }
        let client_verifier =
            rustls::server::WebPkiClientVerifier::builder(std::sync::Arc::new(client_auth_roots))
                .build()
                .context("Failed to build client certificate verifier")?;
        config_builder
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(certs, key)
            .context("Failed to build rustls mTLS server config")?
    } else {
        // Standard TLS: no client certificate verification
        config_builder
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .context("Failed to build rustls server config")?
    };

    // Advertise via ALPN only the protocol(s) the server will actually serve, so
    // negotiation matches the accept loop's `serve_connection` mode. In Auto mode
    // hyper-util's auto Builder handles both, so advertise h2 with an HTTP/1.1
    // fallback; otherwise constrain ALPN to the single supported protocol.
    config.alpn_protocols = match server_protocol {
        HttpServerProtocol::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpServerProtocol::Http1Only => vec![b"http/1.1".to_vec()],
        HttpServerProtocol::Http2Only => vec![b"h2".to_vec()],
    };

    Ok(Arc::new(config))
}

/// Creates a `rustls::ClientConfig` for the HTTPS client.
fn create_rustls_client_config(tls_config: &TlsConfig) -> anyhow::Result<rustls::ClientConfig> {
    let mut root_cert_store = rustls::RootCertStore::empty();

    if let Some(ca_file) = &tls_config.ca_file {
        let mut pem = BufReader::new(
            File::open(ca_file).with_context(|| format!("Failed to open CA file: {}", ca_file))?,
        );
        for cert in rustls_pemfile::certs(&mut pem) {
            root_cert_store.add(cert?)?;
        }
    } else {
        root_cert_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let config_builder =
        rustls::ClientConfig::builder_with_provider(crate::endpoints::get_crypto_provider()?)
            .with_safe_default_protocol_versions()?
            .with_root_certificates(root_cert_store);

    let mut config =
        if let (Some(cert_file), Some(key_file)) = (&tls_config.cert_file, &tls_config.key_file) {
            let certs = load_certs(cert_file)?;
            let key = load_private_key(key_file)?;
            config_builder
                .with_client_auth_cert(certs, key)
                .context("Failed to build mTLS client config")?
        } else {
            config_builder.with_no_client_auth()
        };

    if tls_config.accept_invalid_certs {
        // INSECURE: disables server certificate and hostname verification, so the
        // connection is open to man-in-the-middle attacks. Honoured only because the
        // caller explicitly opted in via `accept_invalid_certs`; never for production.
        warn!("HTTP client TLS is configured to accept invalid certificates. This is insecure and must not be used in production.");
        let schemes = crate::endpoints::get_crypto_provider()?
            .signature_verification_algorithms
            .supported_schemes();
        config
            .dangerous()
            .set_certificate_verifier(Arc::new(NoopClientCertVerifier {
                supported_schemes: schemes,
            }));
    }

    Ok(config)
}

/// A rustls server-certificate verifier that performs no validation. Used only when
/// `accept_invalid_certs` is set on an HTTP publisher's TLS config.
#[derive(Debug)]
struct NoopClientCertVerifier {
    supported_schemes: Vec<rustls::SignatureScheme>,
}

impl rustls::client::danger::ServerCertVerifier for NoopClientCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.supported_schemes.clone()
    }
}

/// Loads certificate chains from a PEM file.
fn load_certs(path: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    let mut cert_file = BufReader::new(
        File::open(path).with_context(|| format!("Cannot open cert file {}", path))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_file).collect::<Result<Vec<_>, _>>()?;
    Ok(certs)
}

/// Loads a private key from a PEM file.
fn load_private_key(path: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    let mut key_file =
        BufReader::new(File::open(path).with_context(|| format!("Cannot open key file {}", path))?);
    rustls_pemfile::private_key(&mut key_file)?.context("No private key found in file")
}

/// Compresses data with `method` if it exceeds the threshold. Returns the (possibly unchanged)
/// bytes and the `Content-Encoding` token to set, or `None` when the body was left uncompressed.
fn compress_if_needed(
    data: Bytes,
    method: Compression,
    threshold: usize,
) -> anyhow::Result<(Bytes, Option<&'static str>)> {
    if matches!(method, Compression::None) || data.len() < threshold {
        return Ok((data, None));
    }

    let (compressed, token): (Vec<u8>, &'static str) = match method {
        Compression::None => unreachable!(),
        Compression::Gzip => (gzip_http(&data)?, "gzip"),
        Compression::Lz4 => (lz4_pooled(&data).context("lz4 encode failed")?, "lz4"),
        Compression::Zstd => (zstd_pooled(&data, zstd::DEFAULT_COMPRESSION_LEVEL)?, "zstd"),
    };

    // Only use compression if it actually saves space.
    if compressed.len() < data.len() {
        Ok((Bytes::from(compressed), Some(token)))
    } else {
        Ok((data, None))
    }
}

/// Hard cap on both the compressed input and the decompressed output of a request/response
/// body, bounding memory use against an oversized body or a decompression bomb (a tiny
/// payload that expands to gigabytes).
const MAX_HTTP_BODY_BYTES: u64 = 256 * 1024 * 1024;

/// Decompresses the body if the `Content-Encoding` header indicates gzip, lz4, or zstd.
///
/// Both the compressed input and the decompressed output are capped at
/// [`MAX_HTTP_BODY_BYTES`]: an oversized compressed body is rejected before decoding, and
/// decoding stops one byte past the cap so a bomb payload is never fully allocated.
fn decompress_if_needed(data: Bytes, content_encoding: Option<&str>) -> anyhow::Result<Bytes> {
    use std::io::Read;
    let Some(encoding) = content_encoding else {
        return Ok(data);
    };
    let encoding = encoding.to_ascii_lowercase();

    // Bounded reader for each supported codec; unknown encodings pass through unchanged.
    let mut decoder: Box<dyn Read + '_> = if encoding.contains("gzip") {
        Box::new(flate2::read::GzDecoder::new(&data[..]))
    } else if encoding.contains("lz4") {
        Box::new(lz4_flex::frame::FrameDecoder::new(&data[..]))
    } else if encoding.contains("zstd") {
        Box::new(zstd::stream::read::Decoder::new(&data[..])?)
    } else {
        return Ok(data);
    };

    // Reject an oversized compressed body before spending memory decoding it.
    if data.len() as u64 > MAX_HTTP_BODY_BYTES {
        anyhow::bail!(
            "compressed body exceeds maximum allowed size of {MAX_HTTP_BODY_BYTES} bytes"
        );
    }

    // Read one byte past the cap so an over-limit decompressed payload is rejected rather
    // than fully allocated.
    let mut decompressed = Vec::new();
    decoder
        .by_ref()
        .take(MAX_HTTP_BODY_BYTES.saturating_add(1))
        .read_to_end(&mut decompressed)?;
    if decompressed.len() as u64 > MAX_HTTP_BODY_BYTES {
        anyhow::bail!(
            "decompressed body exceeds maximum allowed size of {MAX_HTTP_BODY_BYTES} bytes"
        );
    }
    Ok(Bytes::from(decompressed))
}

/// Encodes data as base64 for HTTP Basic Authentication.
fn base64_encode(data: &[u8]) -> String {
    general_purpose::STANDARD.encode(data)
}

/// Constant-time byte-slice equality: compares every byte regardless of where the first
/// mismatch is, so comparison timing can't leak how many leading bytes of a secret matched.
/// A length difference is not itself secret, so an early length check is fine.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Header map carrying the `WWW-Authenticate` Basic challenge, so a 401 tells the client
/// how to authenticate (RFC 7235 §4.1) instead of failing opaquely.
fn basic_auth_challenge_headers() -> HashMap<String, String> {
    let mut headers = HashMap::with_capacity(1);
    headers.insert(
        "WWW-Authenticate".to_string(),
        "Basic realm=\"mq-bridge\"".to_string(),
    );
    headers
}

fn configured_basic_auth(basic_auth: Option<&(String, String)>) -> Option<(&str, &str)> {
    basic_auth.and_then(|(username, password)| {
        if username.is_empty() && password.is_empty() {
            None
        } else {
            Some((username.as_str(), password.as_str()))
        }
    })
}

fn basic_auth_header_value(basic_auth: Option<&(String, String)>) -> Option<String> {
    configured_basic_auth(basic_auth).map(|(username, password)| {
        let credentials = format!("{}:{}", username, password);
        format!("Basic {}", base64_encode(credentials.as_bytes()))
    })
}

mod publisher;
mod stream;
pub use publisher::HttpPublisher;

#[cfg(test)]
mod tests;
