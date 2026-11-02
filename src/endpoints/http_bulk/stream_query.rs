//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `http_bulk` `query` with `stream_to`: each message is one request answered by server-sent events.
//!
//! Every `value` an event holds is sent to `stream_to` as it arrives, for latency only;
//! the whole text is the message's response, so the input is committed after it.
//! A message on `cancel_from` with a request's `correlation_id` stops that request.

use super::publisher::{scalar_text, Failure};
use super::query::Query;
use super::{quoted, Connection};
use crate::models::{HttpBulkConfig, HttpBulkQuery};
use crate::support::http_status;
use crate::support::sse::{find_sse_event_end, parse_sse_event};
use crate::traits::{
    ConsumerError, MessageConsumer, MessageDisposition, MessagePublisher, PublisherError, SentBatch,
};
use crate::CanonicalMessage;
use anyhow::{anyhow, bail, Context};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reqwest::header::{HeaderValue, ACCEPT, CONTENT_TYPE};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// Longest silence of the server when `request_timeout_ms` is unset.
const DEFAULT_SILENCE_MS: u64 = 300_000;
const CANCEL_BATCH: usize = 64;
/// How long a cancel waits for a request that has not started yet.
const EARLY_CANCEL_TTL: Duration = Duration::from_secs(30);
/// Most such cancels kept; the oldest is dropped first.
const EARLY_CANCELS: usize = 1024;
const EVENT_STREAM: &str = "text/event-stream";
/// Longest event the stream may hold unterminated.
const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub(crate) struct StreamQueryPublisher {
    connection: Connection,
    query: Query,
    label: String,
    events: Events,
    parse_value: bool,
    stream_to: Arc<dyn MessagePublisher>,
    running: Option<Arc<Running>>,
    listener: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for StreamQueryPublisher {
    fn drop(&mut self) {
        if let Some(listener) = &self.listener {
            listener.abort();
        }
    }
}

/// How a stream ended.
enum End {
    Complete,
    Cancelled,
    /// Broke off after the first text: asking again may help.
    Failed(String),
    /// An error event before any text: the request itself is refused.
    Refused(String),
}

impl End {
    fn status(&self) -> &'static str {
        match self {
            End::Complete => "complete",
            End::Cancelled => "cancelled",
            End::Failed(_) | End::Refused(_) => "error",
        }
    }
}

/// One parsed server-sent event.
enum Event {
    Delta {
        content: Option<String>,
        stop_reason: Option<String>,
    },
    Done,
    Error(String),
}

/// Where an event holds its text, error and stop reason, and the data that ends the stream.
struct Events {
    value: String,
    error: Option<String>,
    stop_reason: Option<String>,
    done: Option<String>,
}

impl Events {
    /// Takes the next event that matters out of `buffer`; `None` when it holds none yet.
    fn next(&self, buffer: &mut Vec<u8>) -> Option<Event> {
        while let Some(end) = find_sse_event_end(buffer) {
            let terminator = if buffer[end..].starts_with(b"\r\n\r\n") {
                4
            } else {
                2
            };
            let raw: Vec<u8> = buffer.drain(..end + terminator).collect();
            // A whole event is framed, so no character is split here.
            let Some(event) = parse_sse_event(&String::from_utf8_lossy(&raw[..end])) else {
                continue;
            };
            let data = String::from_utf8_lossy(&event.payload);
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if self.done.as_deref() == Some(data) {
                return Some(Event::Done);
            }
            let Ok(event) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            let at = |pointer: &Option<String>| {
                let pointer = pointer.as_deref()?;
                event.pointer(pointer).filter(|value| !value.is_null())
            };
            if let Some(error) = at(&self.error) {
                return Some(Event::Error(
                    scalar_text(error).unwrap_or_else(|| error.to_string()),
                ));
            }
            let content = event.pointer(&self.value).and_then(Value::as_str);
            let stop_reason = at(&self.stop_reason).and_then(scalar_text);
            if content.is_none() && stop_reason.is_none() {
                continue;
            }
            return Some(Event::Delta {
                content: content.map(str::to_string),
                stop_reason,
            });
        }
        None
    }
}

