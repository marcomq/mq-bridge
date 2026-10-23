//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Output for HTTP APIs that take many JSON documents in one request.

use super::query::Query;
use super::{quoted, Connection, JSON};
use crate::models::{
    Compression, HttpBulkConfig, HttpBulkDelete, HttpBulkFormat, HttpBulkItems, HttpBulkJob,
    HttpBulkLines, HttpBulkResult, HttpBulkUpsert,
};
use crate::support::change_op::ChangeOp;
use crate::support::compression_pool::{gzip_default, lz4_pooled, zstd_pooled};
use crate::support::interpolation::CompiledTemplate;
use crate::support::poll_job::{poll_until, PollSchedule};
use crate::support::{http_status, ndjson};
use crate::traits::{MessagePublisher, PublisherError, SentBatch};
use crate::CanonicalMessage;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{HeaderValue, CONTENT_ENCODING, CONTENT_TYPE};
use serde_json::Value;
use std::ops::Range;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;
use tracing::{trace, warn};

const IDS: &str = "{ids}";
const JOB_ID: &str = "{id}";
const DOCUMENTS: &str = "{documents}";
const DOCUMENT: &str = "{document}";
const LINE_ID: &str = "{id}";
const NDJSON: &str = "application/x-ndjson";

/// A failed request, kept as text so every message it covered gets its own error.
#[derive(Clone)]
pub(super) struct Failure {
    pub(super) retryable: bool,
    /// The HTTP status, when the target answered with one.
    pub(super) status: Option<u16>,
    pub(super) text: String,
}

impl Failure {
    pub(super) fn permanent(text: String) -> Self {
        Self {
            retryable: false,
            status: None,
            text,
        }
    }

    fn retryable(text: String) -> Self {
        Self {
            retryable: true,
            status: None,
            text,
        }
    }

    /// The target refused the content of the request, which a smaller request may narrow down.
    pub(super) fn refused_content(&self) -> bool {
        matches!(self.status, Some(400 | 409 | 413 | 422))
    }

    pub(super) fn error(&self) -> PublisherError {
        let error = anyhow!("{}", self.text);
        if self.retryable {
            PublisherError::Retryable(error)
        } else {
            PublisherError::NonRetryable(error)
        }
    }
}

enum ResultMode {
    Status,
    Lines(HttpBulkLines),
    Items(HttpBulkItems),
    Job(HttpBulkJob),
}

impl ResultMode {
    fn from_config(result: &HttpBulkResult, request: &str) -> anyhow::Result<Self> {
        match (&result.lines, &result.items, &result.job) {
            (Some(lines), None, None) => Ok(Self::Lines(lines.clone())),
            (None, Some(items), None) => Ok(Self::Items(items.clone())),
            (None, None, Some(job)) => {
                if !job.poll.contains(JOB_ID) {
                    bail!("http_bulk {request}.result.job.poll must contain '{JOB_ID}'");
                }
                if job.timeout_ms == 0 {
                    bail!("http_bulk {request}.result.job.timeout_ms must be greater than 0");
                }
                Ok(Self::Job(job.clone()))
            }
            (None, None, None) => Ok(Self::Status),
            _ => bail!("http_bulk {request}.result sets more than one of 'lines', 'items', 'job'"),
        }
    }
}

struct Request {
    method: reqwest::Method,
    path: String,
    result: ResultMode,
}

impl Request {
    fn new(
        method: Option<&str>,
        path: &str,
        result: &HttpBulkResult,
        name: &str,
    ) -> anyhow::Result<Self> {
        if !path.starts_with('/') {
            bail!("http_bulk {name}.path must start with '/'");
        }
        let method = method.unwrap_or("POST").to_ascii_uppercase();
        Ok(Self {
            method: reqwest::Method::from_bytes(method.as_bytes())
                .with_context(|| format!("http_bulk {name}.method '{method}' is not valid"))?,
            path: path.to_string(),
            result: ResultMode::from_config(result, name)?,
        })
    }
}

fn json_object(payload: &[u8]) -> anyhow::Result<&[u8]> {
    let payload = payload.trim_ascii();
    if payload.first() != Some(&b'{') {
        bail!("the payload is not a JSON object");
    }
    Ok(payload)
}

/// The text before and after a placeholder, which must occur exactly once.
pub(super) fn around(
    template: &str,
    placeholder: &str,
    name: &str,
) -> anyhow::Result<(String, String)> {
    match template.split_once(placeholder) {
        Some((before, after)) if !after.contains(placeholder) => {
            Ok((before.to_string(), after.to_string()))
        }
        _ => bail!("http_bulk {name} must contain '{placeholder}' exactly once"),
    }
}

/// How the ids of a delete request are sent.
enum DeleteBody {
    /// Comma-separated in the path.
    Url,
    /// A JSON array between the two halves of the envelope.
    Array(String, String),
    /// One line per id.
    Lines,
}

/// The `delete.line` template, split around `{id}`.
type DeleteLine = (CompiledTemplate, CompiledTemplate);

/// Documents that go out in one request, with the batch positions they came from.
#[derive(Default)]
struct Chunk {
    body: Vec<u8>,
    indices: Vec<usize>,
}

/// How `upsert.document` turns a message into the document that is sent.
enum Document {
    /// The templates before and after the payload.
    Around(CompiledTemplate, CompiledTemplate),
    /// A template without `{document}` renders the whole document.
    Whole(CompiledTemplate),
}

pub struct HttpBulkPublisher {
    connection: Connection,
    /// Unset on an endpoint that only answers lookups.
    upsert: Option<Request>,
    query: Option<Query>,
    document: Option<Document>,
    format: HttpBulkFormat,
    content_type: HeaderValue,
    action: Option<CompiledTemplate>,
    envelope: (String, String),
    compression: Compression,
    delete: Option<(Request, HttpBulkDelete, DeleteBody, Option<DeleteLine>)>,
    operation: Option<CompiledTemplate>,
    delete_values: Vec<String>,
    max_request_bytes: usize,
    max_documents: usize,
    split_on_reject: bool,
}

