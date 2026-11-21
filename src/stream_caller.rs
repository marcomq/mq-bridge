//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Send requests and read each answer token by token.
//!
//! A [`StreamCaller`] wraps an `openai_chat` or `http_bulk` endpoint with `stream_to`. It
//! sends a request, hands it the tokens with its correlation id from `stream_to` and ends with
//! exactly one last token, also when the request dies before the server sent one.
//!
//! ```no_run
//! use mq_bridge::models::Endpoint;
//! use mq_bridge::{CanonicalMessage, StreamCaller};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let endpoint: Endpoint = serde_json::from_value(serde_json::json!({"openai_chat": {
//!     "url": "http://localhost:11434",
//!     "model": "llama3.2",
//!     "prompt": "${message:payload}",
//!     "stream_to": {"memory": {"topic": "chat.tokens"}},
//!     "cancel_from": {"memory": {"topic": "chat.cancel"}},
//! }}))?;
//! let chat = StreamCaller::new("chat", endpoint).await?;
//! let stop = chat.cancel_handle();
//!
//! let mut reply = chat.start("turn-7", CanonicalMessage::from("Hello")).await?;
//! while let Some(token) = reply.next().await {
//!     print!("{}", token.text());
//! }
//! # drop(stop);
//! # Ok(())
//! # }
//! ```
//!
//! - One caller serves parallel requests under different correlation ids. Clone it for other
//!   tasks. Two callers must not share a `stream_to` or `cancel_from` topic: they would take
//!   each other's messages.
//! - With a `retry` middleware, a failed attempt's `error` token is not the end: the tokens of
//!   the next attempt follow under a new [`StreamToken::attempt`]. Discard the text of the
//!   earlier attempt when it changes.
//! - Dropping an unfinished [`StreamReply`] cancels its request if the endpoint has
//!   `cancel_from`.

use crate::endpoints::{create_consumer_from_route, streaming_endpoints};
use crate::models::Endpoint;
use crate::traits::{ConsumerError, MessageConsumer, MessageDisposition};
use crate::{CanonicalMessage, Publisher, Sent};
use anyhow::Context;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The id a token belongs to, the same as its request's.
pub const CORRELATION_ID: &str = "correlation_id";
/// New for each attempt of a request.
pub const STREAM_ATTEMPT: &str = "mqb.stream.attempt";
/// The token's position within its attempt, from 0.
pub const STREAM_INDEX: &str = "mqb.stream.index";
/// `true` on an attempt's last token.
pub const STREAM_END: &str = "mqb.stream.end";
/// On the last token: `complete`, `cancelled` or `error` (the payload is the error).
pub const STREAM_STATUS: &str = "mqb.stream.status";
/// On the last token, if the server gave one.
pub const STREAM_STOP_REASON: &str = "mqb.stream.stop_reason";
/// Tells a `StreamCaller` request from an earlier one that used the same correlation id.
pub(crate) const STREAM_REQUEST: &str = "mqb.stream.request";

/// How long the last token may trail the request's answer or the token before it.
const LAST_TOKEN_WAIT: Duration = Duration::from_secs(1);
const RECONNECT_MIN: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(5);

/// How an answer ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamEnd {
    Complete,
    Cancelled,
    /// The token's text is the error.
    Failed,
}

impl StreamEnd {
    fn status(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Cancelled => "cancelled",
            Self::Failed => "error",
        }
    }
}

/// One message in the `stream_to` format. Also lets a local source produce the same tokens.
#[derive(Clone, Debug)]
pub struct StreamToken(CanonicalMessage);

impl StreamToken {
    /// A token of text, not the last one.
    pub fn piece(correlation_id: &str, index: usize, text: &str) -> Self {
        Self::build(correlation_id, index, text, false)
    }

    /// The last token; `text` is empty unless `end` is [`StreamEnd::Failed`].
    pub fn last(correlation_id: &str, index: usize, end: StreamEnd, text: &str) -> Self {
        let token = Self::build(correlation_id, index, text, true);
        Self(token.0.with_metadata_kv(STREAM_STATUS, end.status()))
    }

    fn build(correlation_id: &str, index: usize, text: &str, end: bool) -> Self {
        Self(
            CanonicalMessage::from(text)
                .with_metadata_kv(CORRELATION_ID, correlation_id)
                .with_metadata_kv(STREAM_INDEX, index.to_string())
                .with_metadata_kv(STREAM_END, end.to_string()),
        )
    }