impl StreamQueryPublisher {
    pub(crate) async fn new(route_name: &str, config: &HttpBulkConfig) -> anyhow::Result<Self> {
        let mut config = config.clone();
        let query: HttpBulkQuery = config
            .query
            .take()
            .context("http_bulk stream_to needs a 'query'")?;
        if config.upsert.is_some() || config.delete.is_some() || config.operation.is_some() {
            bail!(
                "http_bulk 'query' answers lookups and excludes 'upsert', 'delete' and 'operation'"
            );
        }
        let compiled = Query::new(&query)?;
        let (Some(stream_to), Some(value)) = (query.stream_to.as_deref(), query.value.clone())
        else {
            bail!("http_bulk query.stream_to needs 'per_message' and 'value'");
        };
        config.request_timeout_ms.get_or_insert(DEFAULT_SILENCE_MS);
        let connection = Connection::streaming(&config)?;
        let stream_to = crate::endpoints::create_publisher_from_route(route_name, stream_to)
            .await
            .context("http_bulk query.stream_to")?;
        let (running, listener) = match query.cancel_from.as_deref() {
            Some(endpoint) => {
                let consumer = crate::endpoints::create_consumer_from_route(route_name, endpoint)
                    .await
                    .context("http_bulk query.cancel_from")?;
                let running = Arc::new(Running::default());
                let listener = tokio::spawn(listen(consumer, Arc::downgrade(&running)));
                (Some(running), Some(listener))
            }
            None => (None, None),
        };
        Ok(Self {
            connection,
            label: compiled.label(),
            query: compiled,
            events: Events {
                value,
                error: query.error,
                stop_reason: query.stop_reason,
                done: query.stream_done,
            },
            parse_value: query.parse_value,
            stream_to,
            running,
            listener,
        })
    }

    async fn one(&self, message: &CanonicalMessage) -> Result<CanonicalMessage, PublisherError> {
        let correlation_id = message
            .metadata
            .get("correlation_id")
            .cloned()
            .unwrap_or_else(|| format!("{:032x}", message.message_id));
        let mut tokens = Tokens::new(correlation_id);
        let attempt = AssertUnwindSafe(self.attempt(message, &mut tokens))
            .catch_unwind()
            .await;
        if let Ok(result) = attempt {
            return result;
        }
        // A reader waiting for the last token must not wait forever.
        let reason = format!("{} panicked", self.label);
        let end = End::Failed(reason.clone());
        tokens
            .send(&*self.stream_to, &reason, Some((&end, None)))
            .await;
        Err(PublisherError::NonRetryable(anyhow!(reason)))
    }

    async fn attempt(
        &self,
        message: &CanonicalMessage,
        tokens: &mut Tokens,
    ) -> Result<CanonicalMessage, PublisherError> {
        let entry = self
            .running
            .as_ref()
            .map(|r| r.enter(&tokens.correlation_id));
        let cancelled = entry.as_ref().map(|e| e.token.clone()).unwrap_or_default();
        let label = &self.label;
        let started = match self.start(message, &cancelled).await {
            Ok(started) => started,
            Err(error) => {
                // Every attempt ends with a last token message, also one the server never streamed.
                let reason = reason(&error);
                let end = End::Refused(reason.clone());
                tokens
                    .send(&*self.stream_to, &reason, Some((&end, None)))
                    .await;
                return Err(error);
            }
        };
        let mut text = String::new();
        let mut stop_reason = None;
        let end = match started {
            None => End::Cancelled,
            Some(response) => {
                self.read(response, &cancelled, tokens, &mut text, &mut stop_reason)
                    .await
            }
        };
        drop(entry);

        let content = match &end {
            End::Failed(reason) | End::Refused(reason) => reason.as_str(),
            _ => "",
        };
        tokens
            .send(
                &*self.stream_to,
                content,
                Some((&end, stop_reason.as_deref())),
            )
            .await;
        match end {
            End::Failed(reason) => {
                return Err(PublisherError::Retryable(anyhow!(
                    "{label} stream broke off: {reason}"
                )))
            }
            End::Refused(reason) => {
                return Err(PublisherError::NonRetryable(anyhow!(
                    "{label} refused the request: {reason}"
                )))
            }
            _ => {}
        }
        let value = match end {
            End::Complete if self.parse_value => serde_json::from_str(&text).map_err(|error| {
                PublisherError::NonRetryable(anyhow!(
                    "the answer is not JSON ({error}): {}",
                    quoted(&text)
                ))
            })?,
            _ => Value::String(text),
        };
        let mut response =
            CanonicalMessage::new(value.to_string().into_bytes(), Some(message.message_id))
                .with_metadata_kv("correlation_id", tokens.correlation_id.clone())
                .with_metadata_kv("mqb.stream.attempt", tokens.attempt.clone())
                .with_metadata_kv("mqb.stream.status", end.status());
        if let Some(reason) = stop_reason {
            response = response.with_metadata_kv("mqb.stream.stop_reason", reason);
        }
        Ok(response)
    }