impl HttpBulkPublisher {
    pub fn new(config: &HttpBulkConfig) -> anyhow::Result<Self> {
        let connection = Connection::new(config)?;
        let query = config.query.as_ref().map(Query::new).transpose()?;
        if query.is_some()
            && (config.upsert.is_some() || config.delete.is_some() || config.operation.is_some())
        {
            bail!(
                "http_bulk 'query' answers lookups and excludes 'upsert', 'delete' and 'operation'"
            );
        }
        let unused = HttpBulkUpsert::default();
        let upsert = match (&config.upsert, &query) {
            (Some(upsert), _) => upsert,
            (None, Some(_)) => &unused,
            (None, None) => bail!("http_bulk used as an output needs 'upsert' or 'query'"),
        };
        let content_type = upsert.content_type.as_deref().unwrap_or({
            match upsert.format {
                HttpBulkFormat::Ndjson => NDJSON,
                HttpBulkFormat::JsonArray => JSON,
            }
        });
        let ndjson = upsert.format == HttpBulkFormat::Ndjson;
        if upsert.action.is_some() && !ndjson {
            bail!("http_bulk upsert.action needs format 'ndjson'");
        }
        let envelope = match &upsert.envelope {
            Some(_) if ndjson => bail!("http_bulk upsert.envelope needs format 'json_array'"),
            Some(envelope) => around(envelope, DOCUMENTS, "upsert.envelope")?,
            None => Default::default(),
        };
        let document = match &upsert.document {
            Some(document) => {
                let compile = |part: &str| {
                    CompiledTemplate::compile(part, Some(JSON))
                        .context("Invalid http_bulk upsert.document template")
                };
                let (before, document) = if document.contains(DOCUMENT) {
                    let (before, after) = around(document, DOCUMENT, "upsert.document")?;
                    let parts = Document::Around(compile(&before)?, compile(&after)?);
                    (before, parts)
                } else {
                    let whole = compile(document)?;
                    if !whole.reads_message() {
                        bail!("http_bulk upsert.document needs {DOCUMENT} or a token from the message, e.g. ${{message:payload}}");
                    }
                    (document.clone(), Document::Whole(whole))
                };
                if ndjson && !before.trim_start().starts_with('{') {
                    bail!("http_bulk upsert.document must be a JSON object for format 'ndjson'");
                }
                Some(document)
            }
            None => None,
        };
        if upsert.max_documents == Some(0) {
            bail!("http_bulk upsert.max_documents must be at least 1");
        }
        let reports = &upsert.result;
        if upsert.split_on_reject
            && (reports.lines.is_some() || reports.items.is_some() || reports.job.is_some())
        {
            bail!("http_bulk upsert.split_on_reject applies only without upsert.result");
        }
        let delete = match &config.delete {
            Some(delete) => {
                if delete.max_ids == 0 {
                    bail!("http_bulk delete.max_ids must be at least 1");
                }
                let request = Request::new(
                    delete.method.as_deref(),
                    &delete.path,
                    &delete.result,
                    "delete",
                )?;
                let in_url = delete.path.contains(IDS);
                let body = match (&delete.envelope, &delete.line) {
                    (Some(_), _) if in_url => bail!(
                        "http_bulk delete sends the ids in the path, so it takes no 'envelope'"
                    ),
                    _ if in_url => DeleteBody::Url,
                    (Some(envelope), _) => {
                        let (before, after) = around(envelope, IDS, "delete.envelope")?;
                        DeleteBody::Array(before, after)
                    }
                    (None, Some(_)) => DeleteBody::Lines,
                    (None, None) => DeleteBody::Array(String::new(), String::new()),
                };
                let escape = (!in_url).then_some(JSON);
                let line = match &delete.line {
                    Some(line) => {
                        let (before, after) = around(line, LINE_ID, "delete.line")?;
                        let compile = |part: &str| {
                            CompiledTemplate::compile(part, escape)
                                .context("Invalid http_bulk delete.line template")
                        };
                        Some((compile(&before)?, compile(&after)?))
                    }
                    None => None,
                };
                Some((request, delete.clone(), body, line))
            }
            None => None,
        };
        let request = config.upsert.as_ref().map(|upsert| {
            Request::new(
                upsert.method.as_deref(),
                &upsert.path,
                &upsert.result,
                "upsert",
            )
        });
        Ok(Self {
            connection,
            upsert: request.transpose()?,
            query,
            document,
            format: upsert.format,
            content_type: HeaderValue::from_str(content_type)
                .context("http_bulk upsert.content_type is not a valid header value")?,
            action: upsert
                .action
                .as_deref()
                .map(|template| CompiledTemplate::compile(template, Some(JSON)))
                .transpose()
                .context("Invalid http_bulk upsert.action template")?,
            envelope,
            compression: config.compression,
            delete,
            operation: config
                .operation
                .as_deref()
                .map(|template| CompiledTemplate::compile(template, None))
                .transpose()
                .context("Invalid http_bulk operation template")?,
            delete_values: config.delete_values.clone(),
            max_request_bytes: config.max_request_bytes,
            max_documents: upsert.max_documents.unwrap_or(usize::MAX),
            split_on_reject: upsert.split_on_reject,
        })
    }

    fn operation(&self, message: &CanonicalMessage) -> ChangeOp {
        let Some(template) = &self.operation else {
            return ChangeOp::Upsert;
        };
        let rendered = template.render(Some(message));
        let operation = std::str::from_utf8(&rendered).ok().map(str::trim);
        ChangeOp::classify(operation.filter(|op| !op.is_empty()), &self.delete_values)
    }

    fn append_document(
        &self,
        body: &mut Vec<u8>,
        message: &CanonicalMessage,
    ) -> anyhow::Result<()> {
        let wrapped;
        let payload = match &self.document {
            Some(Document::Whole(template)) => {
                wrapped = template.render(Some(message));
                wrapped.as_slice()
            }
            Some(Document::Around(before, after)) => {
                let payload = json_object(&message.payload)?;
                let mut text = before.render(Some(message));
                text.extend_from_slice(payload);
                text.extend_from_slice(&after.render(Some(message)));
                wrapped = text;
                wrapped.as_slice()
            }
            None => json_object(&message.payload)?,
        };
        match self.format {
            HttpBulkFormat::Ndjson => {
                if let Some(action) = &self.action {
                    body.extend_from_slice(&action.render(Some(message)));
                    body.push(b'\n');
                }
                ndjson::append_line(body, payload)
            }
            HttpBulkFormat::JsonArray => {
                if body.is_empty() {
                    body.extend_from_slice(self.envelope.0.as_bytes());
                    body.push(b'[');
                } else {
                    body.push(b',');
                }
                // One broken element would make the whole array invalid.
                serde_json::from_slice::<serde::de::IgnoredAny>(payload)
                    .context("the payload is not valid JSON")?;
                body.extend_from_slice(payload);
                Ok(())
            }
        }
    }

    fn close_body(&self, mut body: Vec<u8>) -> Vec<u8> {
        if self.format == HttpBulkFormat::JsonArray {
            body.push(b']');
            body.extend_from_slice(self.envelope.1.as_bytes());
        }
        body
    }

    /// Frames the run's payloads into request bodies no larger than the limit. A
    /// payload that is not a JSON object fails alone and is left out.
    fn upsert_chunks(
        &self,
        messages: &[CanonicalMessage],
        run: Range<usize>,
        outcomes: &mut [Option<PublisherError>],
    ) -> Vec<Chunk> {
        let mut chunks = Vec::new();
        let mut chunk = Chunk::default();
        for index in run {
            let message = &messages[index];
            let mark = chunk.body.len();
            if let Err(error) = self.append_document(&mut chunk.body, message) {
                chunk.body.truncate(mark);
                outcomes[index] = Some(PublisherError::NonRetryable(error));
                continue;
            }
            let full = chunk.indices.len() >= self.max_documents;
            if mark > 0 && (full || chunk.body.len() > self.max_request_bytes) {
                chunk.body.truncate(mark);
                chunks.push(std::mem::take(&mut chunk));
                // The same payload was accepted a moment ago.
                let _ = self.append_document(&mut chunk.body, message);
            }
            chunk.indices.push(index);
        }
        if !chunk.indices.is_empty() {
            chunks.push(chunk);
        }
        chunks
    }

    async fn upsert(
        &self,
        messages: &[CanonicalMessage],
        run: Range<usize>,
        outcomes: &mut [Option<PublisherError>],
    ) -> Result<(), (usize, Failure)> {
        let Some(upsert) = &self.upsert else {
            let text = "http_bulk has no 'upsert' request configured".to_string();
            return Err((run.start, Failure::permanent(text)));
        };
        for chunk in self.upsert_chunks(messages, run, outcomes) {
            let url = format!("{}{}", self.connection.base, upsert.path);
            let body = Some((self.content_type.clone(), self.close_body(chunk.body)));
            let sent = self.send(upsert, &url, body, chunk.indices.len()).await;
            trace!(count = chunk.indices.len(), path = %upsert.path, "Upserted documents");
            match sent {
                Err(failure) if self.splits(&failure, &chunk.indices) => {
                    self.upsert_halves(upsert, &url, messages, &chunk.indices, outcomes)
                        .await?
                }
                sent => record(sent, &chunk.indices, outcomes)?,
            }
        }
        Ok(())
    }

    fn splits(&self, failure: &Failure, indices: &[usize]) -> bool {
        self.split_on_reject && indices.len() > 1 && failure.refused_content()
    }