    /// Reads a message in the `stream_to` format.
    pub fn from_message(message: CanonicalMessage) -> Self {
        Self(message)
    }

    pub fn into_message(self) -> CanonicalMessage {
        self.0
    }

    pub fn correlation_id(&self) -> &str {
        self.0.get_metadata(CORRELATION_ID).unwrap_or_default()
    }

    /// Empty for a token that no endpoint attempt produced.
    pub fn attempt(&self) -> &str {
        self.0.get_metadata(STREAM_ATTEMPT).unwrap_or_default()
    }

    pub fn index(&self) -> usize {
        self.0
            .get_metadata(STREAM_INDEX)
            .and_then(|index| index.parse().ok())
            .unwrap_or(0)
    }

    pub fn text(&self) -> Cow<'_, str> {
        self.0.get_payload_str()
    }

    /// How the answer ended; `None` unless this is the last token.
    pub fn end(&self) -> Option<StreamEnd> {
        (self.0.get_metadata(STREAM_END) == Some("true")).then(|| {
            match self.0.get_metadata(STREAM_STATUS) {
                Some("complete") => StreamEnd::Complete,
                Some("cancelled") => StreamEnd::Cancelled,
                _ => StreamEnd::Failed,
            }
        })
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.0.get_metadata(STREAM_STOP_REASON)
    }
}

/// Stops a request by correlation id, from any task. Cheap to clone.
#[derive(Clone)]
pub struct StreamCancel(Publisher);

impl StreamCancel {
    /// A cancel that arrives before its request starts still stops it, within 30 s.
    pub async fn cancel(&self, correlation_id: &str) -> anyhow::Result<()> {
        let stop =
            CanonicalMessage::from(correlation_id).with_metadata_kv(CORRELATION_ID, correlation_id);
        self.0.send(stop).await.map(|_| ())
    }

    /// Stops only this request, never a later one under the same id.
    async fn cancel_request(&self, correlation_id: &str, request_id: &str) -> anyhow::Result<()> {
        let stop = CanonicalMessage::from(correlation_id)
            .with_metadata_kv(CORRELATION_ID, correlation_id)
            .with_metadata_kv(STREAM_REQUEST, request_id);
        self.0.send(stop).await.map(|_| ())
    }
}

/// What the token reader hands a reply: a token, or why no more will come.
type Delivery = Result<StreamToken, String>;

#[derive(Default)]
struct OpenReplies {
    /// By correlation id: the request's own id and where its tokens go.
    by_id: HashMap<String, (String, mpsc::UnboundedSender<Delivery>)>,
    closed: Option<String>,
}

/// The one consumer of `stream_to`; stops when the caller and its replies are gone.
struct TokenReader {
    replies: Arc<Mutex<OpenReplies>>,
    task: JoinHandle<()>,
}

impl TokenReader {
    fn spawn(name: String, tokens: Box<dyn MessageConsumer>, stream_to: Option<Endpoint>) -> Self {
        let replies = Arc::new(Mutex::new(OpenReplies::default()));
        let task = tokio::spawn(read_tokens(name, tokens, stream_to, replies.clone()));
        Self { replies, task }
    }