    /// Sends the request; the event stream to read, or `None` when cancelled first.
    async fn start(
        &self,
        message: &CanonicalMessage,
        cancelled: &CancellationToken,
    ) -> Result<Option<reqwest::Response>, PublisherError> {
        let body = self
            .query
            .part(message, 1)
            .map_err(|text| PublisherError::NonRetryable(anyhow!(text)))?;
        let request = self
            .query
            .request(&self.connection, body)
            .header(ACCEPT, HeaderValue::from_static(EVENT_STREAM));
        let label = &self.label;
        let response = tokio::select! {
            biased;
            _ = cancelled.cancelled() => return Ok(None),
            sent = self.connection.send(request) => sent.map_err(|error| Failure {
                retryable: error.retryable,
                status: None,
                text: format!("{label} failed: {error}"),
            }
            .error())?,
        };
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let failure = Failure {
                retryable: http_status::is_retryable(status.as_u16()),
                status: Some(status.as_u16()),
                text: format!("{label} answered {status}: {}", quoted(&body)),
            };
            return Err(failure.error());
        }
        if !is_event_stream(&response) {
            let kind = response.headers().get(CONTENT_TYPE);
            let kind = kind.and_then(|v| v.to_str().ok()).unwrap_or("no type");
            let kind = kind.to_string();
            let body = response.text().await.unwrap_or_default();
            return Err(PublisherError::NonRetryable(anyhow!(
                "{label} answered '{kind}', not {EVENT_STREAM}: {}",
                quoted(&body)
            )));
        }
        Ok(Some(response))
    }

    /// Reads the event stream to its end, sending each text to `stream_to`.
    async fn read(
        &self,
        mut response: reqwest::Response,
        cancelled: &CancellationToken,
        tokens: &mut Tokens,
        text: &mut String,
        stop_reason: &mut Option<String>,
    ) -> End {
        // Without a marker or a stop reason to wait for, the end of the stream is the end.
        let open_end = self.events.done.is_none() && self.events.stop_reason.is_none();
        let mut buffer = Vec::new();
        loop {
            let chunk = tokio::select! {
                biased;
                _ = cancelled.cancelled() => return End::Cancelled,
                chunk = response.chunk() => chunk,
            };
            match chunk {
                Ok(Some(bytes)) => buffer.extend_from_slice(&bytes),
                Ok(None) if open_end || stop_reason.is_some() => return End::Complete,
                Ok(None) => return End::Failed("the stream ended before it was finished".into()),
                Err(error) => return End::Failed(format!("the stream was cut off: {error}")),
            }
            while let Some(event) = self.events.next(&mut buffer) {
                match event {
                    Event::Done => return End::Complete,
                    Event::Error(reason) if text.is_empty() => return End::Refused(reason),
                    Event::Error(reason) => return End::Failed(reason),
                    Event::Delta {
                        content,
                        stop_reason: reason,
                    } => {
                        if let Some(reason) = reason {
                            *stop_reason = Some(reason);
                        }
                        if let Some(content) = content.filter(|c| !c.is_empty()) {
                            tokens.send(&*self.stream_to, &content, None).await;
                            text.push_str(&content);
                        }
                    }
                }
            }
            if buffer.len() > MAX_EVENT_BYTES {
                return End::Failed(format!("an event is longer than {MAX_EVENT_BYTES} bytes"));
            }
        }
    }
}

/// The error text alone, without the retryable / non-retryable prefix.
fn reason(error: &PublisherError) -> String {
    match error {
        PublisherError::Retryable(error)
        | PublisherError::NonRetryable(error)
        | PublisherError::Connection(error) => format!("{error:#}"),
    }
}

fn is_event_stream(response: &reqwest::Response) -> bool {
    let kind = response.headers().get(CONTENT_TYPE);
    let kind = kind
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    kind.trim_start()
        .to_ascii_lowercase()
        .starts_with(EVENT_STREAM)
}

/// The token messages of one attempt.
struct Tokens {
    correlation_id: String,
    attempt: String,
    index: usize,
    /// Cleared after the last token or the first failed send; later tokens are not sent.
    healthy: bool,
}

impl Tokens {
    fn new(correlation_id: String) -> Self {
        Self {
            correlation_id,
            attempt: fast_uuid_v7::format_uuid(fast_uuid_v7::gen_id()).to_string(),
            index: 0,
            healthy: true,
        }
    }

    async fn send(
        &mut self,
        sink: &dyn MessagePublisher,
        content: &str,
        end: Option<(&End, Option<&str>)>,
    ) {
        if !self.healthy {
            return;
        }
        let mut message = CanonicalMessage::from_vec(content.as_bytes())
            .with_metadata_kv("correlation_id", self.correlation_id.clone())
            .with_metadata_kv("mqb.stream.attempt", self.attempt.clone())
            .with_metadata_kv("mqb.stream.index", self.index.to_string())
            .with_metadata_kv("mqb.stream.end", end.is_some().to_string());
        if let Some((end, finish_reason)) = end {
            message = message.with_metadata_kv("mqb.stream.status", end.status());
            if let Some(reason) = finish_reason {
                message = message.with_metadata_kv("mqb.stream.stop_reason", reason);
            }
        }
        self.index += 1;
        self.healthy = end.is_none();
        if let Err(error) = sink.send(message).await {
            self.healthy = false;
            warn!(
                correlation_id = %self.correlation_id,
                "http_bulk stream_to failed, the answer is read on without tokens: {error}"
            );
        }
    }
}

