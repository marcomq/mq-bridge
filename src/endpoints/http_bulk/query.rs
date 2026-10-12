//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! A batch of lookups answered by one request: every message renders its part
//! of the body, and the answers are matched to the messages by position. With
//! `per_message` each message is a request of its own.

use super::publisher::{around, scalar_text, Failure};
use super::{quoted, Connection, JSON};
use crate::models::{HttpBulkFormat, HttpBulkQuery};
use crate::support::http_status;
use crate::support::interpolation::CompiledTemplate;
use crate::CanonicalMessage;
use anyhow::{bail, Context};
use futures::StreamExt;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use serde_json::Value;

const REQUESTS: &str = "{requests}";
const INDEX: &str = "{index}";
const NDJSON: &str = "application/x-ndjson";
const DEFAULT_CONCURRENCY: usize = 8;

/// What one message's lookup found (`None` = nothing), or why it failed.
pub(super) type Answer = Result<Option<Value>, Failure>;

/// A request that gave no answers, and whether one with fewer messages may.
struct Refused {
    failure: Failure,
    narrows: bool,
}

pub(super) struct Query {
    method: reqwest::Method,
    path: String,
    format: HttpBulkFormat,
    content_type: HeaderValue,
    request: CompiledTemplate,
    /// The rest of the request after `{index}`, when the template numbers its messages.
    numbered: Option<CompiledTemplate>,
    envelope: (String, String),
    responses: String,
    responses_text: Option<String>,
    max_messages: usize,
    index: Option<String>,
    stop_reason: Option<String>,
    split_on_reject: bool,
    value: Option<String>,
    found: Option<String>,
    error: Option<String>,
    per_message: bool,
    parse_value: bool,
    concurrency: usize,
}

impl Query {
    pub(super) fn new(config: &HttpBulkQuery) -> anyhow::Result<Self> {
        if !config.path.starts_with('/') {
            bail!("http_bulk query.path must start with '/'");
        }
        let method = config.method.as_deref().unwrap_or("POST");
        let method = method.to_ascii_uppercase();
        let ndjson = config.format == HttpBulkFormat::Ndjson;
        let envelope = match &config.envelope {
            Some(_) if ndjson => bail!("http_bulk query.envelope needs format 'json_array'"),
            Some(envelope) => around(envelope, REQUESTS, "query.envelope")?,
            None => Default::default(),
        };
        if config.per_message && (config.envelope.is_some() || !config.responses.is_empty()) {
            bail!("http_bulk query.per_message excludes 'envelope' and 'responses'");
        }
        let batched_only = config.responses_text.is_some()
            || config.max_messages.is_some()
            || config.index.is_some()
            || config.split_on_reject;
        if config.per_message && batched_only {
            bail!(
                "http_bulk query.per_message excludes 'responses_text', 'max_messages', \
                 'index' and 'split_on_reject'"
            );
        }
        let concurrency = match config.concurrency {
            Some(0) => bail!("http_bulk query.concurrency must be at least 1"),
            Some(concurrency) => concurrency,
            None if config.per_message => DEFAULT_CONCURRENCY,
            None => 1,
        };
        if config.max_messages == Some(0) {
            bail!("http_bulk query.max_messages must be at least 1");
        }
        let compile = |template: &str| {
            CompiledTemplate::compile(template, Some(JSON))
                .context("Invalid http_bulk query.request template")
        };
        let (request, numbered) = match config.request.split_once(INDEX) {
            Some((_, after)) if after.contains(INDEX) => {
                bail!("http_bulk query.request may contain '{INDEX}' only once")
            }
            Some((before, after)) => (before, Some(compile(after)?)),
            None => (config.request.as_str(), None),
        };
        let content_type =
            config
                .content_type
                .as_deref()
                .unwrap_or(if ndjson { NDJSON } else { JSON });
        Ok(Self {
            method: reqwest::Method::from_bytes(method.as_bytes())
                .with_context(|| format!("http_bulk query.method '{method}' is not valid"))?,
            path: config.path.clone(),
            format: config.format,
            content_type: HeaderValue::from_str(content_type)
                .context("http_bulk query.content_type is not a valid header value")?,
            request: compile(request)?,
            numbered,
            envelope,
            responses: config.responses.clone(),
            responses_text: config.responses_text.clone(),
            max_messages: config.max_messages.unwrap_or(usize::MAX),
            index: config.index.clone(),
            stop_reason: config.stop_reason.clone(),
            split_on_reject: config.split_on_reject,
            value: config.value.clone(),
            found: config.found.clone(),
            error: config.error.clone(),
            per_message: config.per_message,
            parse_value: config.parse_value,
            concurrency,
        })
    }

