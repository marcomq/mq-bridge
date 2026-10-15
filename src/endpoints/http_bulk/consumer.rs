//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Input for HTTP APIs that list JSON documents page by page.
//!
//! One request reads one page. Where the next page starts is configuration: a
//! value in the response, a field of the last document, the documents read, or a
//! next-page URL from the response or its `Link` header.
//! With `read.stream` one response stays open and is read event by event.

use super::{quoted, Connection, JSON};
use crate::checkpoint::{self, CheckpointBackend, CheckpointStore, VersionedCheckpoint};
use crate::endpoints::poll::PollBackoff;
use crate::models::{HttpBulkConfig, HttpBulkRead, HttpBulkStream};
use crate::support::http_status;
use crate::support::sse::{find_sse_event_end, parse_sse_event, ParsedSseEvent};
use crate::traits::{
    BatchCommitFunc, BoxFuture, ConsumerError, MessageConsumer, MessageDisposition, ReceivedBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use bytes::Bytes;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{ACCEPT, CONTENT_TYPE, LINK};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, trace, warn};

const CURSOR: &str = "{cursor}";
const LIMIT: &str = "{limit}";
/// Most bytes of a stream held while one event or line is incomplete.
const MAX_ITEM_BYTES: usize = 64 * 1024 * 1024;

/// Where the next page starts. `End` is stored too, so a finished scan stays finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Position {
    At(Value),
    End,
}

/// Where the position after a page comes from.
enum CursorSource {
    /// The number of documents read so far.
    Count,
    /// A JSON pointer into the response; `end_when_missing` reads an absent field as the end.
    Response {
        pointer: String,
        end_when_missing: bool,
    },
    /// A JSON pointer into the last document of the page.
    Item(String),
    /// The id of a server-sent event, sent back as `Last-Event-ID`.
    EventId,
    /// The URL of the next page, at this JSON pointer or, with `None`, in the `Link` header.
    NextUrl(Option<String>),
    /// A stream of lines without a position: it cannot be resumed.
    Untracked,
}

/// The response a stream is read from.
struct Open {
    response: reqwest::Response,
    buffer: Vec<u8>,
    /// Bytes of `buffer` already searched for the end of an item.
    searched: usize,
    /// `Progress::rollbacks` when the request was sent.
    rollbacks: u64,
    /// The id of the last event, which later events without one keep.
    last_id: Option<Position>,
    /// The server closed the response.
    ended: bool,
}

pub struct HttpBulkConsumer {
    connection: Connection,
    method: reqwest::Method,
    path: String,
    body: Option<String>,
    items: String,
    source: CursorSource,
    backoff: PollBackoff,
    checkpoint: Option<Arc<dyn CheckpointStore>>,
    progress: Arc<Mutex<Progress>>,
    /// A draining route ends on the first empty page, so it does not wait for it.
    exit_on_empty: bool,
    stream: Option<HttpBulkStream>,
    open: Option<Open>,
    /// How long a draining route waits for a silent stream.
    silence: Duration,
}

/// The read position, and how often a nack has moved it back.
struct Progress {
    position: Position,
    rollbacks: u64,
    /// Pages handed out and not yet committed.
    outstanding: usize,
}

/// A batch that is out. Dropped without a commit, the read moves back to its start.
struct Unsettled {
    progress: Arc<Mutex<Progress>>,
    /// Where the batch started; `None` where the read cannot move back.
    before: Option<Position>,
    rollbacks: u64,
    settled: bool,
}

impl Unsettled {
    fn settle(&mut self) {
        self.settled = true;
    }
}

impl Drop for Unsettled {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        progress.outstanding = progress.outstanding.saturating_sub(1);
        if progress.rollbacks == self.rollbacks {
            if let Some(before) = self.before.take() {
                progress.position = before;
                progress.rollbacks += 1;
            }
        }
    }
}

