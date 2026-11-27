//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! `llm_agent`: mq-bridge runs the tool loop. Each round is one request to the
//! model; the tools it calls are endpoints or MCP servers that mq-bridge calls itself.

mod adapters;
mod tools;

use super::{bearer, nested, openai_url, with_tls};
use crate::CanonicalMessage;
use crate::endpoints::http_bulk::HttpBulkPublisher;
use crate::errors::{InvalidConfig, PublisherError};
use crate::models::{HttpBulkConfig, LlmAgentConfig, LlmAnswer, LlmProvider};
use crate::outcomes::{Sent, SentBatch};
use crate::support::interpolation::CompiledTemplate;
use crate::traits::{CustomEndpointFactory, MessageConsumer, MessagePublisher};
use adapters::{ToolResult, Usage};
use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use serde_json::{Map, Value, json};
use tools::Tools;

const DEFAULT_MAX_ROUNDS: u32 = 8;
const DEFAULT_TOOL_CONCURRENCY: usize = 4;
const DEFAULT_CONCURRENCY: usize = 8;

#[derive(Debug)]
pub(super) struct LlmAgentFactory;

#[async_trait]
impl CustomEndpointFactory for LlmAgentFactory {
    async fn create_consumer(
        &self,
        _route_name: &str,
        _config: &Value,
    ) -> anyhow::Result<Box<dyn MessageConsumer>> {
        Err(InvalidConfig(anyhow!("llm_agent is an output only")).into())
    }

    async fn create_publisher(
        &self,
        route_name: &str,
        config: &Value,
    ) -> anyhow::Result<Box<dyn MessagePublisher>> {
        Ok(Box::new(LlmAgentPublisher::new(route_name, config).await?))
    }

    fn config_schema(&self) -> Option<Value> {
        super::derived::<LlmAgentConfig>().map(|schema| schema())
    }
}

pub(super) struct LlmAgentPublisher {
    inner: HttpBulkPublisher,
    provider: LlmProvider,
    /// The request fields every round shares: model, limits, options.
    shared: Map<String, Value>,
    system: Option<CompiledTemplate>,
    /// `None` with `messages_from_payload`.
    prompt: Option<CompiledTemplate>,
    tools: Tools,
    definitions: Vec<Value>,
    max_rounds: u32,
    tool_concurrency: usize,
    concurrency: usize,
    answer: LlmAnswer,
    json: bool,
    /// The endpoint set the `json_object` format, so the system prompt mentions JSON.
    json_hint: bool,
}

/// The parts of the publisher that come from the config alone.
struct Checked {
    inner: HttpBulkPublisher,
    shared: Map<String, Value>,
    system: Option<CompiledTemplate>,
    prompt: Option<CompiledTemplate>,
    /// Tools from `options.tools`, sent as they are, e.g. a provider's web search.
    extra_tools: Vec<Value>,
    json_hint: bool,
}

impl LlmAgentPublisher {
    async fn new(route_name: &str, value: &Value) -> anyhow::Result<Self> {
        let config: LlmAgentConfig = serde_json::from_value(value.clone())
            .context("llm_agent")
            .map_err(InvalidConfig)?;
        let checked = check(&config).map_err(InvalidConfig)?;
        let tools = Tools::new(route_name, &config.tools, &config.mcp_servers).await?;
        let mut definitions = adapters::tool_definitions(config.provider, &tools.specs);
        definitions.extend(checked.extra_tools);
        Ok(Self {
            inner: checked.inner,
            provider: config.provider,
            shared: checked.shared,
            system: checked.system,
            prompt: checked.prompt,
            tools,
            definitions,
            max_rounds: config.max_rounds.unwrap_or(DEFAULT_MAX_ROUNDS),
            tool_concurrency: config
                .tool_concurrency
                .unwrap_or(DEFAULT_TOOL_CONCURRENCY)
                .max(1),
            concurrency: config.concurrency.unwrap_or(DEFAULT_CONCURRENCY).max(1),
            answer: config.answer,
            json: config.json,
            json_hint: checked.json_hint,
        })
    }