    /// Sends a refused request again in halves until the refused documents stand alone.
    async fn upsert_halves(
        &self,
        upsert: &Request,
        url: &str,
        messages: &[CanonicalMessage],
        indices: &[usize],
        outcomes: &mut [Option<PublisherError>],
    ) -> Result<(), (usize, Failure)> {
        let (first, second) = indices.split_at(indices.len() / 2);
        let mut pending = vec![second, first];
        while let Some(part) = pending.pop() {
            let mut body = Vec::new();
            for index in part {
                // The same payloads were framed for the refused request.
                let _ = self.append_document(&mut body, &messages[*index]);
            }
            let body = Some((self.content_type.clone(), self.close_body(body)));
            match self.send(upsert, url, body, part.len()).await {
                Err(failure) if self.splits(&failure, part) => {
                    let (first, second) = part.split_at(part.len() / 2);
                    pending.push(second);
                    pending.push(first);
                }
                sent => record(sent, part, outcomes)?,
            }
        }
        Ok(())
    }

    async fn delete(
        &self,
        messages: &[CanonicalMessage],
        run: Range<usize>,
        outcomes: &mut [Option<PublisherError>],
    ) -> Result<(), (usize, Failure)> {
        let Some((request, delete, shape, line)) = &self.delete else {
            let text = "http_bulk has no 'delete' request configured for a delete message";
            for index in run {
                outcomes[index] = Some(PublisherError::NonRetryable(anyhow!(text)));
            }
            return Ok(());
        };
        // Each id as it goes into the request: a URL list item, an array element or a line.
        let mut ids: Vec<(usize, Vec<u8>)> = Vec::new();
        for index in run {
            let message = &messages[index];
            let id = serde_json::from_slice::<Value>(&message.payload)
                .ok()
                .and_then(|mut document| document.get_mut(&delete.id_field).map(Value::take))
                .filter(|id| id.is_string() || id.is_number());
            let Some(id) = id else {
                outcomes[index] = Some(PublisherError::NonRetryable(anyhow!(
                    "the delete message has no string or number in '{}'",
                    delete.id_field
                )));
                continue;
            };
            let piece = match line {
                Some((before, after)) => {
                    let mut piece = before.render(Some(message));
                    piece.extend_from_slice(id.to_string().as_bytes());
                    piece.extend_from_slice(&after.render(Some(message)));
                    piece
                }
                None => id.to_string().into_bytes(),
            };
            let piece = match shape {
                DeleteBody::Url if line.is_none() => id_in_url(&id).into_bytes(),
                DeleteBody::Url => {
                    let text = String::from_utf8_lossy(&piece);
                    utf8_percent_encode(&text, NON_ALPHANUMERIC)
                        .to_string()
                        .into_bytes()
                }
                DeleteBody::Array(..) if line.is_some() => {
                    // One broken element would make the whole array invalid.
                    if let Err(e) = serde_json::from_slice::<serde::de::IgnoredAny>(&piece) {
                        outcomes[index] = Some(PublisherError::NonRetryable(anyhow!(
                            "delete.line is not valid JSON for this message: {e}"
                        )));
                        continue;
                    }
                    piece
                }
                _ => piece,
            };
            ids.push((index, piece));
        }
        for chunk in ids.chunks(delete.max_ids) {
            let indices: Vec<usize> = chunk.iter().map(|(index, _)| *index).collect();
            let mut url = format!("{}{}", self.connection.base, request.path);
            let joined = |separator: &[u8]| {
                let pieces: Vec<&[u8]> = chunk.iter().map(|(_, piece)| piece.as_slice()).collect();
                pieces.join(separator)
            };
            let body = match shape {
                DeleteBody::Url => {
                    let list = String::from_utf8_lossy(&joined(b",")).into_owned();
                    url = format!(
                        "{}{}",
                        self.connection.base,
                        request.path.replace(IDS, &list)
                    );
                    None
                }
                DeleteBody::Array(before, after) => {
                    let mut body = format!("{before}[").into_bytes();
                    body.extend_from_slice(&joined(b","));
                    body.extend_from_slice(format!("]{after}").as_bytes());
                    Some((HeaderValue::from_static(JSON), body))
                }
                DeleteBody::Lines => {
                    let mut body = joined(b"\n");
                    body.push(b'\n');
                    Some((HeaderValue::from_static(NDJSON), body))
                }
            };
            let sent = self.send(request, &url, body, chunk.len()).await;
            trace!(count = chunk.len(), path = %request.path, "Deleted documents");
            record(sent, &indices, outcomes)?;
        }
        Ok(())
    }

    /// Sends one request and returns the documents the target rejected, as
    /// positions within the request and the reason.
    async fn send(
        &self,
        request: &Request,
        url: &str,
        body: Option<(HeaderValue, Vec<u8>)>,
        documents: usize,
    ) -> Result<Vec<(usize, String)>, Failure> {
        let label = format!("{} {}", request.method, request.path);
        let mut builder = self.connection.request(request.method.clone(), url);
        if let Some((content_type, body)) = body {
            builder = builder.header(CONTENT_TYPE, content_type);
            builder = match self.compress(body).await {
                Ok((body, Some(encoding))) => builder.header(CONTENT_ENCODING, encoding).body(body),
                Ok((body, None)) => builder.body(body),
                Err(error) => {
                    return Err(Failure::permanent(format!(
                        "{label} body could not be compressed: {error}"
                    )))
                }
            };
        }
        let response = self.connection.send(builder).await.map_err(|e| Failure {
            retryable: e.retryable,
            status: None,
            text: format!("{label} failed: {e}"),
        })?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| Failure::retryable(format!("{label} response was cut off: {e}")))?;
        if !status.is_success() {
            return Err(Failure {
                retryable: http_status::is_retryable(status.as_u16()),
                status: Some(status.as_u16()),
                text: format!("{label} answered {status}: {}", quoted(&text)),
            });
        }
        match &request.result {
            ResultMode::Status => Ok(Vec::new()),
            ResultMode::Lines(lines) => rejected_lines(lines, &text, documents, &label),
            ResultMode::Items(items) => rejected_items(items, &text, documents, &label),
            ResultMode::Job(job) => self
                .wait_for_job(job, &text, &label)
                .await
                .map(|()| Vec::new()),
        }
    }

    /// Compresses off the async workers: a body can be tens of MB.
    async fn compress(&self, body: Vec<u8>) -> std::io::Result<(Vec<u8>, Option<&'static str>)> {
        let compression = self.compression;
        if compression == Compression::None {
            return Ok((body, None));
        }
        tokio::task::spawn_blocking(move || {
            Ok(match compression {
                Compression::None => (body, None),
                Compression::Gzip => (gzip_default(&body)?, Some("gzip")),
                Compression::Zstd => (
                    zstd_pooled(&body, zstd::DEFAULT_COMPRESSION_LEVEL)?,
                    Some("zstd"),
                ),
                Compression::Lz4 => (lz4_pooled(&body)?, Some("lz4")),
            })
        })
        .await
        .map_err(std::io::Error::other)?
    }

    async fn wait_for_job(
        &self,
        job: &HttpBulkJob,
        answer: &str,
        label: &str,
    ) -> Result<(), Failure> {
        let id = serde_json::from_str::<Value>(answer)
            .ok()
            .and_then(|answer| answer.pointer(&job.id).and_then(scalar_text))
            .ok_or_else(|| {
                Failure::permanent(format!("{label} answered without a job id at '{}'", job.id))
            })?;
        // The id comes from the server; encoded, it cannot leave its path segment.
        let in_url = utf8_percent_encode(&id, NON_ALPHANUMERIC).to_string();
        let url = format!(
            "{}{}",
            self.connection.base,
            job.poll.replace(JOB_ID, &in_url)
        );
        let timeout = Duration::from_millis(job.timeout_ms);
        let schedule = PollSchedule {
            timeout,
            ..PollSchedule::default()
        };
        let last = Mutex::new("no poll was answered".to_string());
        let (this, url, id, last) = (self, url.as_str(), id.as_str(), &last);
        let seen = move |text: String| *last.lock().unwrap_or_else(PoisonError::into_inner) = text;
        let ended = poll_until(
            schedule,
            |waited| warn!(job = id, waited = ?waited, "http_bulk job is still running"),
            || async move {
                let poll = this.connection.request(reqwest::Method::GET, url);
                let response = match this.connection.send(poll).await {
                    Ok(response) => response,
                    Err(error) if error.retryable => {
                        seen(format!("the poll failed: {error}"));
                        return None;
                    }
                    Err(error) => {
                        return Some(Err(Failure::permanent(format!(
                            "polling job {id} failed: {error}"
                        ))))
                    }
                };
                let status = response.status();
                let text = match response.text().await {
                    Ok(text) => text,
                    Err(error) => {
                        seen(format!("the poll response was cut off: {error}"));
                        return None;
                    }
                };
                if !status.is_success() {
                    if http_status::is_retryable(status.as_u16()) {
                        seen(format!("the poll answered {status}: {}", quoted(&text)));
                        return None;
                    }
                    return Some(Err(Failure::permanent(format!(
                        "polling job {id} answered {status}: {}",
                        quoted(&text)
                    ))));
                }
                let state: Option<Value> = serde_json::from_str(&text).ok();
                let name = state.as_ref().and_then(|state| state.pointer(&job.status));
                // A wrong pointer would otherwise be polled until the deadline.
                let (Some(state), Some(name)) = (&state, name.and_then(Value::as_str)) else {
                    return Some(Err(Failure::permanent(format!(
                        "polling job {id} gave no state at '{}': {}",
                        job.status,
                        quoted(&text)
                    ))));
                };
                if job.succeeded.iter().any(|value| value == name) {
                    Some(Ok(()))
                } else if job.failed.iter().any(|value| value == name) {
                    let reason = job
                        .error
                        .as_deref()
                        .and_then(|pointer| state.pointer(pointer))
                        .and_then(scalar_text)
                        .unwrap_or_else(|| name.to_string());
                    Some(Err(Failure::permanent(format!(
                        "job {id} {name}: {reason}"
                    ))))
                } else {
                    seen(format!("state '{name}'"));
                    None
                }
            },
        )
        .await;
        ended.unwrap_or_else(|| {
            let last = last.lock().unwrap_or_else(PoisonError::into_inner);
            Err(Failure::retryable(format!(
                "job {id} did not end within {timeout:?}; last seen: {last}"
            )))
        })
    }
}