    pub(super) fn per_message(&self) -> bool {
        self.per_message
    }

    /// One message's part of the body: lines for `ndjson`, one JSON value otherwise.
    /// `number` is its place in the request, from 1.
    fn part(&self, message: &CanonicalMessage, number: usize) -> Result<Vec<u8>, String> {
        let mut rendered = self.request.render(Some(message)).to_vec();
        if let Some(rest) = &self.numbered {
            rendered.extend_from_slice(number.to_string().as_bytes());
            rendered.extend_from_slice(&rest.render(Some(message)));
        }
        let part = rendered.trim_ascii();
        if part.is_empty() {
            return Err("the rendered query request is empty".to_string());
        }
        if self.format == HttpBulkFormat::JsonArray {
            serde_json::from_slice::<serde::de::IgnoredAny>(part).map_err(|error| {
                format!("the rendered query request is not valid JSON: {error}")
            })?;
        }
        Ok(part.to_vec())
    }

    fn append(&self, body: &mut Vec<u8>, part: &[u8]) {
        match self.format {
            HttpBulkFormat::Ndjson => {
                body.extend_from_slice(part);
                body.push(b'\n');
            }
            HttpBulkFormat::JsonArray => {
                if body.is_empty() {
                    body.extend_from_slice(self.envelope.0.as_bytes());
                    body.push(b'[');
                } else {
                    body.push(b',');
                }
                body.extend_from_slice(part);
            }
        }
    }

    /// Answers every request, in order, with as few HTTP requests as `max_messages`
    /// and `max_request_bytes` allow. A refused request fails its own messages and those
    /// of the requests after it, and is returned for a caller that fails the whole batch.
    pub(super) async fn answers(
        &self,
        connection: &Connection,
        max_request_bytes: usize,
        requests: &[CanonicalMessage],
    ) -> (Vec<Answer>, Option<Failure>) {
        if self.per_message {
            return (self.each(connection, requests).await, None);
        }
        let mut answers: Vec<Answer> = requests.iter().map(|_| Ok(None)).collect();
        let mut bodies = Vec::new();
        let mut groups: Vec<Vec<usize>> = Vec::new();
        let mut body = Vec::new();
        let mut indices = Vec::new();
        for (index, message) in requests.iter().enumerate() {
            let mut part = self.part(message, indices.len() + 1);
            let size = part.as_ref().map_or(0, Vec::len);
            let full = indices.len() >= self.max_messages
                || (!indices.is_empty() && body.len() + size + 1 > max_request_bytes);
            if full {
                bodies.push(std::mem::take(&mut body));
                groups.push(std::mem::take(&mut indices));
                if self.numbered.is_some() {
                    part = self.part(message, 1);
                }
            }
            match part {
                Ok(part) => {
                    self.append(&mut body, &part);
                    indices.push(index);
                }
                Err(reason) => answers[index] = Err(Failure::permanent(reason)),
            }
        }
        if !indices.is_empty() {
            bodies.push(body);
            groups.push(indices);
        }
        // Collected first: a closure passed to `map` is not general enough over the borrow.
        let calls: Vec<_> = bodies
            .into_iter()
            .zip(&groups)
            .map(|(body, indices)| self.ask(connection, body, indices, requests))
            .collect();
        let mut asked = futures::stream::iter(calls).buffered(self.concurrency);
        let mut refused: Option<Failure> = None;
        for indices in &groups {
            // The requests after a refused one fail with it; those not started are not sent.
            let found = match &refused {
                Some(failure) => Err(failure.clone()),
                None => match asked.next().await {
                    Some(found) => found,
                    None => break,
                },
            };
            match found {
                Ok(found) => {
                    for (index, answer) in indices.iter().zip(found) {
                        answers[*index] = answer;
                    }
                }
                Err(failure) => {
                    for index in indices {
                        answers[*index] = Err(failure.clone());
                    }
                    refused.get_or_insert(failure);
                }
            }
        }
        (answers, refused)
    }