    async fn converse(
        &self,
        message: &CanonicalMessage,
    ) -> Result<CanonicalMessage, PublisherError> {
        let fail = PublisherError::NonRetryable;
        let mut system = self.system.as_ref().map(|system| render(system, message));
        if self.json_hint {
            system = adapters::with_json_hint(system);
        }
        let mut history = match &self.prompt {
            Some(prompt) => vec![adapters::user(render(prompt, message))],
            None => match serde_json::from_slice(&message.payload) {
                Ok(Value::Array(history)) => history,
                _ => return Err(fail(anyhow!("the payload is not a JSON array of messages"))),
            },
        };
        let mut usage = Usage::default();
        let mut called = Vec::new();
        for round in 1..=self.max_rounds {
            let body = adapters::request(
                self.provider,
                &self.shared,
                system.as_deref(),
                &history,
                &self.definitions,
            );
            let request = CanonicalMessage::new(body.to_string().into_bytes(), None);
            let response = match self.inner.send(request).await? {
                Sent::Response(response) => response,
                Sent::Ack => return Err(fail(anyhow!("the model API gave no response"))),
            };
            let response: Value = serde_json::from_slice(&response.payload)
                .map_err(|error| fail(anyhow!("the response is not JSON: {error}")))?;
            let turn = adapters::parse(self.provider, response).map_err(fail)?;
            usage.input += turn.usage.input;
            usage.output += turn.usage.output;
            history.extend(turn.assistant);
            if turn.calls.is_empty() {
                return self
                    .answer(message, turn.text, history, round, &usage)
                    .map_err(fail);
            }
            called = turn.calls.iter().map(|call| call.name.clone()).collect();
            // No round is left to hand their results to.
            if round == self.max_rounds {
                break;
            }
            let results: Vec<ToolResult> = futures::stream::iter(turn.calls)
                .map(|call| self.tools.call(call))
                .buffered(self.tool_concurrency)
                .try_collect()
                .await?;
            history.extend(adapters::results(self.provider, results));
        }
        Err(fail(anyhow!(
            "no answer after {} rounds (max_rounds); the last round called `{}`",
            self.max_rounds,
            called.join("`, `")
        )))
    }

    fn answer(
        &self,
        original: &CanonicalMessage,
        text: String,
        history: Vec<Value>,
        rounds: u32,
        usage: &Usage,
    ) -> anyhow::Result<CanonicalMessage> {
        let payload = match self.answer {
            LlmAnswer::Full => Value::Array(history),
            LlmAnswer::Text if text.is_empty() => bail!("the response has no text"),
            LlmAnswer::Text if self.json => serde_json::from_str(&text)
                .map_err(|error| anyhow!("the answer is not JSON ({error}): {text}"))?,
            LlmAnswer::Text => Value::String(text),
        };
        tracing::debug!(
            rounds,
            input_tokens = usage.input,
            output_tokens = usage.output,
            "llm_agent answered"
        );
        Ok(
            CanonicalMessage::new(payload.to_string().into_bytes(), Some(original.message_id))
                .with_metadata_kv("llm_rounds", rounds.to_string())
                .with_metadata_kv("llm_input_tokens", usage.input.to_string())
                .with_metadata_kv("llm_output_tokens", usage.output.to_string()),
        )
    }
}

fn render(template: &CompiledTemplate, message: &CanonicalMessage) -> String {
    String::from_utf8_lossy(&template.render(Some(message))).into_owned()
}

fn check(config: &LlmAgentConfig) -> anyhow::Result<Checked> {
    let prompt = match (config.messages_from_payload, config.prompt.is_empty()) {
        (true, false) => bail!("'messages_from_payload' excludes 'prompt'"),
        (false, true) => bail!("'prompt' is required unless 'messages_from_payload' is set"),
        (true, true) => None,
        (false, false) => {
            Some(CompiledTemplate::compile(&config.prompt, None).context("'prompt'")?)
        }
    };
    let system = match &config.system {
        Some(system) => Some(CompiledTemplate::compile(system, None).context("'system'")?),
        None => None,
    };
    if config.max_rounds == Some(0) {
        bail!("'max_rounds' must be at least 1");
    }
    let provider = config.provider;
    let mut shared = match nested("options", config.options.clone())? {
        Some(Value::Object(options)) => options,
        None => Map::new(),
        Some(_) => bail!("'options' must be a JSON object"),
    };
    if let Some(field) = adapters::own_fields(provider)
        .iter()
        .find(|field| shared.contains_key(**field))
    {
        bail!("'options' cannot set '{field}'");
    }
    let extra_tools = match shared.remove("tools") {
        Some(Value::Array(tools)) => tools,
        None => Vec::new(),
        Some(_) => bail!("'options.tools' must be a JSON array"),
    };
    let json_hint = adapters::prepare(
        provider,
        &mut shared,
        &config.model,
        config.max_tokens,
        config.json,
    );
    let (default_url, headers) = match provider {
        LlmProvider::Anthropic => {
            let mut headers = Map::new();
            if let Some(key) = &config.api_key {
                headers.insert("x-api-key".into(), json!(key));
            }
            headers.insert("anthropic-version".into(), json!("2023-06-01"));
            (adapters::ANTHROPIC_URL, headers)
        }
        LlmProvider::OpenAi | LlmProvider::OpenAiChat => {
            (super::OPENAI_URL, bearer(config.api_key.clone()))
        }
    };
    let resolved = json!({
        "url": openai_url("llm-agent", Some(config.url.as_deref().unwrap_or(default_url)))?,
        "headers": headers,
        "auth": nested("auth", config.auth.clone())?,
        "request_timeout_ms": config.request_timeout_ms,
        "query": {
            "path": adapters::path(provider),
            "format": "json_array",
            "request": "${message:payload | raw}",
            "per_message": true,
            "error": "/error/message",
            "concurrency": 1
        }
    });
    let resolved: HttpBulkConfig =
        serde_json::from_value(with_tls(resolved, config.tls.clone())?).context("llm_agent")?;
    Ok(Checked {
        inner: HttpBulkPublisher::new(&resolved)?,
        shared,
        system,
        prompt,
        extra_tools,
        json_hint,
    })
}