/// Requests in flight by correlation id, and cancels that came before their request.
#[derive(Default)]
struct Running(Mutex<Requests>);

#[derive(Default)]
struct Requests {
    /// Each with its cancel token and how many requests share the id.
    running: HashMap<String, (CancellationToken, usize)>,
    /// Oldest first; a request that starts within `EARLY_CANCEL_TTL` is cancelled at once.
    early: VecDeque<(String, Instant)>,
}

impl Running {
    fn enter(self: &Arc<Self>, id: &str) -> Entry {
        let mut requests = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let early = requests.take_early(id);
        let (token, count) = requests.running.entry(id.to_string()).or_default();
        if early {
            token.cancel();
        }
        *count += 1;
        Entry {
            running: self.clone(),
            id: id.to_string(),
            token: token.clone(),
        }
    }

    fn cancel(&self, id: &str) {
        let mut requests = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match requests.running.get(id) {
            Some((token, _)) => token.cancel(),
            None => requests.remember(id),
        }
    }
}

impl Requests {
    fn remember(&mut self, id: &str) {
        let now = Instant::now();
        while self.early.front().is_some_and(|(_, at)| {
            now.duration_since(*at) > EARLY_CANCEL_TTL || self.early.len() >= EARLY_CANCELS
        }) {
            self.early.pop_front();
        }
        self.early.push_back((id.to_string(), now));
    }

    fn take_early(&mut self, id: &str) -> bool {
        let Some(at) = self.early.iter().position(|(early, _)| early == id) else {
            return false;
        };
        let (_, when) = self.early.remove(at).expect("found");
        when.elapsed() <= EARLY_CANCEL_TTL
    }
}

struct Entry {
    running: Arc<Running>,
    id: String,
    token: CancellationToken,
}

impl Drop for Entry {
    fn drop(&mut self) {
        let mut requests = self
            .running
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((_, count)) = requests.running.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                requests.running.remove(&self.id);
            }
        }
    }
}

