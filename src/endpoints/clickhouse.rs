//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT License, see License file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! ClickHouse endpoint over the ClickHouse **HTTP interface**.
//!
//! ClickHouse is an OLAP columnar store, not a message queue, so the two roles are asymmetric:
//! - **Publisher (sink):** batch-inserts messages with `FORMAT JSONEachRow`. This is where ClickHouse
//!   shines and matches the bridge's `send_batch`.
//! - **Consumer (source):** reads an existing table **non-destructively** by paging over a monotonic
//!   `cursor_column` (there is no native pub/sub), serializing each row to a JSON payload. Mirrors the
//!   SQLx cursor reader and reuses the shared [`crate::checkpoint`] store for durable resume.
//!
//! We talk raw HTTP (via `reqwest`) rather than the typed `clickhouse` crate because that crate is
//! RowBinary/`Row`-derive only and cannot round-trip arbitrary dynamic JSON. Raw HTTP also avoids the
//! crate's `?`-as-bind-placeholder quirk that would corrupt JSON payloads containing `?`.

use super::poll::PollBackoff;
use crate::checkpoint::{self, CheckpointBackend, CheckpointStore};
use crate::models::{ClickHouseConfig, Compression};
use crate::traits::{
    BoxFuture, ConsumerError, EndpointStatus, MessageConsumer, MessageDisposition,
    MessagePublisher, PublisherError, ReceivedBatch, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, Context};
use async_trait::async_trait;
use percent_encoding::percent_decode_str;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, trace, warn};

/// Validate a ClickHouse identifier. Table names may be schema-qualified (`db.table`); column names
/// may not. Only ASCII alphanumerics and `_` are allowed, keeping interpolation into SQL injection-safe.
fn is_valid_ident(name: &str, allow_dot: bool) -> bool {
    if name.is_empty() || name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || (allow_dot && c == '.'))
}

/// Resolve a publisher column-mapping token for one message into a JSON value.
/// `${payload:<field>}` → top-level payload field (JSON type preserved); `${metadata:<key>}` →
/// metadata string; anything else is a literal string. Unresolvable tokens yield JSON null.
fn resolve_token(
    token: &str,
    msg: &CanonicalMessage,
    payload_json: &Option<serde_json::Value>,
) -> serde_json::Value {
    use serde_json::Value;
    if let Some(inner) = token.strip_prefix("${").and_then(|t| t.strip_suffix('}')) {
        if let Some((prefix, name)) = inner.split_once(':') {
            return match prefix.trim() {
                "payload" => payload_json
                    .as_ref()
                    .and_then(|v| v.get(name.trim()))
                    .cloned()
                    .unwrap_or(Value::Null),
                "metadata" => msg
                    .metadata
                    .get(name.trim())
                    .map(|s| Value::String(s.clone()))
                    .unwrap_or(Value::Null),
                _ => Value::String(token.to_string()),
            };
        }
    }
    Value::String(token.to_string())
}

/// Build the JSON object (one JSONEachRow row) for a single message.
/// With a `columns` mapping, each column is resolved from its token; otherwise the whole payload is
/// used and must itself be a JSON object.
fn build_row(
    msg: &CanonicalMessage,
    columns: &Option<std::collections::BTreeMap<String, String>>,
) -> anyhow::Result<serde_json::Value> {
    let payload_json: Option<serde_json::Value> = serde_json::from_slice(&msg.payload).ok();
    match columns {
        Some(map) => {
            let mut obj = serde_json::Map::with_capacity(map.len());
            for (col, token) in map {
                obj.insert(col.clone(), resolve_token(token, msg, &payload_json));
            }
            Ok(serde_json::Value::Object(obj))
        }
        None => match payload_json {
            Some(v @ serde_json::Value::Object(_)) => Ok(v),
            _ => Err(anyhow!(
                "ClickHouse default insert requires a JSON object payload; set `columns` to map fields for non-object payloads"
            )),
        },
    }
}