    /// The answers of one request, in the order of `indices`.
    async fn ask(
        &self,
        connection: &Connection,
        mut body: Vec<u8>,
        indices: &[usize],
        requests: &[CanonicalMessage],
    ) -> Result<Vec<Answer>, Failure> {
        self.close(&mut body);
        match self.matched(connection, body, indices.len()).await {
            Ok(answers) => Ok(answers),
            Err(refused) if self.split_on_reject && refused.narrows => {
                tracing::warn!(
                    messages = indices.len(),
                    reason = %refused.failure.text,
                    "http_bulk query: asking again with fewer messages per request"
                );
                Ok(self
                    .halves(connection, indices, requests, refused.failure)
                    .await)
            }
            Err(refused) => Err(refused.failure),
        }
    }

    /// Asks again in halves until the messages that cannot be answered stand alone.
    async fn halves(
        &self,
        connection: &Connection,
        indices: &[usize],
        requests: &[CanonicalMessage],
        failure: Failure,
    ) -> Vec<Answer> {
        if indices.len() == 1 {
            return vec![Err(failure)];
        }
        let mut answers: Vec<Answer> = indices.iter().map(|_| Ok(None)).collect();
        let middle = indices.len() / 2;
        let mut pending = vec![middle..indices.len(), 0..middle];
        while let Some(range) = pending.pop() {
            let mut body = Vec::new();
            for (place, index) in indices[range.clone()].iter().enumerate() {
                // The same messages were rendered for the request that is split.
                if let Ok(part) = self.part(&requests[*index], place + 1) {
                    self.append(&mut body, &part);
                }
            }
            self.close(&mut body);
            match self.matched(connection, body, range.len()).await {
                Ok(found) => {
                    for (slot, answer) in answers[range].iter_mut().zip(found) {
                        *slot = answer;
                    }
                }
                Err(refused) if refused.narrows && range.len() > 1 => {
                    let middle = range.start + range.len() / 2;
                    pending.push(middle..range.end);
                    pending.push(range.start..middle);
                }
                Err(refused) => {
                    for slot in &mut answers[range] {
                        *slot = Err(refused.failure.clone());
                    }
                }
            }
        }
        answers
    }

    /// Sends one body and matches its `count` answers to the messages.
    async fn matched(
        &self,
        connection: &Connection,
        body: Vec<u8>,
        count: usize,
    ) -> Result<Vec<Answer>, Refused> {
        let text = self
            .post(connection, body)
            .await
            .map_err(|failure| Refused {
                narrows: failure.refused_content(),
                failure,
            })?;
        let entries = self.entries(&text, count).map_err(|failure| Refused {
            narrows: true,
            failure,
        })?;
        Ok(entries.iter().map(|entry| self.answer(entry)).collect())
    }

    fn close(&self, body: &mut Vec<u8>) {
        if self.format == HttpBulkFormat::JsonArray {
            body.push(b']');
            body.extend_from_slice(self.envelope.1.as_bytes());
        }
    }

