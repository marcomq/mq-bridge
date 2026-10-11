//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! A batch of lookups answered by one request: every message renders its part
//! of the body, and the answers are matched to the messages by position.

use super::publisher::{around, scalar_text, Failure};
use super::{quoted, Connection, JSON};
use crate::models::{HttpBulkFormat, HttpBulkQuery};
use crate::support::http_status;
use crate::support::interpolation::CompiledTemplate;
use crate::CanonicalMessage;
use anyhow::{bail, Context};
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use serde_json::Value;

const REQUESTS: &str = "{requests}";
const NDJSON: &str = "application/x-ndjson";

/// What one message's lookup found (`None` = nothing), or why it failed.
pub(super) type Answer = Result<Option<Value>, String>;

pub(super) struct Query {
    method: reqwest::Method,
    path: String,
    format: HttpBulkFormat,
    content_type: HeaderValue,
    request: CompiledTemplate,
    envelope: (String, String),
    responses: String,
    value: Option<String>,
    found: Option<String>,
    error: Option<String>,
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
            request: CompiledTemplate::compile(&config.request, Some(JSON))
                .context("Invalid http_bulk query.request template")?,
            envelope,
            responses: config.responses.clone(),
            value: config.value.clone(),
            found: config.found.clone(),
            error: config.error.clone(),
        })
    }

    /// One message's part of the body: lines for `ndjson`, one JSON value otherwise.
    fn part(&self, message: &CanonicalMessage) -> Result<Vec<u8>, String> {
        let rendered = self.request.render(Some(message));
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

    /// Answers every request, in order, with as few HTTP requests as
    /// `max_request_bytes` allows. A refused request fails them all.
    pub(super) async fn answers(
        &self,
        connection: &Connection,
        max_request_bytes: usize,
        requests: &[CanonicalMessage],
    ) -> Result<Vec<Answer>, Failure> {
        let mut answers: Vec<Answer> = requests.iter().map(|_| Ok(None)).collect();
        let mut body = Vec::new();
        let mut indices = Vec::new();
        for (index, message) in requests.iter().enumerate() {
            let part = match self.part(message) {
                Ok(part) => part,
                Err(reason) => {
                    answers[index] = Err(reason);
                    continue;
                }
            };
            if !indices.is_empty() && body.len() + part.len() + 1 > max_request_bytes {
                let sent = std::mem::take(&mut body);
                self.ask(connection, sent, &indices, &mut answers).await?;
                indices.clear();
            }
            self.append(&mut body, &part);
            indices.push(index);
        }
        if !indices.is_empty() {
            self.ask(connection, body, &indices, &mut answers).await?;
        }
        Ok(answers)
    }

    async fn ask(
        &self,
        connection: &Connection,
        mut body: Vec<u8>,
        indices: &[usize],
        answers: &mut [Answer],
    ) -> Result<(), Failure> {
        if self.format == HttpBulkFormat::JsonArray {
            body.push(b']');
            body.extend_from_slice(self.envelope.1.as_bytes());
        }
        let label = format!("{} {}", self.method, self.path);
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
        let answer: Option<Value> = serde_json::from_str(&text).ok();
        let entries = answer
            .as_ref()
            .and_then(|answer| answer.pointer(&self.responses))
            .and_then(Value::as_array);
        // Answers are matched by position, so a short array cannot be assigned.
        let Some(entries) = entries.filter(|entries| entries.len() == indices.len()) else {
            return Err(Failure::permanent(format!(
                "{label} answered no array of {} entries at '{}': {}",
                indices.len(),
                self.responses,
                quoted(&text)
            )));
        };
        for (index, entry) in indices.iter().zip(entries) {
            answers[*index] = self.answer(entry);
        }
        Ok(())
    }

    fn answer(&self, entry: &Value) -> Answer {
        let at = |pointer: &Option<String>| match pointer {
            Some(pointer) => entry.pointer(pointer).filter(|value| !value.is_null()),
            None => None,
        };
        if let Some(reason) = at(&self.error) {
            return Err(scalar_text(reason).unwrap_or_else(|| reason.to_string()));
        }
        if self.found.is_some() && at(&self.found).and_then(Value::as_bool) != Some(true) {
            return Ok(None);
        }
        let value = match &self.value {
            Some(_) => at(&self.value),
            None => Some(entry).filter(|entry| !entry.is_null()),
        };
        Ok(value.cloned())
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

        // The batch declines, so the lookup middleware asks one by one.
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
            let publisher = publisher(&server, multi_search());
            let error = publisher.lookup_batch(&lookups).await.unwrap().unwrap_err();
            let retried = matches!(error, crate::errors::PublisherError::Retryable(_));
            assert_eq!(retried, retryable, "{error}");
        }
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
        let none: HttpBulkConfig =
            serde_json::from_value(json!({"url": "http://localhost:1"})).unwrap();
        let error = HttpBulkPublisher::new(&none).map(|_| ()).unwrap_err();
        assert!(error.to_string().contains("'upsert' or 'query'"), "{error}");
    }
}