/// Whether the URL names this machine, where a clear-text password never leaves it.
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(host)) => host == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// A minimal ClickHouse HTTP client: every statement is a POST whose body is the SQL (plus inline
/// data for inserts). Auth and target database travel as headers/params so payload `?` chars are never
/// treated as bind placeholders.
struct ChClient {
    http: reqwest::Client,
    url: String,
    database: String,
    user: String,
    password: String,
    compression: Compression,
}

impl ChClient {
    fn from_config(config: &ClickHouseConfig) -> anyhow::Result<Self> {
        let mut url = url::Url::parse(&config.url).context("Invalid ClickHouse URL")?;
        if !url.has_host() || !matches!(url.scheme(), "http" | "https") {
            return Err(anyhow!(
                "ClickHouse URL must be an absolute http(s) URL with a host, e.g. 'http://localhost:8123'"
            ));
        }
        let url_username = if url.username().is_empty() {
            None
        } else {
            Some(
                percent_decode_str(url.username())
                    .decode_utf8()
                    .context("ClickHouse URL username is not valid UTF-8")?
                    .into_owned(),
            )
        };
        let url_password = url
            .password()
            .map(|password| {
                percent_decode_str(password)
                    .decode_utf8()
                    .context("ClickHouse URL password is not valid UTF-8")
                    .map(|password| password.into_owned())
            })
            .transpose()?;
        url.set_password(None)
            .map_err(|_| anyhow!("ClickHouse URL cannot contain password userinfo"))?;
        url.set_username("")
            .map_err(|_| anyhow!("ClickHouse URL cannot contain username userinfo"))?;

        // No redirect following: credentials travel as plain `X-ClickHouse-*` headers, which
        // reqwest does not strip on a cross-host hop, and the HTTP interface never redirects.
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_millis(
                config.connect_timeout_ms.unwrap_or(10_000),
            ));
        if let Some(ms) = config.request_timeout_ms {
            builder = builder.timeout(Duration::from_millis(ms));
        }
        if config.tls.accept_invalid_certs {
            builder = builder.danger_accept_invalid_certs(true);
        }
        if let Some(ca) = &config.tls.ca_file {
            let pem = std::fs::read(ca)
                .with_context(|| format!("Failed to read ClickHouse CA file '{}'", ca))?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_pem(&pem)
                    .with_context(|| format!("Invalid ClickHouse CA certificate '{}'", ca))?,
            );
        }
        let http = builder
            .build()
            .context("Failed to build ClickHouse HTTP client")?;
        let password = config.password.clone().or(url_password).unwrap_or_default();
        if !password.is_empty() && url.scheme() == "http" && !is_loopback(&url) {
            warn!(
                host = url.host_str().unwrap_or_default(),
                "ClickHouse password is sent in clear text over http; use an https URL"
            );
        }
        Ok(Self {
            http,
            url: url.as_str().trim_end_matches('/').to_string(),
            database: config.database.clone().unwrap_or_else(|| "default".into()),
            user: config
                .username
                .clone()
                .or(url_username)
                .unwrap_or_else(|| "default".into()),
            password,
            compression: config.compression,
        })
    }

    /// Compresses a request body with the configured method, returning the encoded bytes and the
    /// `Content-Encoding` token. `None` compression returns the bytes unchanged with no token.
    fn encode_body(&self, data: &[u8]) -> anyhow::Result<(Vec<u8>, Option<&'static str>)> {
        use std::io::Write;
        match self.compression {
            Compression::None => Ok((data.to_vec(), None)),
            Compression::Gzip => {
                let mut enc =
                    flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(data)
                    .context("Failed to gzip ClickHouse request body")?;
                Ok((
                    enc.finish().context("Failed to finish gzip encoding")?,
                    Some("gzip"),
                ))
            }
            Compression::Lz4 => {
                let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
                enc.write_all(data)
                    .context("Failed to lz4 ClickHouse request body")?;
                Ok((
                    enc.finish().context("Failed to finish lz4 encoding")?,
                    Some("lz4"),
                ))
            }
            Compression::Zstd => Ok((
                zstd::stream::encode_all(data, zstd::DEFAULT_COMPRESSION_LEVEL)
                    .context("Failed to zstd ClickHouse request body")?,
                Some("zstd"),
            )),
        }
    }

    /// POST `sql` (which may include trailing JSONEachRow data) and return the response body.
    /// `extra` adds request query params (e.g. `async_insert`, `param_*` typed query parameters).
    /// When `compress_body` is set the request body is compressed with the configured method
    /// (`Content-Encoding: gzip`/`lz4`), worth it for large insert bodies. For responses we let the
    /// server compress (via `enable_http_compression`): `gzip` is gunzipped transparently by reqwest's
    /// `gzip` feature, and `lz4` (which reqwest does not know) is decoded here.
    async fn run(
        &self,
        sql: &str,
        extra: &[(&str, &str)],
        compress_body: bool,
    ) -> anyhow::Result<String> {
        let mut params: Vec<(&str, &str)> = vec![("database", self.database.as_str())];
        params.extend_from_slice(extra);
        let mut req = self
            .http
            .post(&self.url)
            .query(&params)
            .header("X-ClickHouse-User", &self.user)
            .header("X-ClickHouse-Key", &self.password);
        // reqwest advertises gzip on its own; for lz4/zstd we must ask explicitly (and doing so also
        // stops reqwest adding its gzip default, so the server won't send a body reqwest would
        // silently gunzip out from under our own decoder).
        match self.compression {
            Compression::Lz4 => req = req.header("Accept-Encoding", "lz4"),
            Compression::Zstd => req = req.header("Accept-Encoding", "zstd"),
            _ => {}
        }
        if compress_body {
            let (encoded, encoding) = self.encode_body(sql.as_bytes())?;
            match encoding {
                Some(token) => req = req.header("Content-Encoding", token).body(encoded),
                None => req = req.body(encoded),
            }
        } else {
            req = req.body(sql.to_string());
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("ClickHouse request to '{}' failed", self.url))?;
        let status = resp.status();
        // reqwest strips `Content-Encoding` after auto-gunzipping, so a surviving encoding here
        // means the raw body still needs decoding: `gzip` (only reachable when we asked for
        // lz4/zstd and the server ignored it), `lz4`, and `zstd` are decoded below; anything
        // else is treated as plain.
        let resp_encoding = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.to_ascii_lowercase());
        let bytes = resp.bytes().await.with_context(|| {
            format!(
                "Failed to read ClickHouse response body from '{}'",
                self.url
            )
        })?;
        let text = match resp_encoding.as_deref() {
            Some("gzip") => {
                // When we asked for lz4/zstd, reqwest's auto-gunzip is disabled, so a server
                // that still answers with gzip leaves a live `Content-Encoding: gzip` we must
                // decode ourselves (using the clickhouse feature's flate2 dependency).
                use std::io::Read;
                let mut decoder = flate2::read::GzDecoder::new(&bytes[..]);
                let mut decoded = Vec::new();
                decoder.read_to_end(&mut decoded).with_context(|| {
                    format!(
                        "Failed to gzip-decode ClickHouse response from '{}'",
                        self.url
                    )
                })?;
                String::from_utf8_lossy(&decoded).into_owned()
            }
            Some("lz4") => {
                let decoded = lz4_decode_all(&bytes).with_context(|| {
                    format!(
                        "Failed to lz4-decode ClickHouse response from '{}'",
                        self.url
                    )
                })?;
                String::from_utf8_lossy(&decoded).into_owned()
            }
            Some("zstd") => {
                let decoded = zstd::stream::decode_all(&bytes[..]).with_context(|| {
                    format!(
                        "Failed to zstd-decode ClickHouse response from '{}'",
                        self.url
                    )
                })?;
                String::from_utf8_lossy(&decoded).into_owned()
            }
            _ => String::from_utf8_lossy(&bytes).into_owned(),
        };
        if !status.is_success() {
            return Err(anyhow!("ClickHouse returned {}: {}", status, text.trim()));
        }
        Ok(text)
    }
}