    /// The `count` answers of a response, in the order of the messages: by `index` when
    /// the entries carry one, else by position.
    fn entries(&self, text: &str, count: usize) -> Result<Vec<Value>, Failure> {
        let outer: Option<Value> = serde_json::from_str(text).ok();
        let mut answer = match &self.responses_text {
            Some(pointer) => outer
                .as_ref()
                .and_then(|outer| outer.pointer(pointer)?.as_str())
                .and_then(|inner| serde_json::from_str(inner).ok()),
            None => outer.clone(),
        };
        let entries = answer
            .as_mut()
            .and_then(|answer| answer.pointer_mut(&self.responses))
            .and_then(Value::as_array_mut)
            .filter(|entries| entries.len() == count)
            .map(std::mem::take)
            .and_then(|entries| self.ordered(entries));
        entries.ok_or_else(|| {
            let stop = self.stop_reason.as_ref().and_then(|pointer| {
                let reason = outer.as_ref()?.pointer(pointer)?.as_str()?;
                Some(format!(" (the response ended with '{reason}')"))
            });
            let numbered = match self.index {
                Some(_) => format!(" numbered 1 to {count}"),
                None => String::new(),
            };
            Failure::permanent(format!(
                "{} answered no array of {count} entries{numbered} at '{}'{}: {}",
                self.label(),
                self.responses,
                stop.unwrap_or_default(),
                quoted(text)
            ))
        })
    }

    /// The entries by their `index`; `None` when a number is missing, repeated or unknown.
    fn ordered(&self, entries: Vec<Value>) -> Option<Vec<Value>> {
        let Some(pointer) = &self.index else {
            return Some(entries);
        };
        let mut ordered: Vec<Option<Value>> = entries.iter().map(|_| None).collect();
        for entry in entries {
            let number = match entry.pointer(pointer)? {
                Value::Number(number) => number.as_u64()?,
                Value::String(number) => number.trim().parse().ok()?,
                _ => return None,
            };
            let slot = ordered.get_mut(usize::try_from(number).ok()?.checked_sub(1)?)?;
            if slot.replace(entry).is_some() {
                return None;
            }
        }
        ordered.into_iter().collect()
    }

    /// One request per message, `concurrency` at a time; a refused one fails its message alone.
    async fn each(&self, connection: &Connection, requests: &[CanonicalMessage]) -> Vec<Answer> {
        // Collected first: a closure passed to `map` is not general enough over the borrow.
        let calls: Vec<_> = requests
            .iter()
            .map(|message| self.one(connection, message))
            .collect();
        futures::stream::iter(calls)
            .buffered(self.concurrency)
            .collect()
            .await
    }

    async fn one(&self, connection: &Connection, message: &CanonicalMessage) -> Answer {
        let part = self.part(message, 1).map_err(Failure::permanent)?;
        let text = self.post(connection, part).await?;
        let answer: Value = serde_json::from_str(&text).map_err(|_| {
            Failure::permanent(format!(
                "{} answered no JSON: {}",
                self.label(),
                quoted(&text)
            ))
        })?;
        self.answer(&answer)
    }

    fn label(&self) -> String {
        format!("{} {}", self.method, self.path)
    }