/// Writes the outcome of one request into `outcomes`. A request that may be
/// retried stops the batch, since later messages may depend on it; one that
/// failed for good costs only its own documents.
fn record(
    sent: Result<Vec<(usize, String)>, Failure>,
    indices: &[usize],
    outcomes: &mut [Option<PublisherError>],
) -> Result<(), (usize, Failure)> {
    match sent {
        Ok(rejected) => {
            for (position, reason) in rejected {
                outcomes[indices[position]] =
                    Some(PublisherError::NonRetryable(anyhow!("{reason}")));
            }
            Ok(())
        }
        Err(failure) if failure.retryable => Err((indices[0], failure)),
        Err(failure) => {
            for index in indices {
                outcomes[*index] = Some(failure.error());
            }
            Ok(())
        }
    }
}

pub(super) fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn id_in_url(id: &Value) -> String {
    let text = scalar_text(id).unwrap_or_default();
    utf8_percent_encode(&text, NON_ALPHANUMERIC).to_string()
}

fn rejected_lines(
    lines: &HttpBulkLines,
    text: &str,
    documents: usize,
    label: &str,
) -> Result<Vec<(usize, String)>, Failure> {
    let answers: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if answers.len() != documents {
        return Err(Failure::permanent(format!(
            "{label} answered {} result lines for {documents} documents",
            answers.len()
        )));
    }
    let mut rejected = Vec::new();
    for (position, answer) in answers.into_iter().enumerate() {
        let parsed: Option<Value> = serde_json::from_str(answer).ok();
        let written = parsed
            .as_ref()
            .and_then(|value| value.pointer(&lines.success))
            .and_then(Value::as_bool);
        if written != Some(true) {
            let reason = lines
                .error
                .as_deref()
                .and_then(|pointer| parsed.as_ref()?.pointer(pointer))
                .and_then(scalar_text)
                .unwrap_or_else(|| answer.to_string());
            rejected.push((position, reason));
        }
    }
    Ok(rejected)
}

fn rejected_items(
    items: &HttpBulkItems,
    text: &str,
    documents: usize,
    label: &str,
) -> Result<Vec<(usize, String)>, Failure> {
    let answer: Option<Value> = serde_json::from_str(text).ok();
    let empty = Vec::new();
    let entries = match answer.as_ref().map(|answer| answer.pointer(&items.path)) {
        // A list of failures only is left out when nothing failed.
        Some(None | Some(Value::Null)) if items.index.is_some() => Some(&empty),
        found => found.flatten().and_then(Value::as_array),
    };
    let fits = |entries: &&Vec<Value>| match items.index {
        Some(_) => entries.len() <= documents,
        None => entries.len() == documents,
    };
    let Some(entries) = entries.filter(fits) else {
        return Err(Failure::permanent(format!(
            "{label} answered no array of {documents} entries at '{}': {}",
            items.path,
            quoted(text)
        )));
    };
    let failed = |entry: &Value| {
        let status = items.success.as_ref().and_then(|success| {
            let status = entry.pointer(&success.path).unwrap_or(&Value::Null);
            (*status != success.equals).then_some(status)
        });
        match (entry.pointer(&items.error), status) {
            (None | Some(Value::Null), None) => None,
            (None | Some(Value::Null), Some(status)) => Some(format!("status {status}")),
            (Some(reason), _) => Some(scalar_text(reason).unwrap_or_else(|| reason.to_string())),
        }
    };
    let mut rejected = Vec::new();
    for (position, entry) in entries.iter().enumerate() {
        let Some(reason) = failed(entry) else {
            continue;
        };
        let position = match &items.index {
            None => position,
            Some(pointer) => match entry.pointer(pointer).and_then(Value::as_u64) {
                Some(index) if (index as usize) < documents => index as usize,
                _ => {
                    return Err(Failure::permanent(format!(
                        "{label} answered an entry without a document index below {documents} at '{pointer}': {}",
                        quoted(text)
                    )))
                }
            },
        };
        rejected.push((position, reason));
    }
    Ok(rejected)
}