/// Decodes one or more concatenated lz4 frames (ClickHouse may flush the response as several frames).
fn lz4_decode_all(data: &[u8]) -> std::io::Result<Vec<u8>> {
    use std::io::{BufRead, Read};
    let mut reader = std::io::Cursor::new(data);
    let mut out = Vec::new();
    while !reader.fill_buf()?.is_empty() {
        let mut dec = lz4_flex::frame::FrameDecoder::new(&mut reader);
        dec.read_to_end(&mut out)?;
    }
    Ok(out)
}

// --- Publisher (sink) ---

pub struct ClickHousePublisher {
    client: ChClient,
    table: String,
    columns: Option<std::collections::BTreeMap<String, String>>,
    async_insert: bool,
    wait_for_async_insert: bool,
}

impl ClickHousePublisher {
    pub async fn new(config: &ClickHouseConfig) -> anyhow::Result<Self> {
        if !is_valid_ident(&config.table, true) {
            return Err(anyhow!(
                "Invalid ClickHouse table name: '{}'.",
                config.table
            ));
        }
        if let Some(map) = &config.columns {
            for col in map.keys() {
                if !is_valid_ident(col, false) {
                    return Err(anyhow!("Invalid ClickHouse column name: '{}'.", col));
                }
            }
        }
        let client = ChClient::from_config(config)?;
        client
            .run("SELECT 1", &[], false)
            .await
            .context("ClickHouse publisher connection check failed")?;
        info!(table = %config.table, "ClickHouse publisher connected");
        Ok(Self {
            client,
            table: config.table.clone(),
            columns: config.columns.clone(),
            async_insert: config.async_insert,
            wait_for_async_insert: config.wait_for_async_insert.unwrap_or(true),
        })
    }
}