    fn open(&self) -> std::sync::MutexGuard<'_, OpenReplies> {
        self.replies.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for TokenReader {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_tokens(
    name: String,
    mut tokens: Box<dyn MessageConsumer>,
    stream_to: Option<Endpoint>,
    replies: Arc<Mutex<OpenReplies>>,
) {
    let mut wait = RECONNECT_MIN;
    loop {
        let received = match (tokens.receive().await, &stream_to) {
            (Ok(received), _) => {
                wait = RECONNECT_MIN;
                received
            }
            (Err(ConsumerError::Connection(error)), Some(endpoint)) => {
                tracing::warn!("StreamCaller '{name}': stream_to lost, reconnecting: {error:#}");
                tokens = loop {
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(RECONNECT_MAX);
                    match create_consumer_from_route(&name, endpoint).await {
                        Ok(consumer) => break consumer,
                        Err(error) => {
                            tracing::warn!("StreamCaller '{name}': reconnect failed: {error:#}")
                        }
                    }
                };
                continue;
            }
            (Err(error), _) => {
                let reason = format!("{error:#}");
                let mut open = replies.lock().unwrap_or_else(PoisonError::into_inner);
                for (_, (_, reply)) in open.by_id.drain() {
                    let _ = reply.send(Err(reason.clone()));
                }
                open.closed = Some(reason);
                return;
            }
        };
        let _ = (received.commit)(MessageDisposition::Ack).await;
        let token = StreamToken(received.message);
        let open = replies.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((request, reply)) = open.by_id.get(token.correlation_id()) {
            let sent_for = token.0.get_metadata(STREAM_REQUEST);
            if sent_for.is_none_or(|sent_for| sent_for == request) {
                let _ = reply.send(Ok(token));
            }
        }
    }
}

/// A request/reply client for an endpoint that streams its answer. Cheap to clone.
#[derive(Clone)]
pub struct StreamCaller {
    name: String,
    requests: Publisher,
    reader: Arc<TokenReader>,
    cancels: Option<StreamCancel>,
}

impl StreamCaller {
    /// Connects to `endpoint`, an `openai_chat` or `http_bulk` query with `stream_to`.
    pub async fn new(name: &str, endpoint: Endpoint) -> anyhow::Result<Self> {
        let [stream_to, cancel_from] = streaming_endpoints(&endpoint.endpoint_type);
        let stream_to = stream_to
            .with_context(|| format!("StreamCaller '{name}': the endpoint needs 'stream_to'"))?;
        let stream_to = stream_to.clone();
        // Subscribed first: a broker topic drops what is sent before its consumer exists.
        let tokens = create_consumer_from_route(name, &stream_to)
            .await
            .with_context(|| format!("StreamCaller '{name}': stream_to"))?;
        let cancels = match cancel_from {
            Some(cancel_from) => Some(StreamCancel(
                Publisher::new(cancel_from.clone())
                    .await
                    .with_context(|| format!("StreamCaller '{name}': cancel_from"))?,
            )),
            None => None,
        };
        let requests = Publisher::new(endpoint).await?;
        Ok(Self::connect(
            name,
            requests,
            tokens,
            cancels,
            Some(stream_to),
        ))
    }

    /// `stream_to` is what the token consumer reconnects to; `None` ends it on a lost connection.
    fn connect(
        name: &str,
        requests: Publisher,
        tokens: Box<dyn MessageConsumer>,
        cancels: Option<StreamCancel>,
        stream_to: Option<Endpoint>,
    ) -> Self {
        Self {
            name: name.to_string(),
            requests,
            reader: Arc::new(TokenReader::spawn(name.to_string(), tokens, stream_to)),
            cancels,
        }
    }

    /// `None` when the endpoint has no `cancel_from`.
    pub fn cancel_handle(&self) -> Option<StreamCancel> {
        self.cancels.clone()
    }

    /// Sends `request` under `correlation_id`; read its answer from the returned reply.
    /// Fails if a reply with this id is still open or the token consumer has stopped.
    pub async fn start(
        &self,
        correlation_id: &str,
        request: CanonicalMessage,
    ) -> anyhow::Result<StreamReply> {
        let (sender, tokens) = mpsc::unbounded_channel();
        let request_id = fast_uuid_v7::format_uuid(fast_uuid_v7::gen_id()).to_string();
        {
            // Registered before the request is sent, so that no token can miss its reply.
            let mut open = self.reader.open();
            if let Some(reason) = &open.closed {
                anyhow::bail!("StreamCaller '{}': {reason}", self.name);
            }
            if open.by_id.contains_key(correlation_id) {
                anyhow::bail!(
                    "StreamCaller '{}': a reply for '{correlation_id}' is still open",
                    self.name
                );
            }
            open.by_id
                .insert(correlation_id.to_string(), (request_id.clone(), sender));
        }
        let request = request
            .with_metadata_kv(CORRELATION_ID, correlation_id)
            .with_metadata_kv(STREAM_REQUEST, request_id.clone());
        let requests = self.requests.clone();
        Ok(StreamReply {
            answer: tokio::spawn(async move { requests.send(request).await }),
            name: self.name.clone(),
            reader: self.reader.clone(),
            tokens,
            cancels: self.cancels.clone(),
            runtime: tokio::runtime::Handle::current(),
            id: correlation_id.to_string(),
            request_id,
            next_index: 0,
            failed: None,
            answered: None,
            succeeded: false,
            ended: false,
        })
    }
}

/// The answer to one request, read with [`StreamReply::next`].
pub struct StreamReply {
    name: String,
    reader: Arc<TokenReader>,
    tokens: mpsc::UnboundedReceiver<Delivery>,
    cancels: Option<StreamCancel>,
    answer: JoinHandle<anyhow::Result<Sent>>,
    runtime: tokio::runtime::Handle,
    id: String,
    request_id: String,
    next_index: usize,
    /// An attempt's `error` token, held until the request gives up or retries.
    failed: Option<StreamToken>,
    /// The request has returned: why no last token came, and how long to wait for one.
    answered: Option<(String, Instant)>,
    /// The request succeeded, so an `error` token is from an attempt it retried.
    succeeded: bool,
    ended: bool,
}

impl StreamReply {
    /// The next token; the one with [`StreamToken::end`] set is the last, then `None`.
    pub async fn next(&mut self) -> Option<StreamToken> {
        if self.ended {
            return None;
        }
        loop {
            let deadline = self.answered.as_ref().map(|(_, deadline)| *deadline);
            tokio::select! {
                biased;
                delivery = self.tokens.recv() => match delivery {
                    Some(Ok(token)) => {
                        if let Some((_, deadline)) = &mut self.answered {
                            *deadline = Instant::now() + LAST_TOKEN_WAIT;
                        }
                        if let Some(token) = self.accept(token) {
                            return Some(token);
                        }
                    }
                    Some(Err(reason)) => return Some(self.end_failed(reason)),
                    None => return Some(self.end_failed("the token reader stopped".to_string())),
                },
                answer = &mut self.answer, if self.answered.is_none() => {
                    let reason = match answer {
                        Ok(Ok(_)) => {
                            self.failed = None;
                            self.succeeded = true;
                            "the request ended without its last token".to_string()
                        }
                        Ok(Err(error)) => format!("{error:#}"),
                        Err(error) => error.to_string(),
                    };
                    if let Some(failed) = self.failed.take() {
                        self.ended = true;
                        return Some(failed);
                    }
                    self.answered = Some((reason, Instant::now() + LAST_TOKEN_WAIT));
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)),
                    if deadline.is_some() =>
                {
                    let (reason, _) = self.answered.take().expect("the guard checked it");
                    return Some(self.end_failed(reason));
                }
            }
        }
    }

    /// The token to hand out, if any: holds a failed attempt's end.
    fn accept(&mut self, token: StreamToken) -> Option<StreamToken> {
        if self.succeeded && token.end() == Some(StreamEnd::Failed) {
            return None;
        }
        if self
            .failed
            .as_ref()
            .is_some_and(|failed| failed.attempt() != token.attempt())
        {
            self.failed = None;
        }
        self.next_index = token.index() + 1;
        match token.end() {
            Some(StreamEnd::Failed) if self.answered.is_none() => {
                self.failed = Some(token);
                None
            }
            Some(_) => {
                self.ended = true;
                Some(token)
            }
            None => Some(token),
        }
    }

    fn end_failed(&mut self, reason: String) -> StreamToken {
        self.ended = true;
        let reason = format!("StreamCaller '{}': {reason}", self.name);
        StreamToken::last(&self.id, self.next_index, StreamEnd::Failed, &reason)
    }
}

impl Drop for StreamReply {
    fn drop(&mut self) {
        self.reader.open().by_id.remove(&self.id);
        if self.ended || self.answered.is_some() || self.answer.is_finished() {
            return;
        }
        if let Some(cancels) = self.cancels.take() {
            let id = std::mem::take(&mut self.id);
            let request_id = std::mem::take(&mut self.request_id);
            self.runtime.spawn(async move {
                if let Err(error) = cancels.cancel_request(&id, &request_id).await {
                    tracing::warn!("StreamCaller: cancelling '{id}' failed: {error:#}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{MessagePublisher, PublisherError};
    use crate::SentBatch;
    use anyhow::anyhow;
    use async_trait::async_trait;

    /// Sends `script` to the token topic per request, then answers or fails.
    struct Scripted {
        tokens: Publisher,
        script: Vec<StreamToken>,
        fails: bool,
    }

    #[async_trait]
    impl MessagePublisher for Scripted {
        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            for token in &self.script {
                self.tokens
                    .send(token.clone().into_message())
                    .await
                    .unwrap();
            }
            if self.fails {
                return Err(PublisherError::NonRetryable(anyhow!("refused")));
            }
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    async fn scripted(topic: &str, script: Vec<StreamToken>, fails: bool) -> StreamCaller {
        let endpoint = Endpoint::new_memory(topic, 64);
        let tokens = Publisher::new(endpoint.clone()).await.unwrap();
        let consumer = create_consumer_from_route("test", &endpoint).await.unwrap();
        let requests = Publisher::from(Scripted {
            tokens,
            script,
            fails,
        });
        StreamCaller::connect("test", requests, consumer, None, None)
    }

    fn in_attempt(token: StreamToken, attempt: &str) -> StreamToken {
        StreamToken(token.0.with_metadata_kv(STREAM_ATTEMPT, attempt))
    }

    async fn read_all(caller: &StreamCaller) -> Vec<StreamToken> {
        read(caller, "t").await
    }

    async fn read(caller: &StreamCaller, id: &str) -> Vec<StreamToken> {
        let mut reply = caller.start(id, CanonicalMessage::from("q")).await.unwrap();
        let mut tokens = Vec::new();
        while let Some(token) = reply.next().await {
            tokens.push(token);
        }
        tokens
    }

    #[tokio::test]
    async fn reads_its_own_tokens_up_to_the_last() {
        let script = vec![
            StreamToken::piece("earlier", 0, "stale"),
            StreamToken::piece("t", 0, "Hel"),
            StreamToken::piece("t", 1, "lo"),
            StreamToken::last("t", 2, StreamEnd::Complete, ""),
        ];
        let caller = scripted("stream_caller.own", script, false).await;
        let tokens = read_all(&caller).await;
        let texts: Vec<_> = tokens.iter().map(|t| t.text().into_owned()).collect();
        assert_eq!(texts, ["Hel", "lo", ""]);
        assert_eq!(tokens[2].end(), Some(StreamEnd::Complete));
    }

    #[tokio::test]
    async fn a_retried_attempt_does_not_end_the_reply() {
        let script = vec![
            in_attempt(StreamToken::piece("t", 0, "par"), "a"),
            in_attempt(StreamToken::last("t", 1, StreamEnd::Failed, "cut off"), "a"),
            in_attempt(StreamToken::piece("t", 0, "whole"), "b"),
            in_attempt(StreamToken::last("t", 1, StreamEnd::Complete, ""), "b"),
        ];
        let caller = scripted("stream_caller.retry", script, false).await;
        let tokens = read_all(&caller).await;
        let seen: Vec<_> = tokens
            .iter()
            .map(|t| (t.attempt().to_string(), t.text().into_owned()))
            .collect();
        let expected = [("a", "par"), ("b", "whole"), ("b", "")];
        assert_eq!(seen, expected.map(|(a, t)| (a.to_string(), t.to_string())));
        assert_eq!(tokens[2].end(), Some(StreamEnd::Complete));
    }

    #[tokio::test]
    async fn a_refused_request_ends_with_the_servers_error() {
        let script = vec![StreamToken::last("t", 0, StreamEnd::Failed, "401")];
        let caller = scripted("stream_caller.refused", script, true).await;
        let tokens = read_all(&caller).await;
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].end(), Some(StreamEnd::Failed));
        assert_eq!(tokens[0].text(), "401");
    }

    #[tokio::test]
    async fn a_request_that_dies_without_a_last_token_still_ends() {
        let script = vec![StreamToken::piece("t", 0, "par")];
        let caller = scripted("stream_caller.dies", script, true).await;
        let tokens = read_all(&caller).await;
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].end(), Some(StreamEnd::Failed));
        assert_eq!(tokens[1].index(), 1);
        assert!(tokens[1].text().contains("refused"), "{}", tokens[1].text());
    }

    #[tokio::test]
    async fn dropping_an_unfinished_reply_cancels_it() {
        let cancels = Endpoint::new_memory("stream_caller.drop.cancel", 8);
        let mut listener = create_consumer_from_route("test", &cancels).await.unwrap();
        let mut caller = scripted("stream_caller.drop", Vec::new(), false).await;
        caller.cancels = Some(StreamCancel(Publisher::new(cancels).await.unwrap()));
        drop(
            caller
                .start("t", CanonicalMessage::from("q"))
                .await
                .unwrap(),
        );
        let stop = tokio::time::timeout(Duration::from_secs(2), listener.receive())
            .await
            .expect("a cancel")
            .unwrap();
        assert_eq!(stop.message.get_metadata(CORRELATION_ID), Some("t"));
    }

    /// Answers each request with its own numbered tokens, a yield between them.
    struct Echo(Publisher);

    #[async_trait]
    impl MessagePublisher for Echo {
        async fn send_batch(
            &self,
            messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            for message in messages {
                let id = message.get_metadata(CORRELATION_ID).unwrap().to_string();
                for index in 0..30 {
                    let piece = StreamToken::piece(&id, index, &format!("{id}{index}"));
                    self.0.send(piece.into_message()).await.unwrap();
                    tokio::task::yield_now().await;
                }
                let last = StreamToken::last(&id, 30, StreamEnd::Complete, "");
                self.0.send(last.into_message()).await.unwrap();
            }
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn parallel_requests_each_get_all_of_their_tokens() {
        let endpoint = Endpoint::new_memory("stream_caller.parallel", 64);
        let tokens = Publisher::new(endpoint.clone()).await.unwrap();
        let consumer = create_consumer_from_route("test", &endpoint).await.unwrap();
        let caller =
            StreamCaller::connect("test", Publisher::from(Echo(tokens)), consumer, None, None);
        let (a, b) = tokio::join!(read(&caller, "a"), read(&caller, "b"));
        for (id, reply) in [("a", a), ("b", b)] {
            assert_eq!(reply.len(), 31, "{id}");
            for (index, token) in reply[..30].iter().enumerate() {
                assert_eq!(token.text(), format!("{id}{index}"));
            }
            assert_eq!(reply[30].end(), Some(StreamEnd::Complete));
        }
    }

    /// Sends `now` before it answers and `later` after it, one per `gap`.
    struct Trailing {
        tokens: Publisher,
        now: Vec<StreamToken>,
        later: Vec<StreamToken>,
        gap: Duration,
    }

    #[async_trait]
    impl MessagePublisher for Trailing {
        async fn send_batch(
            &self,
            _messages: Vec<CanonicalMessage>,
        ) -> Result<SentBatch, PublisherError> {
            for token in &self.now {
                self.tokens
                    .send(token.clone().into_message())
                    .await
                    .unwrap();
            }
            let (tokens, later, gap) = (self.tokens.clone(), self.later.clone(), self.gap);
            tokio::spawn(async move {
                for token in later {
                    tokio::time::sleep(gap).await;
                    tokens.send(token.into_message()).await.unwrap();
                }
            });
            Ok(SentBatch::Ack)
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    async fn trailing(
        topic: &str,
        now: Vec<StreamToken>,
        later: Vec<StreamToken>,
        gap: Duration,
    ) -> StreamCaller {
        let endpoint = Endpoint::new_memory(topic, 64);
        let tokens = Publisher::new(endpoint.clone()).await.unwrap();
        let consumer = create_consumer_from_route("test", &endpoint).await.unwrap();
        let requests = Publisher::from(Trailing {
            tokens,
            now,
            later,
            gap,
        });
        StreamCaller::connect("test", requests, consumer, None, None)
    }

    #[tokio::test]
    async fn tokens_of_an_earlier_request_with_the_id_are_ignored() {
        let stale = |token: StreamToken| StreamToken(token.0.with_metadata_kv(STREAM_REQUEST, "x"));
        let script = vec![
            stale(StreamToken::piece("t", 0, "old")),
            stale(StreamToken::last("t", 1, StreamEnd::Cancelled, "")),
            StreamToken::piece("t", 0, "new"),
            StreamToken::last("t", 1, StreamEnd::Complete, ""),
        ];
        let caller = scripted("stream_caller.stale", script, false).await;
        let tokens = read_all(&caller).await;
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0].text(), "new");
        assert_eq!(tokens[1].end(), Some(StreamEnd::Complete));
    }

    /// Loses its connection on the first read.
    struct Disconnected;

    #[async_trait]
    impl MessageConsumer for Disconnected {
        async fn receive_batch(
            &mut self,
            _max_messages: usize,
        ) -> Result<crate::traits::ReceivedBatch, ConsumerError> {
            Err(ConsumerError::Connection(anyhow!("connection lost")))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[tokio::test]
    async fn a_lost_token_connection_is_reconnected() {
        let endpoint = Endpoint::new_memory("stream_caller.reconnect", 8);
        let tokens = Publisher::new(endpoint.clone()).await.unwrap();
        let script = vec![
            StreamToken::piece("t", 0, "back"),
            StreamToken::last("t", 1, StreamEnd::Complete, ""),
        ];
        let requests = Publisher::from(Scripted {
            tokens,
            script,
            fails: false,
        });
        let caller = StreamCaller::connect(
            "test",
            requests,
            Box::new(Disconnected),
            None,
            Some(endpoint),
        );
        let tokens = read_all(&caller).await;
        assert_eq!(tokens[0].text(), "back");
        assert_eq!(tokens[1].end(), Some(StreamEnd::Complete));
    }

    #[tokio::test]
    async fn a_retry_that_succeeds_after_the_answer_is_not_a_failure() {
        let now = vec![in_attempt(
            StreamToken::last("t", 0, StreamEnd::Failed, "cut off"),
            "a",
        )];
        let later = vec![
            in_attempt(StreamToken::piece("t", 0, "whole"), "b"),
            in_attempt(StreamToken::last("t", 1, StreamEnd::Complete, ""), "b"),
        ];
        let gap = Duration::from_millis(50);
        let caller = trailing("stream_caller.late_retry", now, later, gap).await;
        let tokens = read_all(&caller).await;
        assert_eq!(tokens[0].text(), "whole");
        assert_eq!(tokens.last().unwrap().end(), Some(StreamEnd::Complete));
    }

    #[tokio::test]
    async fn tokens_that_keep_arriving_extend_the_wait_for_the_last() {
        let mut later: Vec<_> = (0..4)
            .map(|index| StreamToken::piece("t", index, "x"))
            .collect();
        later.push(StreamToken::last("t", 4, StreamEnd::Complete, ""));
        let gap = Duration::from_millis(400);
        let caller = trailing("stream_caller.lagging", Vec::new(), later, gap).await;
        let tokens = read_all(&caller).await;
        assert_eq!(tokens.len(), 5);
        assert_eq!(tokens[4].end(), Some(StreamEnd::Complete));
    }

    #[tokio::test]
    async fn an_id_with_an_open_reply_is_refused() {
        let caller = scripted("stream_caller.twice", Vec::new(), false).await;
        let open = caller
            .start("t", CanonicalMessage::from("q"))
            .await
            .unwrap();
        let second = caller.start("t", CanonicalMessage::from("q")).await;
        assert!(second.err().unwrap().to_string().contains("still open"));
        drop(open);
        assert!(caller.start("t", CanonicalMessage::from("q")).await.is_ok());
    }

    #[tokio::test]
    async fn an_endpoint_without_stream_to_is_refused() {
        let error = StreamCaller::new("test", Endpoint::null()).await.err();
        assert!(error.expect("refused").to_string().contains("stream_to"));
    }

    #[cfg(feature = "http-bulk")]
    #[tokio::test]
    async fn streams_an_openai_chat_answer() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0; 4096];
            while !complete(&request) {
                let read = socket.read(&mut buf).unwrap();
                request.extend_from_slice(&buf[..read]);
            }
            let events = "data: {\"choices\":[{\"delta\":{\"content\":\"Hal\"}}]}\n\n\
                 data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":\"stop\"}]}\n\n\
                 data: [DONE]\n\n";
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close";
            write!(socket, "{head}\r\n\r\n{events}").unwrap();
        });
        let endpoint = serde_json::from_value(serde_json::json!({"openai_chat": {
            "url": url,
            "model": "m",
            "prompt": "${message:payload}",
            "stream_to": {"memory": {"topic": "stream_caller.chat"}},
        }}))
        .unwrap();
        let caller = StreamCaller::new("test", endpoint).await.unwrap();
        let tokens = read_all(&caller).await;
        let text: String = tokens.iter().map(|t| t.text().into_owned()).collect();
        assert_eq!(text, "Hallo");
        let last = tokens.last().unwrap();
        assert_eq!(last.end(), Some(StreamEnd::Complete));
        assert_eq!(last.stop_reason(), Some("stop"));
    }

    /// Headers and the whole body, by `content-length`.
    #[cfg(feature = "http-bulk")]
    fn complete(request: &[u8]) -> bool {
        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
            return false;
        };
        let length = String::from_utf8_lossy(&request[..end])
            .lines()
            .find_map(|line| {
                let line = line.to_lowercase();
                line.strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        request.len() >= end + 4 + length
    }
}