    /// Sends one body and returns the text of a 2xx response.
    async fn post(&self, connection: &Connection, body: Vec<u8>) -> Result<String, Failure> {
        let label = self.label();
        let url = format!("{}{}", connection.base, self.path);
        let request = connection
            .request(self.method.clone(), &url)
            .header(CONTENT_TYPE, self.content_type.clone())
            .body(body);
        let response = connection.send(request).await.map_err(|e| Failure {
            retryable: e.retryable,
            status: None,
            text: format!("{label} failed: {e}"),
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|e| Failure {
            retryable: true,
            status: None,
            text: format!("{label} response was cut off: {e}"),
        })?;
        if !status.is_success() {
            return Err(Failure {
                retryable: http_status::is_retryable(status.as_u16()),
                status: Some(status.as_u16()),
                text: format!("{label} answered {status}: {}", quoted(&text)),
            });
        }
        Ok(text)
    }

    fn answer(&self, entry: &Value) -> Answer {
        let at = |pointer: &Option<String>| match pointer {
            Some(pointer) => entry.pointer(pointer).filter(|value| !value.is_null()),
            None => None,
        };
        if let Some(reason) = at(&self.error) {
            let reason = scalar_text(reason).unwrap_or_else(|| reason.to_string());
            return Err(Failure::permanent(reason));
        }
        if self.found.is_some() && at(&self.found).and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let value = match &self.value {
            Some(_) => at(&self.value),
            None => Some(entry).filter(|entry| !entry.is_null()),
        };
        match value {
            Some(Value::String(text)) if self.parse_value => {
                serde_json::from_str(text).map(Some).map_err(|error| {
                    Failure::permanent(format!("the value is not JSON ({error}): {}", quoted(text)))
                })
            }
            value => Ok(value.cloned()),
        }
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use crate::endpoints::http_bulk::HttpBulkPublisher;
    use crate::models::HttpBulkConfig;
    use crate::plugin::test_support::{StubHttpServer, StubRequest};
    use crate::traits::MessagePublisher;
    use crate::CanonicalMessage;
    use serde_json::{json, Value};

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

    fn requests(payloads: &[&str]) -> Vec<CanonicalMessage> {
        payloads
            .iter()
            .map(|p| CanonicalMessage::from(*p))
            .collect()
    }

    fn bodies(server: &StubHttpServer) -> Vec<(String, String)> {
        server
            .requests()
            .into_iter()
            .map(|r| (r.target, String::from_utf8(r.body).unwrap()))
            .collect()
    }

    fn multi_search() -> Value {
        json!({"query": {
            "path": "/books/_msearch",
            "request": "{}\n{\"query\":{\"term\":{\"isbn\":\"${payload:isbn}\"}},\"size\":1}",
            "responses": "/responses",
            "value": "/hits/hits/0/_source",
            "error": "/error/reason"
        }})
    }

    /// Answers every search of a multi-search body: `bad` errors, `none` finds nothing.
    fn multi_search_answer(request: &StubRequest) -> (u16, String) {
        let body = String::from_utf8_lossy(&request.body);
        let searches = body.lines().skip(1).step_by(2);
        let responses: Vec<Value> = searches
            .map(|search| match search {
                s if s.contains("bad") => json!({"error": {"reason": "boom"}, "status": 400}),
                s if s.contains("none") => json!({"hits": {"hits": []}}),
                _ => json!({"hits": {"hits": [{"_source": {"title": "Dune"}}]}}),
            })
            .collect();
        (200, json!({"responses": responses}).to_string())
    }

    #[tokio::test]
    async fn a_multi_search_sends_a_header_and_a_body_line_per_lookup() {
        let server = server(multi_search_answer).await;
        let publisher = publisher(&server, multi_search());
        let answers = publisher
            .lookup_batch(&requests(&[r#"{"isbn":"a"}"#, r#"{"isbn":"none"}"#]))
            .await
            .expect("a query endpoint answers batches")
            .unwrap();

        assert_eq!(answers, vec![Some(json!({"title": "Dune"})), None]);
        let sent = server.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].header("content-type"), Some("application/x-ndjson"));
        assert_eq!(
            bodies(&server),
            vec![(
                "/books/_msearch".to_string(),
                concat!(
                    "{}\n{\"query\":{\"term\":{\"isbn\":\"a\"}},\"size\":1}\n",
                    "{}\n{\"query\":{\"term\":{\"isbn\":\"none\"}},\"size\":1}\n",
                )
                .to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_multi_get_checks_the_found_flag() {
        let answer = r#"{"docs":[{"found":true,"_source":{"title":"Dune"}},{"found":false},{"found":true}]}"#;
        let server = server(move |_| (200, answer.to_string())).await;
        let publisher = publisher(
            &server,
            json!({"query": {
                "path": "/books/_mget",
                "format": "json_array",
                "request": "{\"_id\":\"${payload:isbn}\"}",
                "envelope": "{\"docs\":{requests}}",
                "responses": "/docs",
                "value": "/_source",
                "found": "/found"
            }}),
        );
        let lookups = requests(&[r#"{"isbn":"a"}"#, r#"{"isbn":"b"}"#, r#"{"isbn":"c"}"#]);
        let answers = publisher.lookup_batch(&lookups).await.unwrap().unwrap();

        assert_eq!(answers, vec![Some(json!({"title": "Dune"})), None, None]);
        assert_eq!(
            bodies(&server),
            vec![(
                "/books/_mget".to_string(),
                r#"{"docs":[{"_id":"a"},{"_id":"b"},{"_id":"c"}]}"#.to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_batch_search_wraps_the_requests_and_a_missing_answer_is_refused() {
        let answer = r#"{"result":[[{"id":7,"score":0.9}]],"status":"ok"}"#;
        let server = server(move |request| {
            let full = r#"{"result":[[{"id":7,"score":0.9}],[]],"status":"ok"}"#;
            let short = request.body.windows(3).any(|part| part == b"0.9");
            (200, if short { answer } else { full }.to_string())
        })
        .await;
        let publisher = publisher(
            &server,
            json!({"query": {
                "path": "/collections/books/points/search/batch",
                "format": "json_array",
                "request": "{\"vector\":${payload:vector | raw},\"limit\":1}",
                "envelope": "{\"searches\":{requests}}",
                "responses": "/result"
            }}),
        );
        let lookups = requests(&[r#"{"vector":[0.1,0.2]}"#, r#"{"vector":[0.3,0.4]}"#]);
        let answers = publisher.lookup_batch(&lookups).await.unwrap().unwrap();

        assert_eq!(
            answers,
            vec![Some(json!([{"id": 7, "score": 0.9}])), Some(json!([]))]
        );
        assert_eq!(
            bodies(&server),
            vec![(
                "/collections/books/points/search/batch".to_string(),
                r#"{"searches":[{"vector":[0.1,0.2],"limit":1},{"vector":[0.3,0.4],"limit":1}]}"#
                    .to_string()
            )]
        );

        // One answer for two lookups cannot be matched by position.
        let lookups = requests(&[r#"{"vector":[0.9,0.2]}"#, r#"{"vector":[0.3,0.4]}"#]);
        let error = publisher.lookup_batch(&lookups).await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            crate::errors::PublisherError::NonRetryable(_)
        ));
        assert!(error.to_string().contains("2 entries"), "{error}");
    }

    #[tokio::test]
    async fn an_answer_that_reports_an_error_fails_only_its_own_lookup() {
        let server = server(multi_search_answer).await;
        let publisher = publisher(&server, multi_search());
        let lookups = requests(&[r#"{"isbn":"a"}"#, r#"{"isbn":"bad"}"#]);

        let each = publisher.lookup_each(&lookups).await.unwrap();
        assert_eq!(each[0].as_ref().unwrap(), &Some(json!({"title": "Dune"})));
        let refused = each[1].as_ref().unwrap_err();
        assert!(refused.to_string().contains("boom"), "{refused}");
        assert_eq!(server.requests().len(), 1);
        // As a whole the batch declines; a caller of `lookup_batch` asks one by one.
        assert!(publisher.lookup_batch(&lookups).await.is_none());
        let mut lookups = lookups.into_iter();
        let found = publisher.send(lookups.next().unwrap()).await.unwrap();
        let crate::outcomes::Sent::Response(found) = found else {
            panic!("a query answers with a response");
        };
        assert_eq!(found.payload.as_ref(), br#"{"title":"Dune"}"#);
        let refused = publisher.send(lookups.next().unwrap()).await.unwrap_err();
        assert!(refused.to_string().contains("boom"), "{refused}");
    }

    #[tokio::test]
    async fn a_large_batch_is_split_and_a_refused_request_keeps_its_status() {
        let server = server(multi_search_answer).await;
        let mut config = multi_search();
        config["max_request_bytes"] = json!(60);
        let split = publisher(&server, config);
        let lookups = requests(&[r#"{"isbn":"a"}"#, r#"{"isbn":"none"}"#, r#"{"isbn":"c"}"#]);
        let answers = split.lookup_batch(&lookups).await.unwrap().unwrap();
        assert_eq!(answers.iter().filter(|a| a.is_some()).count(), 2);
        assert_eq!(server.requests().len(), 3);

        for (status, retryable) in [(503, true), (400, false)] {
            let server = self::server(move |_| (status, "no".to_string())).await;
            let mut config = multi_search();
            config["max_request_bytes"] = json!(60);
            let publisher = publisher(&server, config);
            let error = publisher.lookup_batch(&lookups).await.unwrap().unwrap_err();
            let retried = matches!(error, crate::errors::PublisherError::Retryable(_));
            assert_eq!(retried, retryable, "{error}");
            // Nothing is sent after the request that was refused.
            assert_eq!(server.requests().len(), 1);
        }
    }

    #[tokio::test]
    async fn a_refused_request_keeps_the_answers_before_it() {
        let server = server(
            |request| match request.body.windows(4).any(|p| p == b"none") {
                true => (503, "no".to_string()),
                false => multi_search_answer(request),
            },
        )
        .await;
        let mut config = multi_search();
        config["max_request_bytes"] = json!(60);
        let publisher = publisher(&server, config);
        let lookups = requests(&[r#"{"isbn":"a"}"#, r#"{"isbn":"none"}"#, r#"{"isbn":"c"}"#]);

        let each = publisher.lookup_each(&lookups).await.unwrap();
        assert_eq!(each[0].as_ref().unwrap(), &Some(json!({"title": "Dune"})));
        for refused in &each[1..] {
            let refused = refused.as_ref().unwrap_err();
            assert!(
                matches!(refused, crate::errors::PublisherError::Retryable(_)),
                "{refused}"
            );
        }
        // The third request is not sent.
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn a_query_needs_no_upsert_and_excludes_writes() {
        let config = |extra: Value| {
            let mut config = multi_search();
            config["url"] = json!("http://localhost:1");
            config
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            let config: HttpBulkConfig = serde_json::from_value(config).unwrap();
            HttpBulkPublisher::new(&config).map(|_| ())
        };
        config(json!({})).unwrap();
        let error = config(json!({"upsert": {"path": "/_bulk"}})).unwrap_err();
        assert!(error.to_string().contains("excludes 'upsert'"), "{error}");
        for (extra, reason) in [
            (
                json!({"per_message": true}),
                "excludes 'envelope' and 'responses'",
            ),
            (
                json!({"per_message": true, "envelope": null, "responses": "", "max_messages": 2}),
                "'index' and 'split_on_reject'",
            ),
        ] {
            let mut query = multi_search();
            query["url"] = json!("http://localhost:1");
            let fields = query["query"].as_object_mut().unwrap();
            fields.extend(extra.as_object().unwrap().clone());
            let query: HttpBulkConfig = serde_json::from_value(query).unwrap();
            let error = HttpBulkPublisher::new(&query).map(|_| ()).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
        }
        let none: HttpBulkConfig =
            serde_json::from_value(json!({"url": "http://localhost:1"})).unwrap();
        let error = HttpBulkPublisher::new(&none).map(|_| ()).unwrap_err();
        assert!(error.to_string().contains("'upsert' or 'query'"), "{error}");
    }
}