#[async_trait]
impl MessagePublisher for ClickHousePublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        let mut body = format!("INSERT INTO {} FORMAT JSONEachRow\n", self.table);
        for msg in &messages {
            let row = build_row(msg, &self.columns).map_err(PublisherError::NonRetryable)?;
            let line = serde_json::to_string(&row).map_err(|e| {
                PublisherError::NonRetryable(anyhow!("Failed to serialize row: {}", e))
            })?;
            body.push_str(&line);
            body.push('\n');
        }
        let extra: &[(&str, &str)] = if self.async_insert {
            if self.wait_for_async_insert {
                &[("async_insert", "1"), ("wait_for_async_insert", "1")]
            } else {
                &[("async_insert", "1"), ("wait_for_async_insert", "0")]
            }
        } else {
            &[]
        };
        self.client
            .run(&body, extra, true)
            .await
            .map_err(PublisherError::Retryable)?;
        trace!(count = messages.len(), table = %self.table, "Published batch to ClickHouse");
        Ok(SentBatch::Ack)
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.client.run("SELECT 1", &[], false).await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        EndpointStatus {
            healthy,
            target: self.table.clone(),
            error,
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// --- Consumer (cursor source) ---

/// A resume cursor value; encodes losslessly to/from the checkpoint store's opaque string.
#[derive(Debug, Clone, PartialEq)]
enum ChCursor {
    Int(i64),
    Uint(u64),
    Text(String),
}

impl ChCursor {
    fn encode(&self) -> String {
        match self {
            ChCursor::Int(n) => format!("int:{}", n),
            ChCursor::Uint(n) => format!("uint:{}", n),
            ChCursor::Text(s) => format!("str:{}", s),
        }
    }

    fn decode(s: &str) -> Option<ChCursor> {
        let (tag, val) = s.split_once(':')?;
        match tag {
            "int" => val.parse::<i64>().ok().map(ChCursor::Int),
            "uint" => val.parse::<u64>().ok().map(ChCursor::Uint),
            "str" => Some(ChCursor::Text(val.to_string())),
            _ => None,
        }
    }

    /// ClickHouse typed-parameter type and value for `{last:<ty>}` substitution.
    fn param(&self) -> (&'static str, String) {
        match self {
            ChCursor::Int(n) => ("Int64", n.to_string()),
            ChCursor::Uint(n) => ("UInt64", n.to_string()),
            ChCursor::Text(s) => ("String", s.clone()),
        }
    }
}

/// Extract the cursor value from a JSONEachRow row object. Numbers prefer `Int64`, falling back to
/// `UInt64` for values above `i64::MAX` (ClickHouse's common full-range UInt64 ids).
fn extract_cursor(row: &serde_json::Value, column: &str) -> Option<ChCursor> {
    match row.get(column) {
        Some(serde_json::Value::Number(n)) => n
            .as_i64()
            .map(ChCursor::Int)
            .or_else(|| n.as_u64().map(ChCursor::Uint)),
        Some(serde_json::Value::String(s)) => Some(ChCursor::Text(s.clone())),
        _ => None,
    }
}

pub struct ClickHouseCursorReader {
    client: ChClient,
    table: String,
    cursor_column: String,
    select_columns: String,
    backoff: PollBackoff,
    checkpoint: Option<Arc<dyn CheckpointStore>>,
    last_value: Arc<Mutex<Option<ChCursor>>>,
}

impl ClickHouseCursorReader {
    pub async fn new(config: &ClickHouseConfig) -> anyhow::Result<Self> {
        Self::new_with_no_resume(config, false).await
    }

    pub(crate) async fn new_with_no_resume(
        config: &ClickHouseConfig,
        no_resume: bool,
    ) -> anyhow::Result<Self> {
        if !is_valid_ident(&config.table, true) {
            return Err(anyhow!(
                "Invalid ClickHouse table name: '{}'.",
                config.table
            ));
        }
        let cursor_column = config
            .cursor_column
            .clone()
            .ok_or_else(|| anyhow!("cursor_column is required for the ClickHouse cursor reader"))?;
        if !is_valid_ident(&cursor_column, false) {
            return Err(anyhow!("Invalid cursor_column name: '{}'.", cursor_column));
        }
        let client = ChClient::from_config(config)?;
        client
            .run("SELECT 1", &[], false)
            .await
            .context("ClickHouse cursor reader connection check failed")?;

        // Durable resume needs an external checkpoint store: ClickHouse is unsuited to per-row cursor
        // upserts, so the source-datastore backend is rejected here.
        let checkpoint: Option<Arc<dyn CheckpointStore>> = if no_resume {
            None
        } else if let Some(cid) = &config.cursor_id {
            match &config.checkpoint_store {
                None => {
                    warn!(
                        table = %config.table,
                        "ClickHouse cursor reader has cursor_id but no checkpoint_store; resume is disabled. Set an external checkpoint_store (file://, postgres://, mongodb://) to persist progress."
                    );
                    None
                }
                Some(spec) => match checkpoint::parse_checkpoint_store(spec)? {
                    CheckpointBackend::Source { .. } => {
                        return Err(anyhow!(
                            "ClickHouse cursor reader requires an external checkpoint_store (file://, postgres://, or mongodb://); a source-datastore checkpoint is not supported because ClickHouse cannot cheaply upsert cursor rows."
                        ));
                    }
                    external => {
                        Some(checkpoint::build_external_store(external, &config.table, cid).await?)
                    }
                },
            }
        } else {
            warn!(
                table = %config.table,
                "ClickHouse cursor reader has no cursor_id; resume is disabled and every restart re-copies from the beginning."
            );
            None
        };

        let last_value = match &checkpoint {
            Some(cp) => cp.load().await?.and_then(|s| {
                let decoded = ChCursor::decode(&s);
                if decoded.is_none() {
                    warn!(value = %s, "Ignoring unparseable ClickHouse cursor; starting from beginning");
                }
                decoded
            }),
            None => None,
        };
        // Validate select_columns for the same injection-safe invariant as table/cursor_column, and
        // require the cursor_column to be present so `extract_cursor` can page (a `*` covers it).
        let select_columns = config
            .select_columns
            .clone()
            .unwrap_or_else(|| "*".to_string());
        if select_columns.trim() != "*" {
            let cols: Vec<String> = select_columns
                .split(',')
                .map(|c| c.trim().to_string())
                .collect();
            for c in &cols {
                if !is_valid_ident(c, false) {
                    return Err(anyhow!(
                        "Invalid column '{}' in select_columns: only simple identifiers or '*' are allowed.",
                        c
                    ));
                }
            }
            if !cols.iter().any(|c| c == &cursor_column) {
                return Err(anyhow!(
                    "select_columns must include the cursor_column '{}' so the reader can page by it.",
                    cursor_column
                ));
            }
        }

        info!(table = %config.table, column = %cursor_column, has_checkpoint = %last_value.is_some(), "ClickHouse cursor reader connected");

        Ok(Self {
            client,
            table: config.table.clone(),
            cursor_column,
            select_columns,
            backoff: PollBackoff::new(
                Duration::from_millis(config.polling_interval_ms.unwrap_or(100)),
                config.max_polling_interval_ms.map(Duration::from_millis),
            ),
            checkpoint,
            last_value: Arc::new(Mutex::new(last_value)),
        })
    }
}

#[async_trait]
impl MessageConsumer for ClickHouseCursorReader {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }

        let last = self.last_value.lock().unwrap().clone();
        // Peek one extra row so a run of equal cursor values split across the LIMIT boundary is
        // detected (a `> last` bound would otherwise silently skip the remainder of that run).
        let fetch_limit = max_messages.saturating_add(1);

        let (sql, extra): (String, Vec<(&str, String)>) = match &last {
            Some(cur) => {
                let (ty, val) = cur.param();
                let sql = format!(
                    "SELECT {cols} FROM {table} WHERE {col} > {{last:{ty}}} ORDER BY {col} ASC LIMIT {lim} FORMAT JSONEachRow",
                    cols = self.select_columns,
                    table = self.table,
                    col = self.cursor_column,
                    ty = ty,
                    lim = fetch_limit,
                );
                (sql, vec![("param_last", val)])
            }
            None => {
                let sql = format!(
                    "SELECT {cols} FROM {table} ORDER BY {col} ASC LIMIT {lim} FORMAT JSONEachRow",
                    cols = self.select_columns,
                    table = self.table,
                    col = self.cursor_column,
                    lim = fetch_limit,
                );
                (sql, Vec::new())
            }
        };
        // Ask the server to gzip the (potentially large) result set; reqwest gunzips it transparently.
        // `output_format_json_quote_64bit_integers=0` keeps Int64/UInt64 as JSON numbers (ClickHouse
        // quotes them as strings by default), so numeric cursor values and payload ids stay numeric —
        // otherwise a UInt64 cursor would page by lexicographic string order.
        let mut extra_refs: Vec<(&str, &str)> = vec![
            ("enable_http_compression", "1"),
            ("output_format_json_quote_64bit_integers", "0"),
        ];
        extra_refs.extend(extra.iter().map(|(k, v)| (*k, v.as_str())));

        let body = self
            .client
            .run(&sql, &extra_refs, false)
            .await
            .map_err(ConsumerError::Connection)?;

        // Parse JSONEachRow: one JSON object per non-empty line.
        let mut fetched: Vec<(ChCursor, CanonicalMessage)> = Vec::new();
        for line in body.lines().filter(|l| !l.trim().is_empty()) {
            let row: serde_json::Value = serde_json::from_str(line).map_err(|e| {
                ConsumerError::Connection(anyhow!("Invalid JSONEachRow row: {}", e))
            })?;
            let cursor = extract_cursor(&row, &self.cursor_column).ok_or_else(|| {
                // Schema-level, so re-polling fails identically: permanent, not a reconnect.
                ConsumerError::Permanent(anyhow!(
                    "cursor_column '{}' missing or of unsupported type in result row",
                    self.cursor_column
                ))
            })?;
            let payload = serde_json::to_vec(&row).unwrap_or_default();
            fetched.push((cursor, CanonicalMessage::new(payload, None)));
        }

        if fetched.is_empty() {
            // Drained: keep polling cadence (backing off if configured), then surface an empty batch.
            tokio::time::sleep(self.backoff.idle_delay()).await;
            return Ok(ReceivedBatch {
                messages: Vec::new(),
                commit: Box::new(|_| Box::pin(async { Ok(()) })),
            });
        }
        // Rows arrived: return to the base polling interval.
        self.backoff.reset();

        // Drop the trailing run equal to the peek row's value so a group of equal cursor values is
        // never split across pages; trimmed rows are re-read next poll via `col > last`.
        let had_more = fetched.len() > max_messages;
        let mut emit_len = fetched.len().min(max_messages);
        if had_more {
            let peek_val = fetched[max_messages].0.clone();
            while emit_len > 0 && fetched[emit_len - 1].0 == peek_val {
                emit_len -= 1;
            }
            if emit_len == 0 {
                // The whole page shares one cursor value and more rows with that value exist beyond
                // it. Advancing past the value would silently skip the remainder, so fail loudly
                // instead of losing rows. Permanent: re-polling returns the same page forever.
                return Err(ConsumerError::Permanent(anyhow!(
                    "cursor_column '{}' has a group of equal values larger than batch_size ({}); \
                     cannot page without skipping rows. Increase batch_size above the size of the \
                     largest equal-value group.",
                    self.cursor_column,
                    max_messages
                )));
            }
        }
        fetched.truncate(emit_len);

        let mut messages = Vec::with_capacity(fetched.len());
        let mut cursors: Vec<ChCursor> = Vec::with_capacity(fetched.len());
        for (cursor, msg) in fetched {
            cursors.push(cursor.clone());
            messages.push(msg);
            // Advance optimistically; rolled back in commit if a row is not acked.
            *self.last_value.lock().unwrap() = Some(cursor);
        }
        trace!(
            count = messages.len(),
            "Received batch of ClickHouse cursor rows"
        );

        let checkpoint = self.checkpoint.clone();
        let last_value = self.last_value.clone();
        let resume_from = last; // cursor value before this batch (for rollback on nack)
        let commit = Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                // Count the contiguous run of Acks from the front (stop at first Nack).
                let mut acked = 0usize;
                for disp in dispositions.iter().take(cursors.len()) {
                    if matches!(disp, MessageDisposition::Ack | MessageDisposition::Reply(_)) {
                        acked += 1;
                    } else {
                        break;
                    }
                }
                let boundary = if acked == 0 {
                    resume_from
                } else {
                    Some(cursors[acked - 1].clone())
                };
                // Roll the in-memory read cursor back to the committed boundary so nacked rows are
                // re-read next poll (at-least-once) instead of skipped until a restart.
                if acked < cursors.len() {
                    *last_value.lock().unwrap() = boundary.clone();
                }
                if let (Some(cur), Some(cp)) = (boundary, checkpoint) {
                    if let Err(e) = cp.save(&cur.encode()).await {
                        warn!(error = %e, "Failed to persist ClickHouse cursor. Rows may be reprocessed on restart.");
                    }
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        });

        Ok(ReceivedBatch { messages, commit })
    }

    async fn status(&self) -> EndpointStatus {
        let (healthy, error) = match self.client.run("SELECT 1", &[], false).await {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        EndpointStatus {
            healthy,
            target: self.table.clone(),
            error,
            details: serde_json::json!({ "mode": "cursor_column", "cursor_column": self.cursor_column }),
            ..Default::default()
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(url: &str) -> ClickHouseConfig {
        serde_json::from_value(serde_json::json!({
            "url": url,
            "table": "orders"
        }))
        .unwrap()
    }

    fn msg(payload: &serde_json::Value, meta: &[(&str, &str)]) -> CanonicalMessage {
        let mut m = CanonicalMessage::new(serde_json::to_vec(payload).unwrap(), None);
        for (k, v) in meta {
            m.metadata.insert(k.to_string(), v.to_string());
        }
        m
    }

    #[test]
    fn ident_validation() {
        assert!(is_valid_ident("orders", false));
        assert!(is_valid_ident("db.orders", true));
        assert!(!is_valid_ident("db.orders", false));
        assert!(!is_valid_ident("drop table", false));
        assert!(!is_valid_ident("", false));
        assert!(!is_valid_ident(".x", true));
        assert!(!is_valid_ident("a..b", true));
    }

    #[test]
    fn url_userinfo_supplies_credentials_and_is_removed_from_request_url() {
        let client = ChClient::from_config(&config("http://demo:p%40ss@localhost:8123")).unwrap();

        assert_eq!(client.user, "demo");
        assert_eq!(client.password, "p@ss");
        assert_eq!(client.url, "http://localhost:8123");
    }

    #[test]
    fn explicit_credentials_override_url_userinfo() {
        let mut config = config("http://embedded:secret@localhost:8123");
        config.username = Some("explicit".into());
        config.password = Some("override".into());

        let client = ChClient::from_config(&config).unwrap();

        assert_eq!(client.user, "explicit");
        assert_eq!(client.password, "override");
        assert_eq!(client.url, "http://localhost:8123");
    }

    #[test]
    fn scheme_less_url_reports_the_missing_absolute_url() {
        let error = ChClient::from_config(&config("localhost:8123"))
            .err()
            .expect("scheme-less URL must fail")
            .to_string();

        assert!(
            error.contains("absolute http(s) URL with a host"),
            "{error}"
        );
    }

    #[test]
    fn default_row_requires_object_payload() {
        let m = msg(&serde_json::json!({"id": 1, "sku": "a"}), &[]);
        let row = build_row(&m, &None).unwrap();
        assert_eq!(row, serde_json::json!({"id": 1, "sku": "a"}));

        // Non-object payloads are rejected in default mode.
        let scalar =
            CanonicalMessage::new(serde_json::to_vec(&serde_json::json!(42)).unwrap(), None);
        assert!(build_row(&scalar, &None).is_err());
    }

    #[test]
    fn mapped_row_resolves_tokens() {
        let m = msg(
            &serde_json::json!({"sku": "widget", "qty": 3}),
            &[("customer_id", "c-99")],
        );
        let mut cols = BTreeMap::new();
        cols.insert(
            "customer".to_string(),
            "${metadata:customer_id}".to_string(),
        );
        cols.insert("sku".to_string(), "${payload:sku}".to_string());
        cols.insert("qty".to_string(), "${payload:qty}".to_string());
        cols.insert("source".to_string(), "clickhouse".to_string()); // literal
        cols.insert("missing".to_string(), "${payload:nope}".to_string());

        let row = build_row(&m, &Some(cols)).unwrap();
        assert_eq!(
            row,
            serde_json::json!({
                "customer": "c-99",
                "sku": "widget",
                "qty": 3,            // numeric type preserved
                "source": "clickhouse",
                "missing": null,
            })
        );
    }

    #[test]
    fn cursor_encode_decode_roundtrip() {
        assert_eq!(
            ChCursor::decode(&ChCursor::Int(42).encode()),
            Some(ChCursor::Int(42))
        );
        assert_eq!(
            ChCursor::decode(&ChCursor::Text("2026-01-01".into()).encode()),
            Some(ChCursor::Text("2026-01-01".into()))
        );
        assert_eq!(ChCursor::decode("garbage"), None);
        assert_eq!(ChCursor::Int(7).param(), ("Int64", "7".to_string()));
    }

    #[test]
    fn extract_cursor_from_row() {
        let row = serde_json::json!({"id": 5, "name": "x"});
        assert_eq!(extract_cursor(&row, "id"), Some(ChCursor::Int(5)));
        assert_eq!(
            extract_cursor(&row, "name"),
            Some(ChCursor::Text("x".into()))
        );
        assert_eq!(extract_cursor(&row, "absent"), None);
    }
}