/// Names the listing in a checkpoint store: host and path, without the query.
fn source_name(config: &HttpBulkConfig, read: &HttpBulkRead) -> String {
    let host = url::Url::parse(&config.url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let path = read.path.split('?').next().unwrap_or_default();
    format!("{host}{path}")
}

/// The checkpoint of an input, or `None` without `read` and its `cursor_id` and `checkpoint_store`.
pub(crate) async fn cursor_checkpoint(
    config: &HttpBulkConfig,
) -> anyhow::Result<Option<Arc<VersionedCheckpoint>>> {
    let Some(read) = &config.read else {
        return Ok(None);
    };
    let (Some(cursor_id), Some(spec)) = (&read.cursor_id, &read.checkpoint_store) else {
        return Ok(None);
    };
    match checkpoint::parse_checkpoint_store(spec)? {
        CheckpointBackend::Source { .. } => Err(anyhow!(
            "http_bulk needs an external read.checkpoint_store (file://, postgres://, mongodb:// or s3://)"
        )),
        external => {
            let source = source_name(config, read);
            let store = checkpoint::build_external_store(external, &source, cursor_id).await?;
            Ok(Some(Arc::new(VersionedCheckpoint::new(
                store,
                format!("http_bulk:{source}"),
            ))))
        }
    }
}

impl HttpBulkConsumer {
    pub async fn new(config: &HttpBulkConfig, no_resume: bool) -> anyhow::Result<Self> {
        let connection = Connection::new(config)?;
        let Some(read) = &config.read else {
            bail!("http_bulk used as an input needs 'read'");
        };
        if !read.path.starts_with('/') {
            bail!("http_bulk read.path must start with '/'");
        }
        let in_body = read.body.as_deref().is_some_and(|b| b.contains(CURSOR));
        let by_url = read.cursor.link || read.cursor.next_url.is_some();
        if by_url && (read.path.contains(CURSOR) || in_body) {
            bail!("http_bulk read follows next-page URLs, so it takes no '{CURSOR}'");
        }
        if read.stream.is_none() && !by_url && !read.path.contains(CURSOR) && !in_body {
            bail!(
                "http_bulk read needs '{CURSOR}' in its path or body, or every page is the first"
            );
        }
        if read.stream.is_some() && (read.path.contains(LIMIT) || in_limit(&read.body)) {
            bail!("http_bulk read has no '{LIMIT}' in a stream: the server decides what it sends");
        }
        if read.stream.is_some() && !read.items.is_empty() {
            bail!("http_bulk read.items does not apply to a stream: every event or line is a document");
        }
        let default_method = if read.body.is_some() { "POST" } else { "GET" };
        let method = read
            .method
            .as_deref()
            .unwrap_or(default_method)
            .to_ascii_uppercase();
        let cursor = &read.cursor;
        let sources = [
            cursor.response.is_some(),
            cursor.item.is_some(),
            cursor.next_url.is_some(),
            cursor.link,
        ];
        if sources.iter().filter(|set| **set).count() > 1 {
            bail!("http_bulk read.cursor sets more than one of 'response', 'item', 'next_url', 'link'");
        }
        if by_url && read.stream.is_some() {
            bail!("http_bulk read.cursor.next_url and link do not apply to a stream");
        }
        let (source, start) = match (&read.cursor.response, &read.cursor.item, read.stream) {
            _ if by_url => (CursorSource::NextUrl(cursor.next_url.clone()), Value::Null),
            (Some(_), Some(_), _) => unreachable!("refused above"),
            (Some(_), None, Some(_)) => {
                bail!("http_bulk read.cursor.response does not apply to a stream; use 'item'")
            }
            (Some(pointer), None, None) => (
                CursorSource::Response {
                    pointer: pointer.clone(),
                    end_when_missing: read.cursor.end_when_missing,
                },
                Value::Null,
            ),
            (None, Some(pointer), _) => (CursorSource::Item(pointer.clone()), Value::Null),
            (None, None, Some(HttpBulkStream::Sse)) => (CursorSource::EventId, Value::Null),
            (None, None, Some(HttpBulkStream::Ndjson)) => (CursorSource::Untracked, Value::Null),
            (None, None, None) => (CursorSource::Count, Value::from(0u64)),
        };
        if read.cursor.end_when_missing && !matches!(source, CursorSource::Response { .. }) {
            bail!("http_bulk read.cursor.end_when_missing needs read.cursor.response");
        }
        let start = read.cursor.start.clone().unwrap_or(start);
        if matches!(source, CursorSource::Count) && !start.is_u64() {
            bail!("http_bulk read.cursor.start must be a whole number when the cursor is a count");
        }

        let checkpoint: Option<Arc<dyn CheckpointStore>> = if no_resume {
            None
        } else {
            match cursor_checkpoint(config).await? {
                Some(checkpoint) => Some(checkpoint),
                None => {
                    warn!(
                        path = %read.path,
                        "http_bulk input has no read.cursor_id and read.checkpoint_store; every start reads from the beginning"
                    );
                    None
                }
            }
        };
        let saved = match &checkpoint {
            Some(checkpoint) => checkpoint.load().await?.and_then(|text| {
                let position = serde_json::from_str::<Position>(&text).ok();
                if position.is_none() {
                    warn!(value = %text, "Ignoring an unreadable http_bulk cursor; reading from the beginning");
                }
                position
            }),
            None => None,
        };
        info!(path = %read.path, resumed = saved.is_some(), "http_bulk input ready");

        Ok(Self {
            connection,
            method: reqwest::Method::from_bytes(method.as_bytes())
                .with_context(|| format!("http_bulk read.method '{method}' is not valid"))?,
            path: read.path.clone(),
            body: read.body.clone(),
            items: read.items.clone(),
            source,
            backoff: PollBackoff::new(
                Duration::from_millis(read.polling_interval_ms.unwrap_or(1000)),
                read.max_polling_interval_ms.map(Duration::from_millis),
            ),
            checkpoint,
            progress: Arc::new(Mutex::new(Progress {
                position: saved.unwrap_or(Position::At(start)),
                rollbacks: 0,
                outstanding: 0,
            })),
            exit_on_empty: false,
            stream: read.stream,
            open: None,
            silence: Duration::from_millis(read.polling_interval_ms.unwrap_or(1000)),
        })
    }

    /// The read request at `cursor`.
    fn request(&self, cursor: &Value, limit: usize) -> reqwest::RequestBuilder {
        let limit = limit.to_string();
        let url = match (&self.source, cursor) {
            (CursorSource::NextUrl(_), Value::String(next)) => next.clone(),
            _ => format!(
                "{}{}",
                self.connection.base,
                self.path
                    .replace(
                        CURSOR,
                        &utf8_percent_encode(&cursor_text(cursor), NON_ALPHANUMERIC).to_string()
                    )
                    .replace(LIMIT, &limit)
            ),
        };
        let mut request = self.connection.request(self.method.clone(), &url);
        if let Some(body) = &self.body {
            request = request.header(CONTENT_TYPE, JSON).body(
                body.replace(CURSOR, &cursor.to_string())
                    .replace(LIMIT, &limit),
            );
        }
        request
    }

    /// Requests the page at `cursor` and returns the parsed response, with the URL of the
    /// next page when the cursor follows URLs.
    async fn page(
        &self,
        cursor: &Value,
        limit: usize,
    ) -> Result<(Value, Option<Position>), ConsumerError> {
        let label = format!("{} {}", self.method, self.path);
        let base = url::Url::parse(&self.connection.base)
            .map_err(|e| ConsumerError::Permanent(anyhow!("http_bulk URL: {e}")))?;
        if let (CursorSource::NextUrl(_), Value::String(next)) = (&self.source, cursor) {
            // A saved URL may predate a change of `url`.
            same_origin(&base, &base, next)?;
        }
        let request = self.request(cursor, limit);
        let response = self
            .connection
            .send(request)
            .await
            .map_err(|e| send_error(&label, e))?;
        let status = response.status();
        let link = response
            .headers()
            .get_all(LINK)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find_map(next_link);
        let page_url = response.url().clone();
        let text = response
            .text()
            .await
            .map_err(|e| ConsumerError::Connection(anyhow!("{label} response was cut off: {e}")))?;
        if !status.is_success() {
            return Err(status_error(&label, status, &text));
        }
        let body: Value = serde_json::from_str(&text).map_err(|e| {
            ConsumerError::Permanent(anyhow!(
                "{label} did not answer with JSON ({e}): {}",
                quoted(&text)
            ))
        })?;
        let CursorSource::NextUrl(pointer) = &self.source else {
            return Ok((body, None));
        };
        let next = match pointer {
            Some(pointer) => match body.pointer(pointer) {
                Some(Value::String(next)) => Some(next.clone()),
                None | Some(Value::Null) => None,
                Some(other) => {
                    return Err(ConsumerError::Permanent(anyhow!(
                        "http_bulk read.cursor.next_url '{pointer}' is not a URL: {other}"
                    )))
                }
            },
            None => link,
        };
        let next = match next.filter(|next| !next.is_empty()) {
            Some(next) => Position::At(Value::from(same_origin(&base, &page_url, &next)?)),
            None => Position::End,
        };
        Ok((body, Some(next)))
    }

    /// The position after each document of a page; `None` where a page cannot be split.
    fn positions(
        &self,
        response: &Value,
        items: &[Value],
        cursor: &Value,
        next_url: Option<Position>,
    ) -> Result<Vec<Option<Position>>, ConsumerError> {
        match &self.source {
            CursorSource::NextUrl(_) => Ok(after_page(items, next_url.unwrap_or(Position::End))),
            CursorSource::Count => {
                let read = cursor.as_u64().unwrap_or_default();
                Ok((1..=items.len() as u64)
                    .map(|n| Some(Position::At(Value::from(read + n))))
                    .collect())
            }
            CursorSource::Item(pointer) => items
                .iter()
                .map(|item| match item.pointer(pointer) {
                    Some(value) if !value.is_null() => Ok(Some(Position::At(value.clone()))),
                    _ => Err(ConsumerError::Permanent(anyhow!(
                        "http_bulk read.cursor.item '{pointer}' is missing in a document"
                    ))),
                })
                .collect(),
            CursorSource::Response {
                pointer,
                end_when_missing,
            } => {
                let next = match response.pointer(pointer) {
                    Some(Value::Null) => Position::End,
                    // Sent back, an empty position would read the first page again.
                    Some(Value::String(text)) if text.is_empty() => Position::End,
                    Some(value) => Position::At(value.clone()),
                    None if *end_when_missing => Position::End,
                    None => {
                        return Err(ConsumerError::Permanent(anyhow!(
                            "http_bulk read.cursor.response '{pointer}' is missing in the response; set read.cursor.end_when_missing if the API leaves it out on the last page"
                        )))
                    }
                };
                Ok(after_page(items, next))
            }
            // Only a stream has these, and it does not read pages.
            CursorSource::EventId | CursorSource::Untracked => Ok(vec![None; items.len()]),
        }
    }

    /// Opens the stream at `cursor`.
    async fn connect(
        &self,
        cursor: &Value,
        format: HttpBulkStream,
    ) -> Result<reqwest::Response, ConsumerError> {
        let label = format!("{} {}", self.method, self.path);
        let mut request = self.request(cursor, 0);
        if !self.connection.headers.contains_key(ACCEPT) {
            let accept = match format {
                HttpBulkStream::Sse => "text/event-stream",
                HttpBulkStream::Ndjson => "application/x-ndjson",
            };
            request = request.header(ACCEPT, accept);
        }
        if matches!(self.source, CursorSource::EventId) && !cursor.is_null() {
            request = request.header("Last-Event-ID", cursor_text(cursor));
        }
        let response = self
            .connection
            .send(request)
            .await
            .map_err(|e| send_error(&label, e))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let text = response.text().await.unwrap_or_default();
        Err(status_error(&label, status, &text))
    }

    /// Reads the events or lines that have arrived on the open response.
    async fn receive_stream(
        &mut self,
        format: HttpBulkStream,
        max_messages: usize,
    ) -> Result<ReceivedBatch, ConsumerError> {
        loop {
            let (before, rollbacks) = {
                let progress = self.progress.lock().unwrap();
                (progress.position.clone(), progress.rollbacks)
            };
            let Position::At(cursor) = &before else {
                return Ok(self.idle().await);
            };
            // A nack moved the position back: the response in hand is past it.
            if self.open.as_ref().is_some_and(|o| o.rollbacks != rollbacks) {
                self.open = None;
            }
            if self.open.is_none() {
                let response = self.connect(cursor, format).await?;
                let resumed = matches!(self.source, CursorSource::EventId) && !cursor.is_null();
                self.open = Some(Open {
                    response,
                    buffer: Vec::new(),
                    searched: 0,
                    rollbacks,
                    last_id: resumed.then(|| before.clone()),
                    ended: false,
                });
            }
            let Some(open) = self.open.as_mut() else {
                continue;
            };
            let items = take_items(format, &mut open.buffer, &mut open.searched, max_messages);
            if items.is_empty() {
                if open.ended {
                    // A draining route is done; connecting again would read it twice.
                    if self.exit_on_empty {
                        return Ok(ReceivedBatch::empty());
                    }
                    self.open = None;
                    return Ok(self.idle().await);
                }
                if open.buffer.len() > MAX_ITEM_BYTES {
                    self.open = None;
                    return Err(ConsumerError::Permanent(anyhow!(
                        "http_bulk stream sent {MAX_ITEM_BYTES} bytes without the end of an event or line"
                    )));
                }
                let chunk = if self.exit_on_empty {
                    match tokio::time::timeout(self.silence, open.response.chunk()).await {
                        Ok(chunk) => chunk,
                        Err(_) => return Ok(ReceivedBatch::empty()),
                    }
                } else {
                    open.response.chunk().await
                };
                match chunk {
                    Ok(Some(bytes)) => open.buffer.extend_from_slice(&bytes),
                    Ok(None) => {
                        // The last line may come without a line break.
                        if format == HttpBulkStream::Ndjson {
                            open.buffer.push(b'\n');
                        }
                        open.ended = true;
                    }
                    // A draining route must not pass a cut stream off as a complete one.
                    Err(error) if self.exit_on_empty => {
                        self.open = None;
                        return Err(ConsumerError::Connection(anyhow!(
                            "{} {} was cut off: {error}",
                            self.method,
                            self.path
                        )));
                    }
                    Err(error) => {
                        warn!(%error, path = %self.path, "http_bulk stream was cut off; connecting again");
                        self.open = None;
                        tokio::time::sleep(self.backoff.idle_delay()).await;
                    }
                }
                continue;
            }

            let mut positions = Vec::with_capacity(items.len());
            let mut messages = Vec::with_capacity(items.len());
            for item in items {
                positions.push(match &self.source {
                    CursorSource::Item(pointer) => Some(item_position(pointer, &item.payload)?),
                    CursorSource::EventId => {
                        if let Some(id) = &item.event_id {
                            open.last_id = Some(Position::At(Value::from(id.as_str())));
                        }
                        open.last_id.clone()
                    }
                    _ => None,
                });
                let mut message = CanonicalMessage::new_bytes(item.payload, None);
                if let Some(id) = item.event_id {
                    message = message.with_metadata_kv("sse_id", id);
                }
                if let Some(name) = item.event_name {
                    message = message.with_metadata_kv("sse_event", name);
                }
                messages.push(message);
            }
            {
                let mut progress = self.progress.lock().unwrap();
                if progress.rollbacks != rollbacks {
                    continue;
                }
                if let Some(last) = positions.iter().flatten().last() {
                    progress.position = last.clone();
                }
                progress.outstanding += 1;
            }
            self.backoff.reset();
            trace!(count = messages.len(), path = %self.path, "Read stream items");
            let commit = self.commit(positions, before, rollbacks);
            return Ok(ReceivedBatch { messages, commit });
        }
    }

    /// The commit of a batch: a nack moves the read back behind the last acked document.
    fn commit(
        &self,
        positions: Vec<Option<Position>>,
        before: Position,
        rollbacks: u64,
    ) -> BatchCommitFunc {
        let checkpoint = self.checkpoint.clone();
        let progress = self.progress.clone();
        let resumable = !matches!(self.source, CursorSource::Untracked);
        let mut unsettled = Unsettled {
            progress: self.progress.clone(),
            before: resumable.then(|| before.clone()),
            rollbacks,
            settled: false,
        };
        Box::new(move |dispositions: Vec<MessageDisposition>| {
            Box::pin(async move {
                unsettled.settle();
                let acked = dispositions
                    .iter()
                    .take(positions.len())
                    .take_while(|d| {
                        matches!(d, MessageDisposition::Ack | MessageDisposition::Reply(_))
                    })
                    .count();
                let boundary = positions[..acked].last().cloned().flatten();
                {
                    let mut progress = progress.lock().unwrap();
                    progress.outstanding = progress.outstanding.saturating_sub(1);
                    // An earlier nack rewound past this page: it is read again.
                    if progress.rollbacks != rollbacks {
                        return Ok(());
                    }
                    if acked < positions.len() && resumable {
                        progress.position = boundary.clone().unwrap_or(before);
                        progress.rollbacks += 1;
                    } else if acked < positions.len() {
                        warn!("A line of an http_bulk stream without read.cursor.item was not acknowledged and is not read again");
                    }
                }
                if let Some(boundary) = boundary {
                    save(&checkpoint, &boundary).await;
                }
                Ok(())
            }) as BoxFuture<'static, anyhow::Result<()>>
        })
    }

    async fn idle(&mut self) -> ReceivedBatch {
        if !self.exit_on_empty {
            tokio::time::sleep(self.backoff.idle_delay()).await;
        }
        ReceivedBatch::empty()
    }
}

/// Positions for a page whose end is `next`; an empty page may still move on, or end the read.
fn after_page(items: &[Value], next: Position) -> Vec<Option<Position>> {
    let mut positions = vec![None; items.len()];
    match positions.last_mut() {
        Some(last) => *last = Some(next),
        None => positions.push(Some(next)),
    }
    positions
}

/// The `rel="next"` target of one `Link` header value (RFC 8288).
fn next_link(value: &str) -> Option<String> {
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after.find('>')?;
        let params = &after[end + 1..];
        let until = params.find('<').unwrap_or(params.len());
        let next = params[..until].split(';').any(|param| {
            param.split_once('=').is_some_and(|(name, value)| {
                name.trim().eq_ignore_ascii_case("rel")
                    && (value.trim().trim_matches('"').split_ascii_whitespace())
                        .any(|rel| rel.eq_ignore_ascii_case("next"))
            })
        });
        if next {
            return Some(after[..end].to_string());
        }
        rest = &params[until..];
    }
    None
}