#[async_trait]
impl MessagePublisher for HttpBulkPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        if let Some(query) = &self.query {
            let (answers, refused) = query
                .answers(&self.connection, self.max_request_bytes, &messages)
                .await;
            if let Some(failure) = refused {
                return Err(failure.error());
            }
            let mut responses = Vec::new();
            let mut failed = Vec::new();
            for (message, answer) in messages.into_iter().zip(answers) {
                match answer {
                    Ok(value) => {
                        let value = value.unwrap_or(Value::Null).to_string();
                        // Failed lookups are left out, so the id is what matches an answer.
                        let id = Some(message.message_id);
                        responses.push(CanonicalMessage::new(value.into_bytes(), id));
                    }
                    Err(failure) => failed.push((message, failure.error())),
                }
            }
            return Ok(SentBatch::Partial {
                responses: Some(responses),
                failed,
            });
        }
        let operations: Vec<ChangeOp> = messages.iter().map(|m| self.operation(m)).collect();
        let mut outcomes: Vec<Option<PublisherError>> = messages.iter().map(|_| None).collect();
        let mut next = 0;
        while next < messages.len() {
            let operation = operations[next];
            let length = operations[next..]
                .iter()
                .take_while(|other| **other == operation)
                .count();
            let run = next..next + length;
            next = run.end;
            let sent = match operation {
                ChangeOp::Upsert => self.upsert(&messages, run, &mut outcomes).await,
                ChangeOp::Delete => self.delete(&messages, run, &mut outcomes).await,
                ChangeOp::Truncate => {
                    for index in run {
                        outcomes[index] = Some(PublisherError::NonRetryable(anyhow!(
                            "http_bulk cannot apply a truncate"
                        )));
                    }
                    Ok(())
                }
            };
            if let Err((from, failure)) = sent {
                for outcome in &mut outcomes[from..] {
                    outcome.get_or_insert_with(|| failure.error());
                }
                break;
            }
        }
        SentBatch::from_outcomes(messages, outcomes)
    }

    /// One request for the whole batch; `None` when an answer reports an error,
    /// which `lookup_each` gives to its own message.
    async fn lookup_batch(
        &self,
        requests: &[CanonicalMessage],
    ) -> Option<Result<Vec<Option<Value>>, PublisherError>> {
        let query = self.query.as_ref().filter(|query| !query.per_message())?;
        let (answers, refused) = query
            .answers(&self.connection, self.max_request_bytes, requests)
            .await;
        if let Some(failure) = refused {
            return Some(Err(failure.error()));
        }
        let failed = answers.iter().filter(|answer| answer.is_err()).count();
        if failed > 0 {
            warn!(
                failed,
                of = requests.len(),
                "http_bulk query: an answer reported an error, the batch is not answered as a whole"
            );
            return None;
        }
        Some(Ok(answers
            .into_iter()
            .map(Result::unwrap_or_default)
            .collect()))
    }

    /// One result per lookup: a message whose answer is an error fails alone.
    async fn lookup_each(
        &self,
        requests: &[CanonicalMessage],
    ) -> Option<Vec<Result<Option<Value>, PublisherError>>> {
        let query = self.query.as_ref().filter(|query| !query.per_message())?;
        let (answers, _) = query
            .answers(&self.connection, self.max_request_bytes, requests)
            .await;
        Some(
            answers
                .into_iter()
                .map(|answer| answer.map_err(|failure| failure.error()))
                .collect(),
        )
    }

    /// With `operation` set the messages are changes, and a delete must not
    /// overtake the upsert of the batch before it.
    fn requires_ordered_publish(&self) -> bool {
        self.operation.is_some()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::plugin::test_support::{StubHttpServer, StubRequest};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn server(
        respond: impl Fn(&StubRequest) -> (u16, String) + Send + Sync + 'static,
    ) -> StubHttpServer {
        StubHttpServer::start(respond).await.expect("stub server")
    }

    fn publisher(server: &StubHttpServer, mut config: Value) -> HttpBulkPublisher {
        config["url"] = json!(server.url());
        let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
        HttpBulkPublisher::new(&config).expect("publisher")
    }

    fn documents(payloads: &[&str]) -> Vec<CanonicalMessage> {
        payloads
            .iter()
            .map(|p| CanonicalMessage::from(*p))
            .collect()
    }

    /// The failed payloads with whether each may be retried, and the error text.
    fn failures(sent: SentBatch) -> Vec<(String, bool, String)> {
        match sent {
            SentBatch::Ack => Vec::new(),
            SentBatch::Partial { failed, .. } => failed
                .into_iter()
                .map(|(message, error)| {
                    let payload = String::from_utf8_lossy(&message.payload).into_owned();
                    let retryable = matches!(error, PublisherError::Retryable(_));
                    (payload, retryable, error.to_string())
                })
                .collect(),
        }
    }

    fn calls(server: &StubHttpServer) -> Vec<(String, String, String)> {
        server
            .requests()
            .into_iter()
            .map(|r| (r.method, r.target, String::from_utf8(r.body).unwrap()))
            .collect()
    }

    #[tokio::test]
    async fn a_batch_is_one_ndjson_request_and_a_bad_payload_fails_alone() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({"headers": {"x-key": "secret"}, "upsert": {"path": "/docs?mode=upsert"}}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":1}"#, "not json", r#"{"id":2}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].0.as_str(), failed[0].1), ("not json", false));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/docs?mode=upsert");
        assert_eq!(
            requests[0].header("content-type"),
            Some("application/x-ndjson")
        );
        assert_eq!(requests[0].header("x-key"), Some("secret"));
        assert_eq!(requests[0].body, b"{\"id\":1}\n{\"id\":2}\n");
    }

    fn refuses_bad_rows(request: &StubRequest) -> (u16, String) {
        if String::from_utf8_lossy(&request.body).contains("bad") {
            (400, r#"{"message":"invalid input"}"#.to_string())
        } else {
            (201, String::new())
        }
    }

    const ROWS: [&str; 5] = [
        r#"{"id":1}"#,
        r#"{"id":2}"#,
        r#"{"id":"bad"}"#,
        r#"{"id":4}"#,
        r#"{"id":5}"#,
    ];

    #[tokio::test]
    async fn a_refused_request_fails_all_its_documents_by_default() {
        let server = server(refuses_bad_rows).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/rows", "format": "json_array"}}),
        );
        let failed = failures(publisher.send_batch(documents(&ROWS)).await.unwrap());
        assert_eq!(failed.len(), 5);
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn split_on_reject_fails_only_the_refused_document() {
        let server = server(refuses_bad_rows).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/rows", "format": "json_array", "split_on_reject": true}}),
        );
        let failed = failures(publisher.send_batch(documents(&ROWS)).await.unwrap());
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert_eq!((failed[0].0.as_str(), failed[0].1), (ROWS[2], false));
        assert!(failed[0].2.contains("invalid input"), "{}", failed[0].2);

        let accepted: Vec<String> = server
            .requests()
            .into_iter()
            .map(|r| String::from_utf8(r.body).unwrap())
            .filter(|body| !body.contains("bad"))
            .collect();
        assert_eq!(
            accepted,
            [r#"[{"id":1},{"id":2}]"#, r#"[{"id":4},{"id":5}]"#]
        );
    }

    #[tokio::test]
    async fn split_on_reject_leaves_other_statuses_alone() {
        let server = server(|_| (403, "no".to_string())).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/rows", "split_on_reject": true}}),
        );
        let failed = failures(publisher.send_batch(documents(&ROWS)).await.unwrap());
        assert_eq!(failed.len(), 5);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn split_on_reject_is_refused_with_a_result() {
        let config: HttpBulkConfig = serde_json::from_value(json!({
            "url": "http://127.0.0.1:9",
            "upsert": {"path": "/rows", "split_on_reject": true,
                "result": {"lines": {"success": "/ok"}}},
        }))
        .expect("config");
        let error = HttpBulkPublisher::new(&config).err().expect("an error");
        assert!(error.to_string().contains("split_on_reject"), "{error}");
    }

    #[tokio::test]
    async fn a_batch_over_the_size_limit_is_split_and_json_array_stays_valid() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({"max_request_bytes": 20, "upsert": {"path": "/docs", "method": "put", "format": "json_array"}}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#]))
            .await
            .unwrap();

        assert!(matches!(sent, SentBatch::Ack));
        let bodies: Vec<String> = calls(&server).into_iter().map(|call| call.2).collect();
        assert_eq!(bodies, [r#"[{"id":1},{"id":2}]"#, r#"[{"id":3}]"#]);
        assert_eq!(server.requests()[0].method, "PUT");
        assert_eq!(
            server.requests()[0].header("content-type"),
            Some("application/json")
        );
    }

    #[tokio::test]
    async fn a_broken_document_fails_alone_in_a_json_array() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/docs", "format": "json_array"}}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":"#, r#"{"id":3}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].0.as_str(), failed[0].1), (r#"{"id":"#, false));
        let bodies: Vec<String> = calls(&server).into_iter().map(|call| call.2).collect();
        assert_eq!(bodies, [r#"[{"id":1},{"id":3}]"#]);
    }

    #[tokio::test]
    async fn a_result_line_per_document_fails_only_the_rejected_one() {
        let server = server(|_| {
            let body = "{\"success\":true}\n{\"success\":false,\"error\":\"Bad field\"}\n";
            (200, body.to_string())
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/import", "content_type": "text/plain",
                "result": {"lines": {"success": "/success", "error": "/error"}}}}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":2}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].0.as_str(), failed[0].1), (r#"{"id":2}"#, false));
        assert!(failed[0].2.contains("Bad field"), "{}", failed[0].2);
        assert_eq!(
            server.requests()[0].header("content-type"),
            Some("text/plain")
        );
    }

    fn job_config() -> Value {
        json!({"upsert": {"path": "/docs", "result": {"job": {
            "id": "/taskUid", "poll": "/tasks/{id}", "status": "/status",
            "succeeded": ["succeeded"], "failed": ["failed", "canceled"],
            "error": "/error/message"}}}})
    }

    #[tokio::test]
    async fn a_job_is_polled_until_it_succeeds() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let server = server(move |request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":7}"#.to_string()),
            _ if seen.fetch_add(1, Ordering::SeqCst) < 2 => {
                (200, r#"{"status":"processing"}"#.to_string())
            }
            _ => (200, r#"{"status":"succeeded"}"#.to_string()),
        })
        .await;
        let sent = publisher(&server, job_config())
            .send_batch(documents(&[r#"{"id":1}"#]))
            .await
            .unwrap();

        assert!(matches!(sent, SentBatch::Ack));
        assert_eq!(polls.load(Ordering::SeqCst), 3);
        assert_eq!(server.requests()[1].target, "/tasks/7");
    }

    fn job_config_with_timeout(timeout_ms: u64) -> Value {
        let mut config = job_config();
        config["upsert"]["result"]["job"]["timeout_ms"] = json!(timeout_ms);
        config
    }

    #[tokio::test]
    async fn a_job_that_never_ends_fails_as_retryable_at_the_deadline() {
        let server = server(|request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":7}"#.to_string()),
            _ => (200, r#"{"status":"processing"}"#.to_string()),
        })
        .await;
        let sent = publisher(&server, job_config_with_timeout(60))
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":2}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 2);
        for (_, retryable, text) in &failed {
            assert!(*retryable, "{text}");
            assert!(text.contains("job 7 did not end within 60ms"), "{text}");
            assert!(text.contains("state 'processing'"), "{text}");
        }
    }

    #[tokio::test]
    async fn a_poll_that_keeps_failing_ends_at_the_deadline_with_its_last_error() {
        let server = server(|request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":7}"#.to_string()),
            _ => (503, "warming up".to_string()),
        })
        .await;
        let sent = publisher(&server, job_config_with_timeout(60))
            .send_batch(documents(&[r#"{"id":1}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert!(failed[0].1, "{}", failed[0].2);
        assert!(failed[0].2.contains("503"), "{}", failed[0].2);
        assert!(failed[0].2.contains("warming up"), "{}", failed[0].2);
    }

    #[tokio::test]
    async fn a_job_that_ends_before_its_deadline_succeeds() {
        let polls = Arc::new(AtomicUsize::new(0));
        let seen = polls.clone();
        let server = server(move |request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":7}"#.to_string()),
            _ if seen.fetch_add(1, Ordering::SeqCst) < 2 => {
                (200, r#"{"status":"processing"}"#.to_string())
            }
            _ => (200, r#"{"status":"succeeded"}"#.to_string()),
        })
        .await;
        let sent = publisher(&server, job_config_with_timeout(10_000))
            .send_batch(documents(&[r#"{"id":1}"#]))
            .await
            .unwrap();

        assert!(matches!(sent, SentBatch::Ack));
    }

    #[test]
    fn a_job_timeout_defaults_to_five_minutes_and_cannot_be_zero() {
        let mut config = job_config();
        config["url"] = json!("http://localhost:1");
        let config: HttpBulkConfig = serde_json::from_value(config).unwrap();
        let job = config.upsert.unwrap().result.job.unwrap();
        assert_eq!(job.timeout_ms, 300_000);

        let mut zero = job_config_with_timeout(0);
        zero["url"] = json!("http://localhost:1");
        let zero: HttpBulkConfig = serde_json::from_value(zero).unwrap();
        let error = HttpBulkPublisher::new(&zero).err().expect("rejected");
        assert!(error.to_string().contains("timeout_ms"), "{error}");
    }

    #[tokio::test]
    async fn a_failed_job_fails_its_documents_for_good_with_the_reason() {
        let server = server(|request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":"a1"}"#.to_string()),
            _ => (
                200,
                r#"{"status":"failed","error":{"message":"no primary key"}}"#.to_string(),
            ),
        })
        .await;
        let sent = publisher(&server, job_config())
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":2}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 2);
        assert!(failed
            .iter()
            .all(|f| !f.1 && f.2.contains("no primary key")));
    }

    #[tokio::test]
    async fn a_job_id_cannot_leave_its_path_segment() {
        let server = server(|request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":"../keys?x=1"}"#.to_string()),
            _ => (200, r#"{"status":"succeeded"}"#.to_string()),
        })
        .await;
        publisher(&server, job_config())
            .send_batch(documents(&[r#"{"id":1}"#]))
            .await
            .unwrap();

        assert_eq!(server.requests()[1].target, "/tasks/%2E%2E%2Fkeys%3Fx%3D1");
    }

    #[tokio::test]
    async fn deletes_keep_their_place_between_upserts() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({"operation": "${metadata:op}", "upsert": {"path": "/docs"},
                "delete": {"path": "/docs/delete-batch", "id_field": "key"}}),
        );
        let mut batch = documents(&[
            r#"{"key":1}"#,
            r#"{"key":1}"#,
            r#"{"key":"b"}"#,
            r#"{"other":3}"#,
            r#"{"key":4}"#,
        ]);
        for message in &mut batch[1..=3] {
            *message = message.clone().with_metadata_kv("op", "DELETE");
        }
        let sent = publisher.send_batch(batch).await.unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!(
            (failed[0].0.as_str(), failed[0].1),
            (r#"{"other":3}"#, false)
        );
        let expected = [
            ("POST", "/docs", "{\"key\":1}\n"),
            ("POST", "/docs/delete-batch", r#"[1,"b"]"#),
            ("POST", "/docs", "{\"key\":4}\n"),
        ];
        let calls = calls(&server);
        let got: Vec<_> = calls
            .iter()
            .map(|c| (c.0.as_str(), c.1.as_str(), c.2.as_str()))
            .collect();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn ids_go_into_the_url_when_the_path_asks_for_them() {
        let server = server(|_| (200, r#"{"num_deleted":2}"#.to_string())).await;
        let publisher = publisher(
            &server,
            json!({"operation": "${metadata:op}", "upsert": {"path": "/docs"},
                "delete": {"method": "DELETE", "path": "/docs?filter_by=id:[{ids}]", "max_ids": 2}}),
        );
        let batch = documents(&[r#"{"id":"a b"}"#, r#"{"id":2}"#, r#"{"id":3}"#])
            .into_iter()
            .map(|message| message.with_metadata_kv("op", "d"))
            .collect();
        let sent = publisher.send_batch(batch).await.unwrap();

        assert!(matches!(sent, SentBatch::Ack));
        let calls = calls(&server);
        assert_eq!(calls[0].0, "DELETE");
        assert_eq!(calls[0].1, "/docs?filter_by=id:[a%20b,2]");
        assert_eq!(calls[1].1, "/docs?filter_by=id:[3]");
        assert!(calls.iter().all(|call| call.2.is_empty()));
    }

    #[tokio::test]
    async fn a_busy_target_fails_the_rest_of_the_batch_as_retryable() {
        let server = server(|request| match request.target.as_str() {
            "/docs" => (200, String::new()),
            _ => (503, "busy".to_string()),
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"operation": "${metadata:op}", "upsert": {"path": "/docs"},
                "delete": {"path": "/delete"}}),
        );
        let mut batch = documents(&[r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":3}"#]);
        batch[1] = batch[1].clone().with_metadata_kv("op", "delete");
        let sent = publisher.send_batch(batch).await.unwrap();

        let failed = failures(sent);
        let payloads: Vec<&str> = failed.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(payloads, [r#"{"id":2}"#, r#"{"id":3}"#]);
        assert!(failed.iter().all(|f| f.1));
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_request_refused_for_good_costs_only_its_own_documents() {
        let server = server(|request| match request.body.starts_with(b"{\"id\":1}") {
            true => (400, "x".repeat(2000)),
            false => (200, String::new()),
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"max_request_bytes": 10, "upsert": {"path": "/docs"}}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":1}"#, r#"{"id":2}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].0.as_str(), failed[0].1), (r#"{"id":1}"#, false));
        assert!(failed[0].2.len() < 700, "{}", failed[0].2.len());
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_poll_answer_without_the_state_fails_instead_of_polling_forever() {
        let server = server(|request| match request.method.as_str() {
            "POST" => (202, r#"{"taskUid":7}"#.to_string()),
            _ => (200, r#"{"state":"done"}"#.to_string()),
        })
        .await;
        let sent = publisher(&server, job_config())
            .send_batch(documents(&[r#"{"id":1}"#]))
            .await
            .unwrap();

        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert!(failed[0].2.contains("/status"), "{}", failed[0].2);
    }

    #[tokio::test]
    async fn only_a_change_stream_asks_for_ordered_batches() {
        let server = server(|_| (200, String::new())).await;
        let plain = publisher(&server, json!({"upsert": {"path": "/docs"}}));
        let changes = publisher(
            &server,
            json!({"operation": "${metadata:op}", "upsert": {"path": "/docs"}}),
        );
        assert!(!plain.requires_ordered_publish());
        assert!(changes.requires_ordered_publish());
    }

    #[tokio::test]
    async fn an_action_line_precedes_each_document_and_array_entries_report_failures() {
        let server = server(|_| {
            let items = json!({"errors": true, "items": [
                {"index": {"status": 201}},
                {"index": {"status": 400, "error": {"reason": "bad year"}}},
            ]});
            (200, items.to_string())
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"upsert": {
                "path": "/_bulk",
                "action": r#"{"index":{"_id":"${payload:id}"}}"#,
                "result": {"items": {"path": "/items", "error": "/index/error/reason"}},
            }}),
        );
        let sent = publisher
            .send_batch(documents(&[r#"{"id":"a\"b"}"#, r#"{"id":2}"#]))
            .await
            .unwrap();

        assert_eq!(
            calls(&server)[0].2,
            "{\"index\":{\"_id\":\"a\\\"b\"}}\n{\"id\":\"a\\\"b\"}\n{\"index\":{\"_id\":\"2\"}}\n{\"id\":2}\n"
        );
        let failed = failures(sent);
        assert_eq!(failed.len(), 1);
        assert_eq!((failed[0].0.as_str(), failed[0].1), (r#"{"id":2}"#, false));
        assert!(failed[0].2.contains("bad year"), "{}", failed[0].2);
    }

    #[tokio::test]
    async fn an_envelope_wraps_the_documents_and_the_ids() {
        let server = server(|_| (200, "[]".to_string())).await;
        let publisher = publisher(
            &server,
            json!({
                "operation": "${metadata:op}",
                "upsert": {"path": "/points", "method": "PUT", "format": "json_array",
                    "envelope": r#"{"points": {documents}, "wait": true}"#},
                "delete": {"path": "/points/delete", "envelope": r#"{"points": {ids}}"#},
            }),
        );
        let mut batch = documents(&[r#"{"id":1}"#, r#"{"id":2}"#, r#"{"id":"x"}"#]);
        batch[2] = batch[2].clone().with_metadata_kv("op", "delete");
        assert!(failures(publisher.send_batch(batch).await.unwrap()).is_empty());

        let calls = calls(&server);
        assert_eq!(
            calls[0].2,
            r#"{"points": [{"id":1},{"id":2}], "wait": true}"#
        );
        assert_eq!(calls[1].2, r#"{"points": ["x"]}"#);
    }

    #[tokio::test]
    async fn a_delete_line_per_id_and_a_compressed_body() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({
                "operation": "${metadata:op}",
                "compression": "gzip",
                "upsert": {"path": "/_bulk"},
                "delete": {"path": "/_bulk", "line": r#"{"delete":{"_id":{id}}}"#},
            }),
        );
        let batch = documents(&[r#"{"id":1}"#, r#"{"id":"x"}"#])
            .into_iter()
            .map(|message| message.with_metadata_kv("op", "delete"))
            .collect();
        assert!(failures(publisher.send_batch(batch).await.unwrap()).is_empty());

        let request = &server.requests()[0];
        assert_eq!(request.header("content-encoding"), Some("gzip"));
        assert_eq!(request.header("content-type"), Some(NDJSON));
        let mut body = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(request.body.as_slice()),
            &mut body,
        )
        .unwrap();
        assert_eq!(
            body,
            "{\"delete\":{\"_id\":1}}\n{\"delete\":{\"_id\":\"x\"}}\n"
        );
    }

    #[tokio::test]
    async fn a_delete_line_goes_into_an_array_and_a_url_list_and_reads_the_payload() {
        let server = server(|_| (200, "{}".to_string())).await;
        let deletes = |paths: &[&str]| -> Vec<CanonicalMessage> {
            documents(paths)
                .into_iter()
                .map(|message| message.with_metadata_kv("op", "delete"))
                .collect()
        };
        let in_array = publisher(
            &server,
            json!({
                "operation": "${metadata:op}",
                "upsert": {"path": "/db/_bulk_docs"},
                "delete": {"path": "/db/_bulk_docs", "envelope": r#"{"docs": {ids}}"#,
                    "line": r#"{"_id": {id}, "_rev": "${payload:_rev}", "_deleted": true}"#},
            }),
        );
        let batch = deletes(&[r#"{"id":"a","_rev":"1-x"}"#, r#"{"id":"b","_rev":"2-\"y"}"#]);
        assert!(failures(in_array.send_batch(batch).await.unwrap()).is_empty());
        let in_url = publisher(
            &server,
            json!({
                "operation": "${metadata:op}",
                "upsert": {"path": "/t"},
                "delete": {"method": "DELETE", "path": "/t?id=in.({ids})", "line": "{id}"},
            }),
        );
        let batch = deletes(&[r#"{"id":"a,b"}"#, r#"{"id":7}"#]);
        assert!(failures(in_url.send_batch(batch).await.unwrap()).is_empty());

        let calls = calls(&server);
        assert_eq!(
            calls[0].2,
            r#"{"docs": [{"_id": "a", "_rev": "1-x", "_deleted": true},{"_id": "b", "_rev": "2-\"y", "_deleted": true}]}"#
        );
        assert_eq!(calls[1].1, "/t?id=in.(%22a%2Cb%22,7)");
    }

    #[tokio::test]
    async fn a_delete_line_that_is_no_json_fails_alone_in_an_array() {
        let server = server(|_| (200, "{}".to_string())).await;
        let publisher = publisher(
            &server,
            json!({
                "operation": "${metadata:op}",
                "upsert": {"path": "/d"},
                "delete": {"path": "/d", "envelope": "{ids}", "line": "{\"id\": {id}, \"n\": ${payload:n}}"},
            }),
        );
        let batch = documents(&[r#"{"id":1,"n":5}"#, r#"{"id":2}"#])
            .into_iter()
            .map(|message| message.with_metadata_kv("op", "delete"))
            .collect();
        let failed = failures(publisher.send_batch(batch).await.unwrap());
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].0.contains("\"id\":2"), "{failed:?}");
        assert_eq!(calls(&server)[0].2, r#"[{"id": 1, "n": 5}]"#);
    }

    #[tokio::test]
    async fn max_documents_caps_each_upsert_request() {
        let server = server(|_| (200, "{}".to_string())).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/d", "max_documents": 2}}),
        );
        let batch = documents(&[
            r#"{"a":1}"#,
            r#"{"a":2}"#,
            r#"{"a":3}"#,
            r#"{"a":4}"#,
            r#"{"a":5}"#,
        ]);
        assert!(failures(publisher.send_batch(batch).await.unwrap()).is_empty());
        let lines: Vec<usize> = calls(&server)
            .iter()
            .map(|(_, _, body)| body.lines().count())
            .collect();
        assert_eq!(lines, [2, 2, 1]);
    }

    #[tokio::test]
    async fn result_items_with_an_index_may_list_only_the_failures() {
        let answers = [
            "{\"insertErrors\": [{\"index\": 1, \"errors\": [{\"message\": \"bad\"}]}]}",
            "{\"insertErrors\": [{\"index\": 9, \"errors\": [{\"message\": \"bad\"}]}]}",
            "{\"kind\": \"bigquery#tableDataInsertAllResponse\"}",
        ];
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let server = server(move |_| {
            (
                200,
                answers[seen.fetch_add(1, Ordering::SeqCst)].to_string(),
            )
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/insertAll", "format": "json_array", "result": {"items":
                {"path": "/insertErrors", "error": "/errors/0/message", "index": "/index"}}}}),
        );
        let failed = failures(
            publisher
                .send_batch(documents(&[r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#]))
                .await
                .unwrap(),
        );
        assert_eq!(failed.len(), 1);
        assert!(failed[0].2.contains("bad"), "{failed:?}");
        let failed = failures(
            publisher
                .send_batch(documents(&[r#"{"a":1}"#]))
                .await
                .unwrap(),
        );
        assert_eq!(failed.len(), 1);
        assert!(failed[0].2.contains("index"), "{failed:?}");
        let sent = publisher.send_batch(documents(&[r#"{"a":1}"#])).await;
        assert!(failures(sent.unwrap()).is_empty());
    }

    #[tokio::test]
    async fn result_items_with_a_success_value_fail_other_statuses_without_an_error() {
        let answer = r#"{"Results": [{"Status": "SUCCESS"}, {"Status": "FAILED"}, {"Status": "SUCCESS", "Error": "late"}, {}]}"#;
        let server = server(move |_| (200, answer.to_string())).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {"path": "/bulk", "format": "json_array", "result": {"items":
                {"path": "/Results", "error": "/Error", "success": {"path": "/Status", "equals": "SUCCESS"}}}}}),
        );
        let batch = documents(&[r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#, r#"{"a":4}"#]);
        let failed = failures(publisher.send_batch(batch).await.unwrap());
        let reasons: Vec<&str> = failed.iter().map(|failure| failure.2.as_str()).collect();
        assert_eq!(failed.len(), 3, "{failed:?}");
        assert!(
            reasons.iter().any(|r| r.contains("\"FAILED\"")),
            "{failed:?}"
        );
        assert!(reasons.iter().any(|r| r.contains("late")), "{failed:?}");
        assert!(reasons.iter().any(|r| r.contains("null")), "{failed:?}");
    }

    #[test]
    fn a_config_that_cannot_work_is_refused_at_startup() {
        let refused = |config: Value| {
            let config: HttpBulkConfig = serde_json::from_value(config).expect("config");
            HttpBulkPublisher::new(&config)
                .err()
                .expect("refused")
                .to_string()
        };
        let lines = json!({"success": "/ok"});
        let job =
            json!({"id": "/id", "poll": "/jobs", "status": "/s", "succeeded": [], "failed": []});
        assert!(refused(json!({"url": "localhost", "upsert": {"path": "/d"}})).contains("URL"));
        assert!(
            refused(json!({"url": "http://h?a=1", "upsert": {"path": "/d"}})).contains("query")
        );
        assert!(refused(json!({"url": "http://h", "upsert": {"path": "d"}})).contains("path"));
        for (tls, expected) in [
            (json!({"required": true}), "https"),
            (json!({"cert_password": "x"}), "cert_password"),
            (json!({"key_file": "k.pem"}), "cert_file"),
        ] {
            let error = refused(json!({"url": "http://h", "tls": tls, "upsert": {"path": "/d"}}));
            assert!(error.contains(expected), "{error}");
        }
        let issuer = json!({"oauth2": {"token_url": "http://issuer/token",
            "client_id": "a", "client_secret": "b"}});
        assert!(
            refused(json!({"url": "https://h", "tls": {"required": true},
            "auth": issuer, "upsert": {"path": "/d"}}))
            .contains("token_url")
        );
        assert!(refused(json!({"url": "http://h", "upsert": {"path": "/d",
            "result": {"job": job.clone()}}}))
        .contains("{id}"));
        assert!(refused(json!({"url": "http://h", "upsert": {"path": "/d",
            "result": {"lines": lines, "job": job}}}))
        .contains("more than one"));
        for (upsert, expected) in [
            (
                json!({"path": "/d", "action": "{}", "format": "json_array"}),
                "ndjson",
            ),
            (
                json!({"path": "/d", "envelope": "{documents}"}),
                "json_array",
            ),
            (
                json!({"path": "/d", "format": "json_array", "envelope": "{}"}),
                "exactly once",
            ),
            (json!({"path": "/d", "max_documents": 0}), "max_documents"),
        ] {
            let error = refused(json!({"url": "http://h", "upsert": upsert}));
            assert!(error.contains(expected), "{error}");
        }
        for (delete, expected) in [
            (
                json!({"path": "/d/{ids}", "envelope": "{ids}"}),
                "in the path",
            ),
            (json!({"path": "/d", "line": "x"}), "exactly once"),
        ] {
            let config = json!({"url": "http://h", "upsert": {"path": "/d"}, "delete": delete});
            let error = refused(config);
            assert!(error.contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn a_document_template_wraps_every_document() {
        let server = server(|_| (200, "{}".to_string())).await;
        let wrapped = publisher(
            &server,
            json!({"upsert": {
                "path": "/docs",
                "format": "json_array",
                "envelope": "{\"value\":{documents}}",
                "document": "{\"@search.action\":\"merge\",\"key\":\"${payload:id}\",\"fields\":{document}}"
            }}),
        );
        wrapped
            .send_batch(documents(&[r#"{"id":"a","n":1}"#, r#"{"id":"b","n":2}"#]))
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(server.requests()[0].body.clone()).unwrap(),
            concat!(
                r#"{"value":[{"@search.action":"merge","key":"a","fields":{"id":"a","n":1}},"#,
                r#"{"@search.action":"merge","key":"b","fields":{"id":"b","n":2}}]}"#
            )
        );

        for (document, reason) in [
            ("{\"a\":{document},\"b\":{document}}", "{document}"),
            ("{\"doc\":{doc}}", "{document} or a token"),
            ("[{document}]", "JSON object"),
            ("\"${message:payload | base64}\"", "JSON object"),
        ] {
            let config = json!({
                "url": "http://localhost:1",
                "upsert": {"path": "/docs", "document": document}
            });
            let config: HttpBulkConfig = serde_json::from_value(config).unwrap();
            let error = HttpBulkPublisher::new(&config).map(|_| ()).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        }
    }

    #[tokio::test]
    async fn a_document_without_placeholder_renders_any_payload() {
        let server = server(|_| (200, String::new())).await;
        let publisher = publisher(
            &server,
            json!({"upsert": {
                "path": "/messages",
                "format": "json_array",
                "envelope": "{\"messages\":{documents}}",
                "document": "{\"id\":0,\"payload\":\"${message:payload | base64}\"}"
            }}),
        );
        let binary = CanonicalMessage::new(vec![0xff, 0x00], None);
        let sent = publisher
            .send_batch(vec![binary, CanonicalMessage::from("not json")])
            .await
            .unwrap();

        assert!(failures(sent).is_empty());
        assert_eq!(
            calls(&server)[0].2,
            r#"{"messages":[{"id":0,"payload":"/wA="},{"id":0,"payload":"bm90IGpzb24="}]}"#
        );
    }
}