#[async_trait]
impl MessagePublisher for LlmAgentPublisher {
    async fn send_batch(
        &self,
        messages: Vec<CanonicalMessage>,
    ) -> Result<SentBatch, PublisherError> {
        let outcomes: Vec<_> = futures::stream::iter(messages)
            .map(|message| async move {
                let outcome = self.converse(&message).await;
                (message, outcome)
            })
            .buffered(self.concurrency)
            .collect()
            .await;
        let mut answers = Vec::with_capacity(outcomes.len());
        let mut failed = Vec::new();
        for (message, outcome) in outcomes {
            match outcome {
                Ok(answer) => answers.push(answer),
                Err(error) => failed.push((message, error)),
            }
        }
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

    async fn publisher(config: Value) -> anyhow::Result<Box<dyn MessagePublisher>> {
        LlmAgentFactory.create_publisher("route", &config).await
    }

    fn body(request: &crate::plugin::test_support::StubRequest) -> Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    fn split(sent: SentBatch) -> (Vec<CanonicalMessage>, Vec<String>) {
        let SentBatch::Partial { responses, failed } = sent else {
            panic!("llm_agent answers with responses");
        };
        let failed = failed
            .iter()
            .map(|(message, error)| format!("{} {error}", message.get_payload_str()));
        (responses.unwrap_or_default(), failed.collect())
    }

    fn meta(message: &CanonicalMessage, key: &str) -> String {
        message
            .get_metadata(key)
            .map(|v| v.to_string())
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn chat_runs_parallel_calls_and_hands_back_every_result() {
        let server = StubHttpServer::start(|request| {
            let messages = body(request)["messages"].clone();
            let answer = if messages.as_array().unwrap().iter().any(|m| m["role"] == "tool") {
                json!({"choices": [{"finish_reason": "stop",
                        "message": {"role": "assistant", "content": "It is sunny."}}],
                       "usage": {"prompt_tokens": 20, "completion_tokens": 5}})
            } else {
                json!({"choices": [{"finish_reason": "tool_calls", "message": {
                    "role": "assistant", "content": null, "reasoning_content": "thinking",
                    "tool_calls": [
                        {"id": "a", "type": "function",
                         "function": {"name": "weather", "arguments": "{\"city\":\"Bonn\"}"}},
                        {"id": "b", "type": "function",
                         "function": {"name": "missing", "arguments": "{}"}},
                        {"type": "function", "function": {"name": "weather", "arguments": "no json"}}
                    ]}}],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 3}})
            };
            (200, answer.to_string())
        })
        .await
        .expect("stub server");
        let publisher = publisher(json!({
            "provider": "openai_chat", "url": server.url(), "api_key": "k", "model": "m",
            "system": "Be brief.", "prompt": "Weather in ${payload:city}?",
            "tools": [{"name": "weather", "description": "Weather of a city",
                       "schema": {"type": "object"}, "endpoint": {"static": "sunny"}}]
        }))
        .await
        .unwrap();
        let sent = publisher
            .send_batch(vec![r#"{"city":"Bonn"}"#.into()])
            .await;
        let (answers, failed) = split(sent.unwrap());
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(answers[0].get_payload_str(), r#""It is sunny.""#);
        assert_eq!(meta(&answers[0], "llm_rounds"), "2");
        assert_eq!(meta(&answers[0], "llm_input_tokens"), "30");
        assert_eq!(meta(&answers[0], "llm_output_tokens"), "8");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].target, "/v1/chat/completions");
        assert_eq!(requests[0].header("authorization"), Some("Bearer k"));
        let first = body(&requests[0]);
        assert_eq!(
            first["tools"],
            json!([{"type": "function", "function": {"name": "weather",
                    "description": "Weather of a city", "parameters": {"type": "object"}}}])
        );
        let messages = body(&requests[1])["messages"].clone();
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(
            messages[1],
            json!({"role": "user", "content": "Weather in Bonn?"})
        );
        assert_eq!(messages[2]["reasoning_content"], "thinking");
        assert_eq!(messages[2]["tool_calls"][2]["id"], "call_2");
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "a", "content": "sunny"})
        );
        assert_eq!(messages[4]["tool_call_id"], "b");
        assert!(
            messages[4]["content"]
                .as_str()
                .unwrap()
                .contains("no tool `missing`")
        );
        assert_eq!(messages[5]["tool_call_id"], "call_2");
        assert!(
            messages[5]["content"]
                .as_str()
                .unwrap()
                .contains("not JSON")
        );
    }

    #[tokio::test]
    async fn anthropic_sends_thinking_back_unchanged_and_marks_errors() {
        let server = StubHttpServer::start(|request| {
            let text = String::from_utf8_lossy(&request.body);
            let answer = if text.contains("tool_result") {
                json!({"stop_reason": "end_turn", "content": [
                    {"type": "text", "text": "{\"ok\":"}, {"type": "text", "text": "true}"}
                ]})
            } else if text.contains("refuse") {
                json!({"stop_reason": "refusal", "content": []})
            } else {
                json!({"stop_reason": "tool_use", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "id": "t1", "name": "nope", "input": {}}
                ]})
            };
            (200, answer.to_string())
        })
        .await
        .expect("stub server");
        let publisher = publisher(json!({
            "provider": "anthropic", "url": server.url(), "api_key": "k", "model": "m",
            "system": "Answer in JSON.", "messages_from_payload": true, "json": true
        }))
        .await
        .unwrap();
        let rows = vec![
            r#"[{"role":"user","content":"ask"}]"#.into(),
            r#"[{"role":"user","content":"refuse"}]"#.into(),
        ];
        let (answers, failed) = split(publisher.send_batch(rows).await.unwrap());
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].get_payload_str(), r#"{"ok":true}"#);
        assert_eq!(failed.len(), 1);
        assert!(failed[0].contains("refusal"), "{failed:?}");

        let requests = server.requests();
        let second = requests
            .iter()
            .find(|r| String::from_utf8_lossy(&r.body).contains("tool_result"))
            .expect("a second round");
        assert_eq!(second.target, "/v1/messages");
        assert_eq!(second.header("x-api-key"), Some("k"));
        assert_eq!(second.header("anthropic-version"), Some("2023-06-01"));
        let body = body(second);
        assert_eq!(body["system"], "Answer in JSON.");
        assert_eq!(body["max_tokens"], 16000);
        assert_eq!(
            body["messages"][1],
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                {"type": "tool_use", "id": "t1", "name": "nope", "input": {}}
            ]})
        );
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1",
                   "content": "there is no tool `nope`", "is_error": true}]})
        );
    }

    #[tokio::test]
    async fn responses_stops_after_max_rounds() {
        let server = StubHttpServer::start(|_| {
            let answer = json!({"status": "completed", "output": [
                {"type": "reasoning", "id": "r1", "summary": []},
                {"type": "function_call", "call_id": "c1", "name": "weather", "arguments": "{}"}
            ]});
            (200, answer.to_string())
        })
        .await
        .expect("stub server");
        let publisher = publisher(json!({
            "url": server.url(), "model": "m", "system": "s", "prompt": "p", "max_rounds": 2,
            "options": {"tools": [{"type": "web_search"}]},
            "tools": [{"name": "weather", "endpoint": {"static": "sunny"}}]
        }))
        .await
        .unwrap();
        let (answers, failed) = split(publisher.send_batch(vec!["x".into()]).await.unwrap());
        assert!(answers.is_empty());
        assert!(failed[0].contains("no answer after 2 rounds"), "{failed:?}");
        assert!(failed[0].contains("`weather`"), "{failed:?}");

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].target, "/v1/responses");
        let second = body(&requests[1]);
        assert_eq!(second["instructions"], "s");
        assert_eq!(
            second["tools"],
            json!([{"type": "function", "name": "weather", "description": "",
                    "parameters": {"type": "object"}}, {"type": "web_search"}])
        );
        assert_eq!(
            second["input"],
            json!([
                {"role": "user", "content": "p"},
                {"type": "reasoning", "id": "r1", "summary": []},
                {"type": "function_call", "call_id": "c1", "name": "weather", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "c1", "output": "sunny"}
            ])
        );
    }

    #[tokio::test]
    async fn a_config_mistake_is_refused_at_startup() {
        let tool = json!({"name": "a", "endpoint": {"static": "x"}});
        for (config, error) in [
            (json!({"model": "m"}), "'prompt' is required"),
            (
                json!({"model": "m", "prompt": "p", "messages_from_payload": true}),
                "excludes 'prompt'",
            ),
            (
                json!({"model": "m", "prompt": "p", "options": {"model": "x"}}),
                "cannot set 'model'",
            ),
            (
                json!({"model": "m", "prompt": "p", "max_rounds": 0}),
                "at least 1",
            ),
            (
                json!({"model": "m", "prompt": "p", "tools": [tool.clone(), tool]}),
                "two tools are named `a`",
            ),
            (
                json!({"model": "m", "prompt": "p", "mcp_servers": [{"name": "x"}]}),
                "needs either 'url' or 'command'",
            ),
        ] {
            let refused = publisher(config).await.err().expect("refused");
            assert!(
                format!("{refused:#}").contains(error),
                "{error}: {refused:#}"
            );
        }
    }

    /// Answers the MCP requests of a Streamable HTTP client with plain JSON.
    fn mcp_answer(request: &crate::plugin::test_support::StubRequest) -> (u16, String) {
        if request.method != "POST" {
            return (405, String::new());
        }
        let call = body(request);
        let result = match call["method"].as_str().unwrap_or_default() {
            "initialize" => json!({
                "protocolVersion": call["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "stub", "version": "1"}
            }),
            "tools/list" => json!({"tools": [
                {"name": "search", "description": "Searches the docs",
                 "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}},
                {"name": "hidden", "inputSchema": {"type": "object"}}
            ]}),
            "tools/call" => json!({
                "content": [{"type": "text", "text": format!("found {}", call["params"]["arguments"]["q"])}],
                "isError": false
            }),
            _ => return (202, String::new()),
        };
        let answer = json!({"jsonrpc": "2.0", "id": call["id"], "result": result});
        (200, answer.to_string())
    }

    #[tokio::test]
    async fn mcp_tools_are_listed_filtered_and_called() {
        let mcp = StubHttpServer::start(mcp_answer).await.expect("mcp stub");
        let model = StubHttpServer::start(|request| {
            let messages = body(request)["messages"].clone();
            let answer = match messages
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool")
            {
                Some(result) => json!({"choices": [{"message": {
                    "role": "assistant", "content": result["content"]}}]}),
                None => json!({"choices": [{"message": {"role": "assistant", "tool_calls": [
                    {"id": "a", "type": "function",
                     "function": {"name": "search", "arguments": "{\"q\":\"rust\"}"}}
                ]}}]}),
            };
            (200, answer.to_string())
        })
        .await
        .expect("model stub");
        let publisher = publisher(json!({
            "provider": "openai_chat", "url": model.url(), "model": "m", "prompt": "p",
            "mcp_servers": [{"name": "docs", "url": format!("{}/mcp", mcp.url()),
                             "token": "t", "allowed_tools": ["search"]}]
        }))
        .await
        .unwrap();
        let (answers, failed) = split(publisher.send_batch(vec!["x".into()]).await.unwrap());
        assert!(failed.is_empty(), "{failed:?}");
        assert_eq!(answers[0].get_payload_str(), r#""found \"rust\"""#);

        let first = body(&model.requests()[0]);
        assert_eq!(
            first["tools"],
            json!([{"type": "function", "function": {"name": "search",
                    "description": "Searches the docs",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}}}])
        );
        let called = mcp
            .requests()
            .into_iter()
            .find(|r| String::from_utf8_lossy(&r.body).contains("tools/call"))
            .expect("a tool call");
        assert_eq!(called.header("authorization"), Some("Bearer t"));
        assert_eq!(body(&called)["params"]["arguments"], json!({"q": "rust"}));
    }

    #[tokio::test]
    async fn an_allowed_tool_the_server_lacks_is_refused() {
        let mcp = StubHttpServer::start(mcp_answer).await.expect("mcp stub");
        let refused = publisher(json!({
            "model": "m", "prompt": "p",
            "mcp_servers": [{"name": "docs", "url": mcp.url(), "allowed_tools": ["gone"]}]
        }))
        .await
        .err()
        .expect("refused");
        assert!(
            format!("{refused:#}").contains("has no tool `gone`"),
            "{refused:#}"
        );
    }
}