/// `next` resolved against the page it came from, if on the origin of `base`: another host
/// would receive the credentials.
fn same_origin(base: &url::Url, page: &url::Url, next: &str) -> Result<String, ConsumerError> {
    let resolved = page.join(next).map_err(|e| {
        ConsumerError::Permanent(anyhow!(
            "http_bulk next-page URL '{next}' is not valid: {e}"
        ))
    })?;
    if resolved.origin() != base.origin() {
        return Err(ConsumerError::Permanent(anyhow!(
            "http_bulk next-page URL '{next}' leaves {}; only the same scheme, host and port are followed",
            base.origin().ascii_serialization()
        )));
    }
    Ok(resolved.into())
}

/// The position as it stands in a URL or a header.
fn cursor_text(cursor: &Value) -> String {
    match cursor {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn in_limit(body: &Option<String>) -> bool {
    body.as_deref().is_some_and(|body| body.contains(LIMIT))
}

fn send_error(label: &str, e: super::auth::SendError) -> ConsumerError {
    let error = anyhow!("{label} failed: {e}");
    if e.retryable {
        ConsumerError::Connection(error)
    } else {
        ConsumerError::Permanent(error)
    }
}

fn status_error(label: &str, status: reqwest::StatusCode, text: &str) -> ConsumerError {
    let error = anyhow!("{label} answered {status}: {}", quoted(text));
    if http_status::is_retryable(status.as_u16()) {
        ConsumerError::Connection(error)
    } else {
        ConsumerError::Permanent(error)
    }
}

/// The position of a streamed document, from the field `pointer` names.
fn item_position(pointer: &str, payload: &[u8]) -> Result<Position, ConsumerError> {
    let document = serde_json::from_slice::<Value>(payload).ok();
    match document
        .as_ref()
        .and_then(|document| document.pointer(pointer))
    {
        Some(value) if !value.is_null() => Ok(Position::At(value.clone())),
        _ => Err(ConsumerError::Permanent(anyhow!(
            "http_bulk read.cursor.item '{pointer}' is missing in a document"
        ))),
    }
}

/// Takes up to `max` complete events or lines off the front of `buffer`.
fn take_items(
    format: HttpBulkStream,
    buffer: &mut Vec<u8>,
    searched: &mut usize,
    max: usize,
) -> Vec<ParsedSseEvent> {
    let mut items = Vec::new();
    let mut taken = 0;
    // The end of an event may straddle the part searched before.
    let mut from = searched.saturating_sub(3);
    *searched = 0;
    while items.len() < max {
        let rest = &buffer[taken..];
        let found = match format {
            HttpBulkStream::Ndjson => {
                let end = rest[from..].iter().position(|byte| *byte == b'\n');
                end.map(|end| (from + end, 1))
            }
            HttpBulkStream::Sse => find_sse_event_end(&rest[from..]).map(|end| {
                let end = from + end;
                let crlf = rest[end..].starts_with(b"\r\n\r\n");
                (end, if crlf { 4 } else { 2 })
            }),
        };
        let Some((end, terminator)) = found else {
            *searched = rest.len();
            break;
        };
        match format {
            HttpBulkStream::Ndjson => {
                let line = rest[..end].trim_ascii();
                if !line.is_empty() {
                    items.push(ParsedSseEvent {
                        payload: Bytes::copy_from_slice(line),
                        event_id: None,
                        event_name: None,
                    });
                }
            }
            // The event is complete, so decoding cannot split a character.
            HttpBulkStream::Sse => {
                items.extend(parse_sse_event(&String::from_utf8_lossy(&rest[..end])))
            }
        }
        taken += end + terminator;
        from = 0;
    }
    buffer.drain(..taken);
    items
}

async fn save(checkpoint: &Option<Arc<dyn CheckpointStore>>, position: &Position) {
    let (Some(checkpoint), Ok(text)) = (checkpoint, serde_json::to_string(position)) else {
        return;
    };
    if let Err(error) = checkpoint.save(&text).await {
        warn!(%error, "Failed to save the http_bulk cursor; documents may be read again after a restart");
    }
}

#[async_trait]
impl MessageConsumer for HttpBulkConsumer {
    async fn receive_batch(&mut self, max_messages: usize) -> Result<ReceivedBatch, ConsumerError> {
        if max_messages == 0 {
            return Ok(ReceivedBatch::empty());
        }
        if let Some(format) = self.stream {
            return self.receive_stream(format, max_messages).await;
        }
        loop {
            let (before, rollbacks) = {
                let progress = self.progress.lock().unwrap();
                (progress.position.clone(), progress.rollbacks)
            };
            let Position::At(cursor) = &before else {
                return Ok(self.idle().await);
            };
            let (response, next_url) = self.page(cursor, max_messages).await?;
            let items = match response.pointer(&self.items) {
                Some(Value::Array(items)) => items,
                _ => {
                    return Err(ConsumerError::Permanent(anyhow!(
                        "http_bulk read.items '{}' is not an array in the response",
                        self.items
                    )))
                }
            };
            let mut positions = self.positions(&response, items, cursor, next_url)?;
            if items.is_empty() {
                if let Some(next) = positions.pop().flatten().filter(|next| *next != before) {
                    let settled = {
                        let mut progress = self.progress.lock().unwrap();
                        let current = progress.rollbacks == rollbacks;
                        if current {
                            progress.position = next.clone();
                        }
                        current && progress.outstanding == 0
                    };
                    // Saved past a page that is still out, a restart would skip it.
                    if settled {
                        save(&self.checkpoint, &next).await;
                    }
                }
                return Ok(self.idle().await);
            }
            // Advance optimistically; the commit rolls back to the last acked document.
            {
                let mut progress = self.progress.lock().unwrap();
                if progress.rollbacks != rollbacks {
                    // A nack moved the cursor back while this page was read.
                    continue;
                }
                if let Some(Some(last)) = positions.last() {
                    progress.position = last.clone();
                }
                progress.outstanding += 1;
            }
            self.backoff.reset();

            let messages: Vec<CanonicalMessage> = items
                .iter()
                .map(|item| {
                    CanonicalMessage::new(serde_json::to_vec(item).unwrap_or_default(), None)
                })
                .collect();
            trace!(count = messages.len(), path = %self.path, "Read documents");

            let commit = self.commit(positions, before, rollbacks);
            return Ok(ReceivedBatch { messages, commit });
        }
    }

    fn set_exit_on_empty(&mut self, exit_on_empty: bool) {
        self.exit_on_empty = exit_on_empty;
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::plugin::test_support::{payload_texts, StubHttpServer, StubRequest};
    use serde_json::json;

    async fn server(
        respond: impl Fn(&StubRequest) -> (u16, String) + Send + Sync + 'static,
    ) -> StubHttpServer {
        StubHttpServer::start(respond).await.expect("stub server")
    }

    async fn consumer(server: &StubHttpServer, mut config: Value) -> HttpBulkConsumer {
        config["url"] = json!(server.url());
        config["read"]["polling_interval_ms"] = json!(1);
        let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
        HttpBulkConsumer::new(&config, false)
            .await
            .expect("consumer")
    }

    /// Reads one page and answers every document with `disposition`.
    async fn read(consumer: &mut HttpBulkConsumer, acked: usize) -> Vec<String> {
        let batch = consumer.receive_batch(2).await.expect("page");
        let texts = payload_texts(&batch.messages);
        let dispositions = (0..texts.len())
            .map(|i| {
                if i < acked {
                    MessageDisposition::Ack
                } else {
                    MessageDisposition::Nack
                }
            })
            .collect();
        (batch.commit)(dispositions).await.expect("commit");
        texts
    }

    fn targets(server: &StubHttpServer) -> Vec<String> {
        server.requests().into_iter().map(|r| r.target).collect()
    }

    fn store() -> (std::path::PathBuf, String) {
        let path = std::env::temp_dir().join(format!(
            "mqb-http-bulk-read-{}.json",
            fast_uuid_v7::gen_id_string()
        ));
        let url = format!("file://{}", path.display());
        (path, url)
    }

    #[tokio::test]
    async fn a_page_dropped_without_a_commit_is_read_again() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/docs?offset=0&limit=2" => r#"{"results":[{"id":1},{"id":2}]}"#,
                "/docs?offset=2&limit=2" => r#"{"results":[{"id":3}]}"#,
                _ => r#"{"results":[]}"#,
            };
            (200, page.to_string())
        })
        .await;
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/docs?offset={cursor}&limit={limit}", "items": "/results"}}),
        )
        .await;

        drop(consumer.receive_batch(2).await.expect("page"));
        assert_eq!(read(&mut consumer, 2).await, [r#"{"id":1}"#, r#"{"id":2}"#]);
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":3}"#]);
    }

    #[tokio::test]
    async fn without_a_cursor_source_the_documents_read_are_the_offset() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/docs?offset=0&limit=2" => r#"{"results":[{"id":1},{"id":2}]}"#,
                "/docs?offset=2&limit=2" => r#"{"results":[{"id":3}]}"#,
                _ => r#"{"results":[]}"#,
            };
            (200, page.to_string())
        })
        .await;
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/docs?offset={cursor}&limit={limit}", "items": "/results"}}),
        )
        .await;

        assert_eq!(read(&mut consumer, 2).await, [r#"{"id":1}"#, r#"{"id":2}"#]);
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":3}"#]);
        assert!(read(&mut consumer, 0).await.is_empty());
        assert_eq!(targets(&server).last().unwrap(), "/docs?offset=3&limit=2");
    }

    #[tokio::test]
    async fn a_nacked_document_is_read_again_from_the_last_acked_one() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/rows?id=gt.0" => r#"[{"id":"a b"},{"id":"c"}]"#,
                "/rows?id=gt.a%20b" => r#"[{"id":"c"}]"#,
                _ => "[]",
            };
            (200, page.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({
            "read": {
                "path": "/rows?id=gt.{cursor}",
                "cursor": {"item": "/id", "start": 0},
                "cursor_id": "copy",
                "checkpoint_store": url,
            },
        });
        let mut first = consumer(&server, config.clone()).await;
        assert_eq!(read(&mut first, 1).await.len(), 2);
        assert_eq!(read(&mut first, 1).await, [r#"{"id":"c"}"#]);

        // A restart resumes after the last acked document.
        let mut second = consumer(&server, config).await;
        assert!(read(&mut second, 0).await.is_empty());
        assert_eq!(targets(&server).last().unwrap(), "/rows?id=gt.c");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_page_read_past_a_nacked_one_does_not_move_the_checkpoint() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/docs?offset=0&limit=2" => r#"{"results":[{"id":1},{"id":2}]}"#,
                "/docs?offset=2&limit=2" => r#"{"results":[{"id":3},{"id":4}]}"#,
                _ => r#"{"results":[]}"#,
            };
            (200, page.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({"read": {
            "path": "/docs?offset={cursor}&limit={limit}", "items": "/results",
            "cursor_id": "copy", "checkpoint_store": url,
        }});
        let mut first = consumer(&server, config.clone()).await;
        let earlier = first.receive_batch(2).await.expect("page");
        let later = first.receive_batch(2).await.expect("page");
        (earlier.commit)(vec![MessageDisposition::Nack; 2])
            .await
            .expect("commit");
        (later.commit)(vec![MessageDisposition::Ack; 2])
            .await
            .expect("commit");

        assert_eq!(read(&mut first, 0).await, [r#"{"id":1}"#, r#"{"id":2}"#]);
        // The acked later page saved nothing: a restart starts over as well.
        let mut second = consumer(&server, config).await;
        assert_eq!(read(&mut second, 0).await.len(), 2);
        assert_eq!(targets(&server).last().unwrap(), "/docs?offset=0&limit=2");
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_response_cursor_goes_into_the_body_and_null_ends_the_read_for_good() {
        let server = server(|request| {
            let body = String::from_utf8_lossy(&request.body).into_owned();
            let page = match body.as_str() {
                r#"{"limit":2,"offset":null}"# => r#"{"points":[{"id":1},{"id":2}],"next":"p 2"}"#,
                r#"{"limit":2,"offset":"p 2"}"# => r#"{"points":[{"id":3}],"next":null}"#,
                other => panic!("unexpected body {other}"),
            };
            (200, page.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({
            "read": {
                "path": "/scroll",
                "body": r#"{"limit":{limit},"offset":{cursor}}"#,
                "items": "/points",
                "cursor": {"response": "/next"},
                "cursor_id": "scan",
                "checkpoint_store": url,
            },
        });
        let mut first = consumer(&server, config.clone()).await;
        // A page whose cursor covers all of it is read again as a whole after a nack.
        assert_eq!(read(&mut first, 1).await.len(), 2);
        assert_eq!(read(&mut first, 2).await.len(), 2);
        assert_eq!(read(&mut first, 1).await, [r#"{"id":3}"#]);
        assert!(read(&mut first, 0).await.is_empty());

        let mut second = consumer(&server, config).await;
        assert!(read(&mut second, 0).await.is_empty());
        let requests = server.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].header("content-type"), Some(JSON));
        let _ = std::fs::remove_file(path);
    }

    /// Reads a two-page listing whose last page is `last` and returns the requests made.
    async fn requests_until_the_end(last: &'static str, cursor: Value) -> Vec<String> {
        let server = server(move |request| {
            let page = match request.target.as_str() {
                "/list?page=" => r#"{"items":[{"id":1}],"next":"p2"}"#,
                "/list?page=p2" => last,
                other => panic!("unexpected request {other}"),
            };
            (200, page.to_string())
        })
        .await;
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/list?page={cursor}", "items": "/items", "cursor": cursor}}),
        )
        .await;
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":1}"#]);
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":2}"#]);
        assert!(read(&mut consumer, 0).await.is_empty());
        targets(&server)
    }

    #[tokio::test]
    async fn an_empty_string_cursor_ends_the_read() {
        let requests = requests_until_the_end(
            r#"{"items":[{"id":2}],"next":""}"#,
            json!({"response": "/next", "start": ""}),
        )
        .await;
        assert_eq!(requests, ["/list?page=", "/list?page=p2"]);
    }

    #[tokio::test]
    async fn a_missing_cursor_ends_the_read_when_asked_to() {
        let requests = requests_until_the_end(
            r#"{"items":[{"id":2}]}"#,
            json!({"response": "/next", "start": "", "end_when_missing": true}),
        )
        .await;
        assert_eq!(requests, ["/list?page=", "/list?page=p2"]);
    }

    #[tokio::test]
    async fn a_missing_cursor_stops_the_route_by_default() {
        let server = server(|_| (200, r#"{"items":[{"id":1}]}"#.to_string())).await;
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/list?page={cursor}", "items": "/items", "cursor": {"response": "/next"}}}),
        )
        .await;
        let error = consumer.receive_batch(2).await.expect_err("an error");
        assert!(matches!(error, ConsumerError::Permanent(_)), "{error}");
        assert!(error.to_string().contains("end_when_missing"), "{error}");
    }

    #[tokio::test]
    async fn end_when_missing_needs_a_response_cursor() {
        let config: HttpBulkConfig = serde_json::from_value(json!({
            "url": "http://127.0.0.1:9",
            "read": {"path": "/list?page={cursor}", "cursor": {"item": "/id", "end_when_missing": true}},
        }))
        .expect("config");
        let error = HttpBulkConsumer::new(&config, true)
            .await
            .err()
            .expect("an error");
        assert!(error.to_string().contains("end_when_missing"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_page_can_still_move_the_cursor() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/changes?since=0" => r#"{"results":[],"last_seq":"7-x"}"#,
                _ => r#"{"results":[],"last_seq":"7-x"}"#,
            };
            (200, page.to_string())
        })
        .await;
        let mut consumer = consumer(
            &server,
            json!({"read": {
                "path": "/changes?since={cursor}",
                "items": "/results",
                "cursor": {"response": "/last_seq", "start": 0},
            }}),
        )
        .await;
        assert!(read(&mut consumer, 0).await.is_empty());
        assert!(read(&mut consumer, 0).await.is_empty());
        assert_eq!(
            targets(&server),
            ["/changes?since=0", "/changes?since=7%2Dx"]
        );
    }

    #[tokio::test]
    async fn an_empty_page_does_not_save_past_a_page_that_is_still_out() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/changes?since=0" => r#"{"results":[{"id":1}],"last_seq":5}"#,
                _ => r#"{"results":[],"last_seq":9}"#,
            };
            (200, page.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({"read": {
            "path": "/changes?since={cursor}", "items": "/results",
            "cursor": {"response": "/last_seq", "start": 0},
            "cursor_id": "feed", "checkpoint_store": url,
        }});
        let mut first = consumer(&server, config.clone()).await;
        let out = first.receive_batch(2).await.expect("page");
        assert!(read(&mut first, 0).await.is_empty());

        let mut second = consumer(&server, config.clone()).await;
        assert_eq!(
            second.receive_batch(2).await.expect("page").messages.len(),
            1
        );
        assert_eq!(targets(&server).last().unwrap(), "/changes?since=0");

        // Once the page is acked, an empty page moves the checkpoint again.
        (out.commit)(vec![MessageDisposition::Ack])
            .await
            .expect("commit");
        for since in ["/changes?since=5", "/changes?since=9"] {
            let mut next = consumer(&server, config.clone()).await;
            assert!(read(&mut next, 0).await.is_empty());
            assert_eq!(targets(&server).last().unwrap(), since);
        }
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_busy_source_is_retried_and_a_refusal_or_wrong_shape_is_not() {
        let server = server(|request| match request.target.as_str() {
            "/busy?o=0" => (503, "later".to_string()),
            "/refused?o=0" => (404, "no such index".to_string()),
            "/html?o=0" => (200, "<html>".to_string()),
            _ => (200, r#"{"rows":{}}"#.to_string()),
        })
        .await;
        for (path, permanent, text) in [
            ("/busy?o={cursor}", false, "answered 503"),
            ("/refused?o={cursor}", true, "no such index"),
            ("/html?o={cursor}", true, "did not answer with JSON"),
            ("/shape?o={cursor}", true, "is not an array"),
        ] {
            let mut consumer =
                consumer(&server, json!({"read": {"path": path, "items": "/rows"}})).await;
            let Err(error) = consumer.receive_batch(2).await else {
                panic!("{path} was read");
            };
            assert_eq!(matches!(error, ConsumerError::Permanent(_)), permanent);
            assert!(error.to_string().contains(text), "{error}");
        }
    }

    #[tokio::test]
    async fn a_config_that_cannot_read_is_refused_at_startup() {
        for (config, text) in [
            (json!({"upsert": {"path": "/d"}}), "needs 'read'"),
            (
                json!({"read": {"path": "docs?o={cursor}"}}),
                "start with '/'",
            ),
            (json!({"read": {"path": "/docs"}}), "'{cursor}'"),
            (
                json!({"read": {"path": "/d?o={cursor}", "cursor": {"response": "/a", "item": "/b"}}}),
                "more than one",
            ),
            (
                json!({"read": {"path": "/d", "cursor": {"link": true, "response": "/a"}}}),
                "more than one",
            ),
            (
                json!({"read": {"path": "/d?o={cursor}", "cursor": {"link": true}}}),
                "takes no '{cursor}'",
            ),
            (
                json!({"read": {"path": "/d", "stream": "ndjson", "cursor": {"next_url": "/next"}}}),
                "stream",
            ),
            (
                json!({"read": {"path": "/d?o={cursor}", "cursor": {"start": "a"}}}),
                "whole number",
            ),
            (
                json!({"read": {"path": "/d?o={cursor}", "cursor_id": "c", "checkpoint_store": "cursors"}}),
                "external read.checkpoint_store",
            ),
        ] {
            let mut config = config;
            config["url"] = json!("http://localhost:1");
            let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
            let error = HttpBulkConsumer::new(&config, false)
                .await
                .err()
                .expect("refused");
            assert!(error.to_string().contains(text), "{error}");
        }
    }

    #[tokio::test]
    async fn link_headers_chain_the_pages_until_one_has_no_next() {
        let server = StubHttpServer::start_with_headers(|request| {
            let link = |next: &str| vec![("Link".to_string(), next.to_string())];
            match request.target.as_str() {
                "/list?limit=2" => (
                    200,
                    link(r#"</list?limit=2>; rel="first", </list?page=2>; rel="next""#),
                    r#"[{"id":1},{"id":2}]"#.to_string(),
                ),
                "/list?page=2" => (
                    200,
                    link("</list?page=3>; rel=next"),
                    r#"[{"id":3}]"#.to_string(),
                ),
                "/list?page=3" => (200, Vec::new(), r#"[{"id":4}]"#.to_string()),
                other => panic!("unexpected request {other}"),
            }
        })
        .await
        .expect("stub server");
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/list?limit={limit}", "cursor": {"link": true}}}),
        )
        .await;

        assert_eq!(read(&mut consumer, 2).await.len(), 2);
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":3}"#]);
        assert_eq!(read(&mut consumer, 1).await, [r#"{"id":4}"#]);
        assert!(read(&mut consumer, 0).await.is_empty());
        assert_eq!(targets(&server).len(), 3);
    }

    #[tokio::test]
    async fn a_next_url_in_the_body_resumes_from_the_checkpointed_page() {
        let server = server(|request| {
            let page = match request.target.as_str() {
                "/list" => r#"{"data":[{"id":1}],"links":{"next":"/list?page=2"}}"#,
                "/list?page=2" => r#"{"data":[{"id":2}],"links":{"next":"list?page=3"}}"#,
                "/list?page=3" => r#"{"data":[{"id":3}],"links":{"next":null}}"#,
                other => panic!("unexpected request {other}"),
            };
            (200, page.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({"read": {
            "path": "/list", "items": "/data", "cursor": {"next_url": "/links/next"},
            "cursor_id": "pages", "checkpoint_store": url,
        }});
        let mut first = consumer(&server, config.clone()).await;
        assert_eq!(read(&mut first, 1).await, [r#"{"id":1}"#]);

        let mut second = consumer(&server, config).await;
        assert_eq!(read(&mut second, 1).await, [r#"{"id":2}"#]);
        assert_eq!(read(&mut second, 1).await, [r#"{"id":3}"#]);
        assert!(read(&mut second, 0).await.is_empty());
        assert_eq!(targets(&server), ["/list", "/list?page=2", "/list?page=3"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn a_next_url_on_another_host_is_refused() {
        let server = server(|_| {
            let page = r#"{"data":[{"id":1}],"next":"http://other.example/list?page=2"}"#;
            (200, page.to_string())
        })
        .await;
        let mut consumer = consumer(
            &server,
            json!({"read": {"path": "/list", "items": "/data", "cursor": {"next_url": "/next"}}}),
        )
        .await;
        let Err(error) = consumer.receive_batch(2).await else {
            panic!("another host is refused");
        };
        assert!(matches!(error, ConsumerError::Permanent(_)), "{error:#}");
    }

    #[tokio::test]
    async fn a_saved_next_url_is_not_followed_once_the_url_points_elsewhere() {
        let respond = |_: &StubRequest| (200, r#"{"data":[{"id":1}],"next":"/p2"}"#.to_string());
        let (old, new) = (server(respond).await, server(respond).await);
        let (path, url) = store();
        let config = json!({"read": {
            "path": "/list", "items": "/data", "cursor": {"next_url": "/next"},
            "cursor_id": "pages", "checkpoint_store": url,
        }});
        let mut first = consumer(&old, config.clone()).await;
        assert_eq!(read(&mut first, 1).await.len(), 1);

        let mut second = consumer(&new, config).await;
        let Err(error) = second.receive_batch(2).await else {
            panic!("the saved URL is on the old host");
        };
        assert!(error.to_string().contains("leaves"), "{error:#}");
        assert!(new.requests().is_empty());
        let _ = std::fs::remove_file(path);
    }

    fn item_texts(
        format: HttpBulkStream,
        buffer: &mut Vec<u8>,
        searched: &mut usize,
    ) -> Vec<String> {
        take_items(format, buffer, searched, 10)
            .into_iter()
            .map(|item| String::from_utf8(item.payload.to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn an_event_or_line_is_taken_once_it_is_complete() {
        let (mut buffer, mut searched) = (Vec::new(), 0);
        for (chunk, lines) in [
            ("{\"a\":1}\r\n\n{\"b\"", vec![r#"{"a":1}"#]),
            (":2", vec![]),
            ("}\n{\"c\":3}\n", vec![r#"{"b":2}"#, r#"{"c":3}"#]),
        ] {
            buffer.extend_from_slice(chunk.as_bytes());
            let taken = item_texts(HttpBulkStream::Ndjson, &mut buffer, &mut searched);
            assert_eq!(taken, lines, "{chunk:?}");
        }
        assert!(buffer.is_empty());

        // The blank line that ends an event arrives in two chunks.
        for (chunk, events) in [
            (": ping\n\ndata: one\r\n\r", vec![]),
            ("\ndata: two\n", vec!["one"]),
            ("\n", vec!["two"]),
        ] {
            buffer.extend_from_slice(chunk.as_bytes());
            let taken = item_texts(HttpBulkStream::Sse, &mut buffer, &mut searched);
            assert_eq!(taken, events, "{chunk:?}");
        }
        assert!(buffer.is_empty());
    }

    #[tokio::test]
    async fn an_sse_stream_resumes_after_the_last_acked_event_id() {
        let server = server(|request| {
            let events = match request.header("last-event-id") {
                None => "id: 1\nevent: added\ndata: {\"n\":1}\n\nid: 2\ndata: two\n\n",
                Some("1") => "id: 2\ndata: two\n\n",
                Some("2") => "data: three\n\n",
                _ => "",
            };
            (200, events.to_string())
        })
        .await;
        let config = json!({"read": {"path": "/events", "stream": "sse"}});
        let mut consumer = consumer(&server, config).await;

        let batch = consumer.receive_batch(2).await.expect("events");
        assert_eq!(payload_texts(&batch.messages), [r#"{"n":1}"#, "two"]);
        let metadata = &batch.messages[0].metadata;
        assert_eq!(metadata["sse_id"], "1");
        assert_eq!(metadata["sse_event"], "added");
        let nacked = vec![MessageDisposition::Ack, MessageDisposition::Nack];
        (batch.commit)(nacked).await.expect("commit");

        // The nacked event is asked for again, and the stream goes on behind it.
        assert_eq!(read(&mut consumer, 1).await, ["two"]);
        assert!(read(&mut consumer, 0).await.is_empty());
        assert_eq!(read(&mut consumer, 1).await, ["three"]);
        let requests = server.requests();
        assert_eq!(requests[0].header("accept"), Some("text/event-stream"));
        let resumed: Vec<_> = requests.iter().map(|r| r.header("last-event-id")).collect();
        assert_eq!(resumed, [None, Some("1"), Some("2")]);
    }

    #[tokio::test]
    async fn an_ndjson_stream_saves_the_field_it_resumes_from() {
        let server = server(|request| {
            let lines = match request.target.as_str() {
                "/export?since=" => "{\"seq\":1}\n{\"seq\":2}",
                "/export?since=2" => "{\"seq\":3}\n",
                _ => "",
            };
            (200, lines.to_string())
        })
        .await;
        let (path, url) = store();
        let config = json!({
            "read": {
                "path": "/export?since={cursor}",
                "stream": "ndjson",
                "cursor": {"item": "/seq"},
                "cursor_id": "copy",
                "checkpoint_store": url,
            },
        });
        let mut first = consumer(&server, config.clone()).await;
        assert_eq!(read(&mut first, 2).await, [r#"{"seq":1}"#]);
        // The last line has no line break: it is complete when the response ends.
        assert_eq!(read(&mut first, 2).await, [r#"{"seq":2}"#]);

        let mut second = consumer(&server, config).await;
        assert_eq!(read(&mut second, 1).await, [r#"{"seq":3}"#]);
        assert_eq!(targets(&server), ["/export?since=", "/export?since=2"]);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn an_event_is_delivered_while_the_response_stays_open() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0u8; 1024]).await;
            let event = "data: live\n\n";
            let head = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
            let answer = format!("{head}{:x}\r\n{event}\r\n", event.len());
            socket.write_all(answer.as_bytes()).await.unwrap();
            // The response does not end while the socket is held.
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(socket);
        });
        let read = json!({"path": "/events", "stream": "sse", "polling_interval_ms": 1});
        let config = json!({"url": url, "read": read, "request_timeout_ms": 30_000});
        let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
        let mut consumer = HttpBulkConsumer::new(&config, true).await.unwrap();
        consumer.set_exit_on_empty(true);

        let batch = tokio::time::timeout(Duration::from_secs(5), consumer.receive_batch(10))
            .await
            .expect("delivered before the response ends")
            .expect("event");
        assert_eq!(payload_texts(&batch.messages), ["live"]);
        // A draining route ends on a silent stream.
        let silent = consumer.receive_batch(10).await.expect("silence");
        assert!(silent.messages.is_empty());
        held.abort();
    }

    #[tokio::test]
    async fn a_draining_route_reads_an_export_once() {
        let server = server(|_| (200, "{\"id\":1}\n{\"id\":2}\n{\"id\":3}\n".to_string())).await;
        let config = json!({"read": {"path": "/export", "stream": "ndjson"}});
        let mut consumer = consumer(&server, config).await;
        consumer.set_exit_on_empty(true);

        assert_eq!(read(&mut consumer, 2).await.len(), 2);
        // A line without a position is not read again after a nack.
        assert_eq!(read(&mut consumer, 0).await, [r#"{"id":3}"#]);
        assert!(read(&mut consumer, 0).await.is_empty());
        assert!(read(&mut consumer, 0).await.is_empty());
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_stream_refuses_what_only_a_page_has() {
        for read in [
            json!({"path": "/e", "stream": "sse", "items": "/results"}),
            json!({"path": "/e?limit={limit}", "stream": "ndjson"}),
            json!({"path": "/e", "stream": "sse", "cursor": {"response": "/next"}}),
        ] {
            let config = json!({"url": "http://localhost:1", "read": read});
            let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
            assert!(
                HttpBulkConsumer::new(&config, true).await.is_err(),
                "{:?}",
                config.read
            );
        }
        let failed = server(|_| (404, "no such feed".to_string())).await;
        let mut consumer =
            consumer(&failed, json!({"read": {"path": "/e", "stream": "sse"}})).await;
        let Err(error) = consumer.receive_batch(1).await else {
            panic!("a 404 opens no stream");
        };
        assert!(matches!(error, ConsumerError::Permanent(_)), "{error:#}");
    }
}