/// Cancels the request each message names: its `correlation_id`, else its payload.
async fn listen(mut consumer: Box<dyn MessageConsumer>, running: std::sync::Weak<Running>) {
    loop {
        let batch = match consumer.receive_batch(CANCEL_BATCH).await {
            Ok(batch) => batch,
            Err(ConsumerError::EndOfStream) => return,
            Err(error) => {
                warn!("http_bulk cancel_from failed: {error}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let Some(running) = running.upgrade() else {
            return;
        };
        for message in &batch.messages {
            match message.metadata.get("correlation_id") {
                Some(id) => running.cancel(id),
                None => running.cancel(message.get_payload_str().trim()),
            }
        }
        let acks = vec![MessageDisposition::Ack; batch.messages.len()];
        if let Err(error) = (batch.commit)(acks).await {
            warn!("http_bulk cancel_from commit failed: {error}");
        }
    }
}

#[async_trait]
impl MessagePublisher for StreamQueryPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if messages.is_empty() {
            return Ok(SentBatch::Ack);
        }
        // Collected first: a closure passed to `map` is not general enough over the borrow.
        let calls: Vec<_> = messages.iter().map(|message| self.one(message)).collect();
        let answers: Vec<_> = futures::stream::iter(calls)
            .buffered(self.query.concurrency())
            .collect()
            .await;
        let mut responses = Vec::new();
        let mut failed = Vec::new();
        for (message, answer) in messages.into_iter().zip(answers) {
            match answer {
                Ok(response) => responses.push(response),
                Err(error) => failed.push((message, error)),
            }
        }
        Ok(SentBatch::Partial {
            responses: Some(responses),
            failed,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use crate::endpoints::http_bulk::builtin_preset;
    use crate::endpoints::memory::{get_or_create_channel, MemoryChannel};
    use crate::models::MemoryConfig;
    use crate::traits::{MessagePublisher, PublisherError, Sent};
    use crate::CanonicalMessage;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    const SSE: &str =
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";

    /// Serves one request: `head`, then each chunk after a pause; `hold` keeps it open.
    async fn serve(head: String, chunks: Vec<Vec<u8>>, hold: bool) -> (String, JoinHandle<Value>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            let body = loop {
                let read = socket.read(&mut buffer).await.unwrap();
                request.extend_from_slice(&buffer[..read]);
                let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map(|value| value.trim().parse().unwrap())
                    .unwrap_or(0);
                if request.len() >= end + 4 + length {
                    break request[end + 4..end + 4 + length].to_vec();
                }
            };
            socket.write_all(head.as_bytes()).await.unwrap();
            for chunk in chunks {
                socket.write_all(&chunk).await.unwrap();
                socket.flush().await.unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            if hold {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            serde_json::from_slice(&body).unwrap()
        });
        (url, task)
    }

    fn delta(text: &str) -> Vec<u8> {
        let chunk = json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]});
        format!("data: {chunk}\n\n").into_bytes()
    }

    fn finished() -> Vec<u8> {
        b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
            .to_vec()
    }

    fn topic() -> String {
        format!("chat-stream-{}", fast_uuid_v7::gen_id())
    }

    async fn publisher(url: &str, extra: Value) -> (Box<dyn MessagePublisher>, MemoryChannel) {
        let topic = topic();
        let mut config = json!({
            "url": url,
            "model": "m",
            "prompt": "${message:payload}",
            "stream_to": {"memory": {"topic": topic}}
        });
        for (key, value) in extra.as_object().unwrap() {
            match value {
                Value::Null => config.as_object_mut().unwrap().remove(key),
                value => config
                    .as_object_mut()
                    .unwrap()
                    .insert(key.clone(), value.clone()),
            };
        }
        let factory = builtin_preset("openai_chat").unwrap();
        let publisher = factory.create_publisher("route", &config).await.unwrap();
        (
            publisher,
            get_or_create_channel(&MemoryConfig::new(topic, None)),
        )
    }

    fn meta<'a>(message: &'a CanonicalMessage, key: &str) -> &'a str {
        message
            .metadata
            .get(key)
            .map(String::as_str)
            .unwrap_or_default()
    }

    fn answer(sent: Result<Sent, PublisherError>) -> CanonicalMessage {
        match sent {
            Ok(Sent::Response(response)) => response,
            other => panic!("expected a response, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deltas_arrive_in_order_and_the_response_holds_the_whole_answer() {
        let mut chunks = vec![delta("Hel"), delta("lo"), finished()];
        chunks.push(delta("after done"));
        let (url, server) = serve(SSE.into(), chunks, false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let request = CanonicalMessage::from("hi").with_metadata_kv("correlation_id", "c1");
        let response = answer(publisher.send(request).await);
        assert_eq!(response.get_payload_str(), "\"Hello\"");
        assert_eq!(meta(&response, "mqb.stream.status"), "complete");
        assert_eq!(meta(&response, "mqb.stream.stop_reason"), "stop");
        assert_eq!(meta(&response, "correlation_id"), "c1");

        let tokens = tokens.drain_messages();
        let texts: Vec<_> = tokens.iter().map(|t| t.get_payload_str()).collect();
        assert_eq!(texts, ["Hel", "lo", ""]);
        for (index, token) in tokens.iter().enumerate() {
            assert_eq!(meta(token, "mqb.stream.index"), index.to_string());
            assert_eq!(meta(token, "correlation_id"), "c1");
            assert_eq!(
                meta(token, "mqb.stream.attempt"),
                meta(&response, "mqb.stream.attempt")
            );
            assert_eq!(meta(token, "mqb.stream.end"), (index == 2).to_string());
        }
        assert_eq!(meta(&tokens[2], "mqb.stream.status"), "complete");
        assert_eq!(meta(&tokens[2], "mqb.stream.stop_reason"), "stop");

        let body = server.await.unwrap();
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
    }

    #[tokio::test]
    async fn an_error_event_ends_the_tokens_and_fails_retryable() {
        let chunks = vec![
            delta("a"),
            b"data: {\"error\":{\"message\":\"overloaded\"}}\n\n".to_vec(),
        ];
        let (url, _server) = serve(SSE.into(), chunks, false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::Retryable(_)), "{error}");
        assert!(error.to_string().contains("overloaded"), "{error}");
        let tokens = tokens.drain_messages();
        let last = tokens.last().unwrap();
        assert_eq!(last.get_payload_str(), "overloaded");
        assert_eq!(meta(last, "mqb.stream.status"), "error");
    }

    #[tokio::test]
    async fn an_unterminated_event_fails_once_it_outgrows_the_limit() {
        let chunks = vec![b"data: ".to_vec(), vec![b'x'; super::MAX_EVENT_BYTES + 1]];
        let (url, _server) = serve(SSE.into(), chunks, true).await;
        let (publisher, _tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("longer than"), "{error}");
    }

    #[tokio::test]
    async fn a_refused_request_quotes_the_server_in_its_last_token() {
        let body = r#"{"error":{"message":"bad key"}}"#;
        let head = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, _server) = serve(head, vec![], false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::NonRetryable(_)), "{error}");
        assert!(error.to_string().contains("bad key"), "{error}");
        let tokens = tokens.drain_messages();
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].get_payload_str().contains("bad key"));
        assert_eq!(meta(&tokens[0], "mqb.stream.index"), "0");
        assert_eq!(meta(&tokens[0], "mqb.stream.end"), "true");
        assert_eq!(meta(&tokens[0], "mqb.stream.status"), "error");
    }

    #[tokio::test]
    async fn a_stream_that_ends_before_done_fails_retryable() {
        let (url, _server) = serve(SSE.into(), vec![delta("a")], false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::Retryable(_)), "{error}");
        let tokens = tokens.drain_messages();
        assert_eq!(meta(tokens.last().unwrap(), "mqb.stream.status"), "error");
    }

    #[tokio::test]
    async fn a_failing_stream_to_still_gives_the_answer() {
        let chunks = vec![delta("Hel"), delta("lo"), finished()];
        let (url, _server) = serve(SSE.into(), chunks, false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        tokens.close();
        let response = answer(publisher.send(CanonicalMessage::from("hi")).await);
        assert_eq!(response.get_payload_str(), "\"Hello\"");
    }

    #[tokio::test]
    async fn a_cancel_stops_a_silent_server_and_keeps_the_partial_answer() {
        let (url, _server) = serve(SSE.into(), vec![delta("par")], true).await;
        let cancel = topic();
        let extra = json!({"cancel_from": {"memory": {"topic": cancel}}});
        let (publisher, tokens) = publisher(&url, extra).await;
        let cancels = get_or_create_channel(&MemoryConfig::new(cancel, None));
        let request = CanonicalMessage::from("hi").with_metadata_kv("correlation_id", "c9");
        let sent = tokio::spawn(async move { publisher.send(request).await });
        while tokens.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let stop = CanonicalMessage::from("").with_metadata_kv("correlation_id", "c9");
        cancels.send_message(stop).await.unwrap();
        let sent = tokio::time::timeout(Duration::from_secs(3), sent).await;
        let response = answer(sent.expect("cancelled in time").unwrap());
        assert_eq!(response.get_payload_str(), "\"par\"");
        assert_eq!(meta(&response, "mqb.stream.status"), "cancelled");
        let tokens = tokens.drain_messages();
        assert_eq!(
            meta(tokens.last().unwrap(), "mqb.stream.status"),
            "cancelled"
        );
    }

    #[tokio::test]
    async fn a_cancel_before_its_request_stops_it_at_once() {
        // The server answers once: only the second request reaches it.
        let (url, _server) = serve(SSE.into(), vec![delta("ok"), finished()], false).await;
        let cancel = topic();
        let extra = json!({"cancel_from": {"memory": {"topic": cancel}}});
        let (publisher, tokens) = publisher(&url, extra).await;
        let cancels = get_or_create_channel(&MemoryConfig::new(cancel, None));
        let stop = CanonicalMessage::from("").with_metadata_kv("correlation_id", "early");
        cancels.send_message(stop).await.unwrap();
        while !cancels.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let request = CanonicalMessage::from("hi").with_metadata_kv("correlation_id", "early");
        let sent = tokio::time::timeout(Duration::from_secs(3), publisher.send(request)).await;
        let response = answer(sent.expect("cancelled in time"));
        assert_eq!(meta(&response, "mqb.stream.status"), "cancelled");
        assert_eq!(
            meta(&tokens.drain_messages()[0], "mqb.stream.status"),
            "cancelled"
        );

        // The cancel was used up: the next request with that id runs.
        let request = CanonicalMessage::from("hi").with_metadata_kv("correlation_id", "early");
        let response = answer(publisher.send(request).await);
        assert_eq!(meta(&response, "mqb.stream.status"), "complete");
        assert_eq!(response.get_payload_str(), "\"ok\"");
    }

    #[test]
    fn early_cancels_are_bounded_and_used_once() {
        let mut requests = super::Requests::default();
        for id in 0..super::EARLY_CANCELS + 5 {
            requests.remember(&id.to_string());
        }
        assert_eq!(requests.early.len(), super::EARLY_CANCELS);
        assert!(!requests.take_early("0"));
        let last = (super::EARLY_CANCELS + 4).to_string();
        assert!(requests.take_early(&last));
        assert!(!requests.take_early(&last));
    }

    #[tokio::test]
    async fn the_payload_can_be_the_messages() {
        let (url, server) = serve(SSE.into(), vec![delta("ok"), finished()], false).await;
        let extra = json!({"prompt": null, "messages_from_payload": true});
        let (publisher, _tokens) = publisher(&url, extra).await;
        let messages = json!([{"role": "system", "content": "be \"brief\""}, {"role": "user", "content": "hi"}]);
        let request = CanonicalMessage::from(messages.to_string().as_str());
        answer(publisher.send(request).await);
        assert_eq!(server.await.unwrap()["messages"], messages);
    }

    #[tokio::test]
    async fn a_payload_that_is_not_one_array_is_refused_before_the_request() {
        let extra = json!({"prompt": null, "messages_from_payload": true});
        let (publisher, _tokens) = publisher("http://127.0.0.1:9", extra).await;
        for payload in [
            &br#"[{"role":"user","content":"hi"}],"model":"x""#[..],
            br#"{"a":1}"#,
            b"[\"\xff\"]",
        ] {
            let error = publisher
                .send(CanonicalMessage::new(payload.to_vec(), None))
                .await
                .unwrap_err();
            assert!(matches!(error, PublisherError::NonRetryable(_)), "{error}");
        }
    }

    #[tokio::test]
    async fn the_payload_can_be_the_messages_without_streaming() {
        let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, server) = serve(head, vec![], false).await;
        let config = json!({"url": url, "model": "m", "messages_from_payload": true});
        let factory = builtin_preset("openai_chat").unwrap();
        let publisher = factory.create_publisher("route", &config).await.unwrap();
        let messages = json!([{"role": "user", "content": "hi"}]);
        let request = CanonicalMessage::from(messages.to_string().as_str());
        let response = answer(publisher.send(request).await);
        assert_eq!(response.get_payload_str(), "\"ok\"");
        let body = server.await.unwrap();
        assert_eq!(body["messages"], messages);
        assert!(body.get("stream").is_none());
    }

    #[tokio::test]
    async fn a_character_split_across_chunks_stays_whole() {
        let event = delta("ü");
        let split = event.iter().position(|byte| *byte == 0xC3).unwrap() + 1;
        let chunks = vec![event[..split].to_vec(), event[split..].to_vec(), finished()];
        let (url, _server) = serve(SSE.into(), chunks, false).await;
        let (publisher, _tokens) = publisher(&url, json!({})).await;
        let response = answer(publisher.send(CanonicalMessage::from("hi")).await);
        assert_eq!(response.get_payload_str(), "\"ü\"");
    }

    #[tokio::test]
    async fn conflicting_fields_are_config_errors() {
        let stream = json!({"memory": {"topic": "unused"}});
        for config in [
            json!({"model": "m", "prompt": "p", "stream_to": stream, "messages_per_request": 4}),
            json!({"model": "m", "prompt": "p", "cancel_from": stream}),
            json!({"model": "m", "prompt": "p", "messages_from_payload": true}),
            json!({"model": "m", "system": "s", "messages_from_payload": true}),
            json!({"model": "m", "messages_from_payload": true, "messages_per_request": 4}),
            json!({"model": "m"}),
        ] {
            let factory = builtin_preset("openai_chat").unwrap();
            let result = factory.create_publisher("route", &config).await;
            let error = result.err().unwrap_or_else(|| panic!("accepted {config}"));
            assert!(
                error
                    .downcast_ref::<crate::errors::InvalidConfig>()
                    .is_some(),
                "{config}: {error:#}"
            );
        }
    }

    /// An `http_bulk` publisher built through the endpoint factory.
    async fn bulk(config: Value) -> anyhow::Result<std::sync::Arc<dyn MessagePublisher>> {
        let endpoint: crate::models::Endpoint =
            serde_json::from_value(json!({ "http_bulk": config })).unwrap();
        crate::endpoints::create_publisher_from_route("route", &endpoint).await
    }

    fn event(kind: &str, data: Value) -> Vec<u8> {
        format!("event: {kind}\ndata: {data}\n\n").into_bytes()
    }

    #[tokio::test]
    async fn an_anthropic_stream_is_a_recipe() {
        let chunks = vec![
            event(
                "message_start",
                json!({"type": "message_start", "message": {"id": "m"}}),
            ),
            event(
                "content_block_start",
                json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}}),
            ),
            event("ping", json!({"type": "ping"})),
            event(
                "content_block_delta",
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Hel"}}),
            ),
            event(
                "content_block_delta",
                json!({"type": "content_block_delta", "delta": {"type": "text_delta", "text": "lo"}}),
            ),
            event(
                "message_delta",
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            ),
            event("message_stop", json!({"type": "message_stop"})),
        ];
        let (url, server) = serve(SSE.into(), chunks, false).await;
        let topic = topic();
        let publisher = bulk(json!({
            "url": url,
            "headers": {"x-api-key": "k", "anthropic-version": "2023-06-01"},
            "query": {
                "path": "/v1/messages",
                "format": "json_array",
                "per_message": true,
                "request": r#"{"model": "m", "max_tokens": 64, "stream": true, "messages": [{"role": "user", "content": "${message:payload}"}]}"#,
                "value": "/delta/text",
                "stop_reason": "/delta/stop_reason",
                "error": "/error/message",
                "stream_to": {"memory": {"topic": topic}}
            }
        }))
        .await
        .unwrap();
        let tokens = get_or_create_channel(&MemoryConfig::new(topic, None));
        let response = answer(publisher.send(CanonicalMessage::from("hi")).await);
        assert_eq!(response.get_payload_str(), "\"Hello\"");
        assert_eq!(meta(&response, "mqb.stream.status"), "complete");
        assert_eq!(meta(&response, "mqb.stream.stop_reason"), "end_turn");
        let texts: Vec<_> = tokens
            .drain_messages()
            .iter()
            .map(|t| t.get_payload_str().to_string())
            .collect();
        assert_eq!(texts, ["Hel", "lo", ""]);
        assert_eq!(server.await.unwrap()["messages"][0]["content"], "hi");
    }

    #[tokio::test]
    async fn an_answer_that_is_no_event_stream_fails_for_good() {
        let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let (url, _server) = serve(head, vec![], false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::NonRetryable(_)), "{error}");
        assert!(error.to_string().contains("application/json"), "{error}");
        let tokens = tokens.drain_messages();
        assert_eq!(tokens.len(), 1);
        assert_eq!(meta(&tokens[0], "mqb.stream.status"), "error");
    }

    #[tokio::test]
    async fn an_error_event_before_any_text_fails_for_good() {
        let chunks = vec![b"data: {\"error\":{\"message\":\"bad model\"}}\n\n".to_vec()];
        let (url, _server) = serve(SSE.into(), chunks, false).await;
        let (publisher, tokens) = publisher(&url, json!({})).await;
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::NonRetryable(_)), "{error}");
        assert!(error.to_string().contains("bad model"), "{error}");
        let tokens = tokens.drain_messages();
        assert_eq!(meta(tokens.last().unwrap(), "mqb.stream.status"), "error");
    }

    /// Panics on every token but the last, which it keeps.
    struct PanicsBeforeTheEnd(std::sync::Mutex<Vec<CanonicalMessage>>);

    #[async_trait::async_trait]
    impl MessagePublisher for PanicsBeforeTheEnd {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<crate::traits::SentBatch, PublisherError> {
            for message in messages {
                assert_eq!(meta(&message, "mqb.stream.end"), "true", "a token panics");
                self.0.lock().unwrap().push(message);
            }
            Ok(crate::traits::SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn a_panic_still_ends_the_tokens_with_an_error() {
        let (url, _server) = serve(SSE.into(), vec![delta("a"), finished()], false).await;
        let config = json!({
            "url": url,
            "query": {
                "path": "/x", "format": "json_array", "request": "{}", "per_message": true,
                "value": "/choices/0/delta/content", "stream_to": {"memory": {"topic": topic()}}
            }
        });
        let config: crate::models::HttpBulkConfig = serde_json::from_value(config).unwrap();
        let mut publisher = super::StreamQueryPublisher::new("route", &config)
            .await
            .unwrap();
        let sink = std::sync::Arc::new(PanicsBeforeTheEnd(Default::default()));
        publisher.stream_to = sink.clone();
        let error = publisher
            .send(CanonicalMessage::from("hi"))
            .await
            .unwrap_err();
        assert!(matches!(error, PublisherError::NonRetryable(_)), "{error}");
        let tokens = sink.0.lock().unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(meta(&tokens[0], "mqb.stream.status"), "error");
    }

    #[tokio::test]
    async fn keepalives_and_foreign_events_are_skipped() {
        let chunks = vec![
            b": keepalive\n\ndata:\n\ndata: not json\n\n".to_vec(),
            delta("ok"),
            finished(),
        ];
        let (url, _server) = serve(SSE.into(), chunks, false).await;
        let (publisher, _tokens) = publisher(&url, json!({})).await;
        let response = answer(publisher.send(CanonicalMessage::from("hi")).await);
        assert_eq!(response.get_payload_str(), "\"ok\"");
    }

    #[tokio::test]
    async fn a_streamed_query_rejects_conflicting_fields() {
        let to = json!({"memory": {"topic": "unused"}});
        let query = |extra: Value| {
            let mut query = json!({"path": "/x", "format": "json_array", "request": "{}", "per_message": true, "value": "/v", "stream_to": to});
            for (key, value) in extra.as_object().unwrap() {
                match value {
                    Value::Null => query.as_object_mut().unwrap().remove(key),
                    value => query
                        .as_object_mut()
                        .unwrap()
                        .insert(key.clone(), value.clone()),
                };
            }
            json!({"url": "http://localhost:1", "query": query})
        };
        for config in [
            query(json!({"per_message": null})),
            query(json!({"value": null})),
            query(json!({"found": "/f"})),
            query(json!({"stream_to": null, "cancel_from": to})),
            query(json!({"stream_to": null, "stream_done": "[DONE]"})),
        ] {
            assert!(bulk(config.clone()).await.is_err(), "accepted {config}");
        }
        let config: crate::models::HttpBulkConfig =
            serde_json::from_value(query(json!({}))).unwrap();
        assert!(super::super::HttpBulkPublisher::new(&config).is_err());
    }
}
