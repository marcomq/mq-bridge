//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `llm_hosted_agent`: one request per message to an API that runs the tool and MCP loop itself.

use super::{bearer, nested, openai_url, with_tls};
use crate::endpoints::http_bulk::HttpBulkPublisher;
use crate::errors::PublisherError;
use crate::models::{LlmAnswer, LlmHostedAgentConfig, LlmHostedMcpServer, LlmHostedProvider};
use crate::outcomes::SentBatch;
use crate::traits::MessagePublisher;
use crate::CanonicalMessage;
use anyhow::{anyhow, bail};
use async_trait::async_trait;
use serde_json::{json, Map, Value};

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_MAX_TOKENS: u64 = 16000;
const PAYLOAD: &str = "{payload}";

pub(super) fn resolve(_route: &str, value: &Value) -> anyhow::Result<Value> {
    let config: LlmHostedAgentConfig = serde_json::from_value(value.clone())?;
    if config.messages_from_payload {
        if !config.prompt.is_empty() {
            bail!("'messages_from_payload' excludes 'prompt'");
        }
    } else if config.prompt.is_empty() {
        bail!("'prompt' is required unless 'messages_from_payload' is set");
    }
    let mut body = match nested("options", config.options.clone())? {
        Some(Value::Object(options)) => options,
        None => Map::new(),
        Some(_) => bail!("'options' must be a JSON object"),
    };
    let tools = match body.remove("tools") {
        Some(Value::Array(tools)) => tools,
        None => Vec::new(),
        Some(_) => bail!("'options.tools' must be a JSON array"),
    };
    let (input_field, mut headers) = match config.provider {
        LlmHostedProvider::OpenAi => openai_body(&config, &mut body, tools)?,
        LlmHostedProvider::Anthropic => anthropic_body(&config, &mut body, tools)?,
    };
    let input = match config.messages_from_payload {
        true => json!(PAYLOAD),
        false => json!([{"role": "user", "content": config.prompt}]),
    };
    body.insert(input_field.into(), input);
    // Serializing escapes the literal text; the template escapes what a message puts in.
    // The input is the last key, so a nested `options` value cannot be taken for it.
    let mut request = Value::Object(body).to_string();
    let placeholder = format!(r#""{input_field}":"{PAYLOAD}""#);
    if let Some(at) = request.rfind(&placeholder) {
        request.replace_range(
            at..at + placeholder.len(),
            &format!(r#""{input_field}":${{message:payload | json_array}}"#),
        );
    }
    let (default_url, path) = match config.provider {
        LlmHostedProvider::OpenAi => (super::OPENAI_URL, "/responses"),
        LlmHostedProvider::Anthropic => (ANTHROPIC_URL, "/messages"),
    };
    if config.provider == LlmHostedProvider::Anthropic {
        headers.extend(anthropic_headers(&config));
    }
    let resolved = json!({
        "url": openai_url("llm-hosted-agent", Some(config.url.as_deref().unwrap_or(default_url)))?,
        "headers": headers,
        "auth": nested("auth", config.auth)?,
        "request_timeout_ms": config.request_timeout_ms,
        "query": {
            "path": path,
            "format": "json_array",
            "request": request,
            "per_message": true,
            "error": "/error/message",
            "concurrency": config.concurrency.unwrap_or(8)
        }
    });
    with_tls(resolved, config.tls)
}

/// Fills the Responses API body; returns the input field and the headers.
fn openai_body(
    config: &LlmHostedAgentConfig,
    body: &mut Map<String, Value>,
    mut tools: Vec<Value>,
) -> anyhow::Result<(&'static str, Map<String, Value>)> {
    refuse(body, &["model", "input", "instructions", "stream"])?;
    if config.json && !body.contains_key("text") {
        body.insert("text".into(), json!({"format": {"type": "json_object"}}));
    }
    body.insert("model".into(), json!(config.model));
    if let Some(system) = &config.system {
        body.insert("instructions".into(), json!(system));
    }
    if let Some(max) = config.max_tokens {
        body.insert("max_output_tokens".into(), json!(max));
    }
    for server in &config.mcp_servers {
        let mut tool = json!({
            "type": "mcp",
            "server_label": server.name,
            "server_url": server.url,
            "require_approval": "never"
        });
        if let Some(token) = &server.token {
            tool["authorization"] = json!(token);
        }
        if let Some(allowed) = &server.allowed_tools {
            tool["allowed_tools"] = json!(allowed);
        }
        tools.push(tool);
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    Ok(("input", bearer(config.api_key.clone())))
}

/// Fills the Messages API body; returns the input field and the headers.
fn anthropic_body(
    config: &LlmHostedAgentConfig,
    body: &mut Map<String, Value>,
    mut tools: Vec<Value>,
) -> anyhow::Result<(&'static str, Map<String, Value>)> {
    refuse(
        body,
        &["model", "messages", "system", "stream", "mcp_servers"],
    )?;
    body.insert("model".into(), json!(config.model));
    body.insert(
        "max_tokens".into(),
        json!(config.max_tokens.unwrap_or(ANTHROPIC_MAX_TOKENS)),
    );
    if let Some(system) = &config.system {
        body.insert("system".into(), json!(system));
    }
    if !config.mcp_servers.is_empty() {
        let servers: Vec<Value> = config.mcp_servers.iter().map(mcp_server).collect();
        body.insert("mcp_servers".into(), Value::Array(servers));
        tools.extend(config.mcp_servers.iter().map(mcp_toolset));
    }
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    let mut headers = Map::new();
    if let Some(key) = &config.api_key {
        headers.insert("x-api-key".into(), json!(key));
    }
    Ok(("messages", headers))
}

fn anthropic_headers(config: &LlmHostedAgentConfig) -> Map<String, Value> {
    let mut headers = Map::new();
    headers.insert("anthropic-version".into(), json!("2023-06-01"));
    if !config.mcp_servers.is_empty() {
        headers.insert("anthropic-beta".into(), json!("mcp-client-2025-11-20"));
    }
    headers
}

fn mcp_server(server: &LlmHostedMcpServer) -> Value {
    let mut entry = json!({"type": "url", "url": server.url, "name": server.name});
    if let Some(token) = &server.token {
        entry["authorization_token"] = json!(token);
    }
    entry
}

fn mcp_toolset(server: &LlmHostedMcpServer) -> Value {
    let mut toolset = json!({"type": "mcp_toolset", "mcp_server_name": server.name});
    if let Some(allowed) = &server.allowed_tools {
        let enabled: Map<String, Value> = allowed
            .iter()
            .map(|tool| (tool.clone(), json!({"enabled": true})))
            .collect();
        toolset["default_config"] = json!({"enabled": false});
        toolset["configs"] = Value::Object(enabled);
    }
    toolset
}

fn refuse(body: &Map<String, Value>, fields: &[&str]) -> anyhow::Result<()> {
    match fields.iter().find(|field| body.contains_key(**field)) {
        Some(field) => bail!("'options' cannot set '{field}'"),
        None => Ok(()),
    }
}

/// `http_bulk` answers with the whole response; this keeps its final text, or fails the message.
pub(super) struct LlmHostedAgentPublisher {
    inner: HttpBulkPublisher,
    provider: LlmHostedProvider,
    answer: LlmAnswer,
    json: bool,
}

impl LlmHostedAgentPublisher {
    pub(super) fn new(inner: HttpBulkPublisher, value: &Value) -> anyhow::Result<Self> {
        let config: LlmHostedAgentConfig = serde_json::from_value(value.clone())?;
        Ok(Self {
            inner,
            provider: config.provider,
            answer: config.answer,
            json: config.json,
        })
    }

    fn answer(&self, response: &Value) -> anyhow::Result<Value> {
        let text = match self.provider {
            LlmHostedProvider::OpenAi => openai_text(response)?,
            LlmHostedProvider::Anthropic => anthropic_text(response)?,
        };
        match self.json {
            true => serde_json::from_str(&text)
                .map_err(|error| anyhow!("the answer is not JSON ({error}): {text}")),
            false => Ok(Value::String(text)),
        }
    }
}

/// The text of the last `message` item.
fn openai_text(response: &Value) -> anyhow::Result<String> {
    let status = response["status"].as_str().unwrap_or("completed");
    if status != "completed" {
        let reason = response["incomplete_details"]["reason"]
            .as_str()
            .unwrap_or("no reason given");
        bail!("the response is {status}: {reason}");
    }
    let output = response["output"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(request) = output
        .iter()
        .find(|item| item["type"] == "mcp_approval_request")
    {
        bail!(
            "the provider asks to approve the call of `{}`; answer `full` hands it on",
            request["name"].as_str().unwrap_or_default()
        );
    }
    if let Some(call) = output.iter().find(|item| {
        matches!(
            item["type"].as_str(),
            Some("function_call" | "custom_tool_call")
        )
    }) {
        bail!(client_tool(&call["name"]));
    }
    let message = output.iter().rev().find(|item| item["type"] == "message");
    let parts = message.and_then(|message| message["content"].as_array());
    let text: String = parts
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect();
    match text.is_empty() {
        true => bail!("the response has no text"),
        false => Ok(text),
    }
}

fn client_tool(name: &Value) -> String {
    format!(
        "the model asks to call `{}`, a tool llm_hosted_agent cannot run; answer `full` hands it on",
        name.as_str().unwrap_or_default()
    )
}

/// The text blocks after the last tool result.
fn anthropic_text(response: &Value) -> anyhow::Result<String> {
    if let Some(reason @ ("refusal" | "pause_turn" | "max_tokens")) =
        response["stop_reason"].as_str()
    {
        bail!("the response stopped with {reason}");
    }
    let content = response["content"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(call) = content.iter().find(|block| block["type"] == "tool_use") {
        bail!(client_tool(&call["name"]));
    }
    let trailing = content
        .iter()
        .rev()
        .take_while(|block| block["type"] == "text")
        .collect::<Vec<_>>();
    let text: String = trailing
        .into_iter()
        .rev()
        .filter_map(|block| block["text"].as_str())
        .collect();
    match text.is_empty() {
        true => bail!("the response has no text"),
        false => Ok(text),
    }
}

#[async_trait]
impl MessagePublisher for LlmHostedAgentPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        if self.answer == LlmAnswer::Full {
            return self.inner.send_batch(messages).await;
        }
        let originals = messages.clone();
        let (responses, refused) = match self.inner.send_batch(messages).await? {
            SentBatch::Partial {
                responses: Some(responses),
                failed,
            } => (responses, failed),
            other => return Ok(other),
        };
        // Both lists keep the input order and every message is in one of them.
        let mut skipped = refused.iter().peekable();
        let mut responses = responses.into_iter();
        let mut answers = Vec::with_capacity(responses.len());
        let mut failed = Vec::new();
        for original in originals {
            let is_refused = |(message, _): &&(CanonicalMessage, PublisherError)| {
                message.message_id == original.message_id && message.payload == original.payload
            };
            if skipped.next_if(is_refused).is_some() {
                continue;
            }
            let Some(response) = responses.next() else {
                break;
            };
            let answer = serde_json::from_slice(&response.payload)
                .map_err(anyhow::Error::from)
                .and_then(|value: Value| self.answer(&value));
            match answer {
                Ok(answer) => answers.push(CanonicalMessage::new(
                    answer.to_string().into_bytes(),
                    Some(response.message_id),
                )),
                Err(error) => failed.push((original, PublisherError::NonRetryable(error))),
            }
        }
        failed.extend(refused);
        Ok(SentBatch::Partial {
            responses: Some(answers),
            failed,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(all(test, feature = "plugin", feature = "test-utils"))]
mod tests {
    use super::*;
    use crate::plugin::test_support::StubHttpServer;

    async fn publisher(config: Value) -> Box<dyn MessagePublisher> {
        let preset = super::super::builtin("llm_hosted_agent").expect("preset");
        preset.create_publisher("route", &config).await.unwrap()
    }

    fn split(sent: SentBatch) -> (Vec<String>, Vec<String>) {
        let SentBatch::Partial { responses, failed } = sent else {
            panic!("a query answers with responses");
        };
        let answers = responses.unwrap_or_default();
        let answers = answers.iter().map(|r| r.get_payload_str().to_string());
        let failed = failed
            .iter()
            .map(|(message, error)| format!("{} {error}", message.get_payload_str()));
        (answers.collect(), failed.collect())
    }

    #[tokio::test]
    async fn openai_asks_the_responses_api_and_keeps_the_last_message() {
        let server = StubHttpServer::start(|request| {
            let body = String::from_utf8_lossy(&request.body);
            let answer = if body.contains("fail") {
                json!({"status": "failed", "error": {"message": "server down"}})
            } else if body.contains("cut") {
                json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}})
            } else if body.contains("approve") {
                json!({"status": "completed", "output": [
                    {"type": "mcp_approval_request", "name": "lookup"}
                ]})
            } else if body.contains("call") {
                json!({"status": "completed", "output": [
                    {"type": "function_call", "name": "get_weather", "arguments": "{}"}
                ]})
            } else {
                json!({"status": "completed", "output": [
                    {"type": "message", "content": [{"type": "output_text", "text": "Let me look."}]},
                    {"type": "mcp_list_tools", "server_label": "docs"},
                    {"type": "mcp_call", "name": "lookup", "output": "42"},
                    {"type": "message", "content": [
                        {"type": "output_text", "text": "It is "},
                        {"type": "output_text", "text": "42."}
                    ]}
                ]})
            };
            (200, answer.to_string())
        })
        .await
        .expect("stub server");
        let publisher = publisher(json!({
            "url": server.url(), "api_key": "k", "model": "m",
            "system": "Be brief.", "prompt": "Ask: ${payload:q}",
            "mcp_servers": [{"name": "docs", "url": "https://mcp.example", "token": "t",
                             "allowed_tools": ["lookup"]}],
            "options": {"tools": [{"type": "web_search"}]}
        }))
        .await;
        let mut rows = [
            "\"q\":\"x\"",
            "\"q\":\"fail\"",
            "\"q\":\"cut\"",
            "\"q\":\"approve\"",
            "\"q\":\"call\"",
        ]
        .map(|row| CanonicalMessage::from(format!("{{{row}}}").as_str()));
        // A shared id must not mix up which input a failure belongs to.
        rows[3].message_id = rows[2].message_id;
        let (answers, failed) = split(publisher.send_batch(rows.to_vec()).await.unwrap());
        assert_eq!(answers, [r#""It is 42.""#]);
        assert_eq!(failed.len(), 4, "{failed:?}");
        for (input, error) in [
            ("fail", "server down"),
            ("cut", "max_output_tokens"),
            ("approve", "approve the call of `lookup`"),
            (
                "call",
                "call `get_weather`, a tool llm_hosted_agent cannot run",
            ),
        ] {
            let input = format!(r#"{{"q":"{input}"}} "#);
            assert!(
                failed
                    .iter()
                    .any(|f| f.starts_with(&input) && f.contains(error)),
                "{input}{error}: {failed:?}"
            );
        }

        let request = &server.requests()[0];
        assert_eq!(request.target, "/v1/responses");
        assert_eq!(request.header("authorization"), Some("Bearer k"));
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let expected = json!({
            "model": "m",
            "instructions": "Be brief.",
            "input": [{"role": "user", "content": "Ask: x"}],
            "tools": [
                {"type": "web_search"},
                {"type": "mcp", "server_label": "docs", "server_url": "https://mcp.example",
                 "require_approval": "never", "authorization": "t", "allowed_tools": ["lookup"]}
            ]
        });
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn anthropic_sends_the_mcp_connector_and_keeps_the_trailing_text() {
        let server = StubHttpServer::start(|request| {
            let body = String::from_utf8_lossy(&request.body);
            let answer = if body.contains("refuse") {
                json!({"stop_reason": "refusal", "content": []})
            } else if body.contains("weather") {
                json!({"stop_reason": "tool_use", "content": [
                    {"type": "text", "text": "Let me check."},
                    {"type": "tool_use", "name": "get_weather", "input": {}}
                ]})
            } else {
                json!({"stop_reason": "end_turn", "content": [
                    {"type": "text", "text": "Checking."},
                    {"type": "mcp_tool_use", "name": "lookup"},
                    {"type": "mcp_tool_result", "content": [{"type": "text", "text": "42"}]},
                    {"type": "text", "text": "{\"answer\":"},
                    {"type": "text", "text": "42}"}
                ]})
            };
            (200, answer.to_string())
        })
        .await
        .expect("stub server");
        let publisher = publisher(json!({
            "provider": "anthropic", "url": server.url(), "api_key": "k", "model": "m",
            "messages_from_payload": true, "json": true,
            "mcp_servers": [{"name": "docs", "url": "https://mcp.example", "allowed_tools": ["lookup"]}]
        }))
        .await;
        let rows = [
            r#"[{"role":"user","content":"ask"}]"#.into(),
            r#"[{"role":"user","content":"refuse"}]"#.into(),
            r#"[{"role":"user","content":"weather"}]"#.into(),
        ];
        let (answers, failed) = split(publisher.send_batch(rows.to_vec()).await.unwrap());
        assert_eq!(answers, [r#"{"answer":42}"#]);
        assert_eq!(failed.len(), 2);
        assert!(failed[0].contains("refusal"), "{failed:?}");
        assert!(failed[1].contains("call `get_weather`"), "{failed:?}");

        let request = &server.requests()[0];
        assert_eq!(request.target, "/v1/messages");
        assert_eq!(request.header("x-api-key"), Some("k"));
        assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
        assert_eq!(
            request.header("anthropic-beta"),
            Some("mcp-client-2025-11-20")
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let expected = json!({
            "model": "m",
            "max_tokens": 16000,
            "messages": [{"role": "user", "content": "ask"}],
            "mcp_servers": [{"type": "url", "url": "https://mcp.example", "name": "docs"}],
            "tools": [{"type": "mcp_toolset", "mcp_server_name": "docs",
                       "default_config": {"enabled": false},
                       "configs": {"lookup": {"enabled": true}}}]
        });
        assert_eq!(body, expected);
    }

    #[tokio::test]
    async fn answer_full_hands_on_the_whole_response() {
        let response = json!({"status": "completed", "output": [
            {"type": "mcp_approval_request", "name": "lookup"}
        ]});
        let canned = response.to_string();
        let server = StubHttpServer::start(move |_| (200, canned.clone()))
            .await
            .expect("stub server");
        let publisher = publisher(json!({
            "url": server.url(), "model": "m", "prompt": "p", "answer": "full"
        }))
        .await;
        let (answers, failed) = split(publisher.send_batch(vec!["x".into()]).await.unwrap());
        assert!(failed.is_empty());
        assert_eq!(
            serde_json::from_str::<Value>(&answers[0]).unwrap(),
            response
        );
    }
}
