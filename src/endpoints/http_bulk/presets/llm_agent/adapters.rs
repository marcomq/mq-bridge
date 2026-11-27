//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The request and response shapes of each provider. The history stays in the
//! provider's own format, so reasoning items, thinking blocks and signatures
//! go back unchanged; only the tool calls are read into one shape.

use crate::models::LlmProvider;
use anyhow::bail;
use serde_json::{Map, Value, json};
use std::collections::HashSet;

pub(super) const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1";
const ANTHROPIC_MAX_TOKENS: u64 = 16000;

pub(super) struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}

pub(super) struct ToolCall {
    pub call_id: String,
    pub name: String,
    /// The error text when the model sent arguments that are not a JSON object.
    pub arguments: Result<Value, String>,
}

pub(super) struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Default)]
pub(super) struct Usage {
    pub input: u64,
    pub output: u64,
}

/// One model answer: what goes into the history, its text and the tools it calls.
pub(super) struct Turn {
    pub assistant: Vec<Value>,
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Usage,
}

pub(super) fn path(provider: LlmProvider) -> &'static str {
    match provider {
        LlmProvider::OpenAi => "/responses",
        LlmProvider::Anthropic => "/messages",
        LlmProvider::OpenAiChat => "/chat/completions",
    }
}

/// The request fields the endpoint sets itself.
pub(super) fn own_fields(provider: LlmProvider) -> &'static [&'static str] {
    match provider {
        LlmProvider::OpenAi => &["model", "input", "instructions", "stream"],
        LlmProvider::Anthropic => &["model", "messages", "system", "stream"],
        LlmProvider::OpenAiChat => &["model", "messages", "stream"],
    }
}

/// Puts the fields every request of the endpoint shares into `body`. True when it set
/// the `json_object` format, which needs [`with_json_hint`] on the system prompt.
pub(super) fn prepare(
    provider: LlmProvider,
    body: &mut Map<String, Value>,
    model: &str,
    max_tokens: Option<u64>,
    json: bool,
) -> bool {
    body.insert("model".into(), json!(model));
    let (format_field, format, max_field) = match provider {
        LlmProvider::OpenAi => (
            "text",
            json!({"format": {"type": "json_object"}}),
            "max_output_tokens",
        ),
        LlmProvider::Anthropic => {
            if max_tokens.is_some() || !body.contains_key("max_tokens") {
                let max = max_tokens.unwrap_or(ANTHROPIC_MAX_TOKENS);
                body.insert("max_tokens".into(), json!(max));
            }
            return false;
        }
        LlmProvider::OpenAiChat => (
            "response_format",
            json!({"type": "json_object"}),
            "max_tokens",
        ),
    };
    if let Some(max) = max_tokens {
        body.insert(max_field.into(), json!(max));
    }
    let hint = json && !body.contains_key(format_field);
    if hint {
        body.insert(format_field.into(), format);
    }
    hint
}

/// `system` with the mention of JSON that the `json_object` format requires.
pub(super) fn with_json_hint(system: Option<String>) -> Option<String> {
    let hint = super::super::JSON_HINT;
    Some(match system {
        Some(system) => format!("{system}\n{hint}"),
        None => hint.to_string(),
    })
}

/// The tool definitions as the provider expects them.
pub(super) fn tool_definitions(provider: LlmProvider, specs: &[ToolSpec]) -> Vec<Value> {
    let define = |spec: &ToolSpec| match provider {
        LlmProvider::OpenAi => json!({
            "type": "function", "name": spec.name,
            "description": spec.description, "parameters": spec.schema
        }),
        LlmProvider::Anthropic => json!({
            "name": spec.name, "description": spec.description, "input_schema": spec.schema
        }),
        LlmProvider::OpenAiChat => json!({
            "type": "function",
            "function": {"name": spec.name, "description": spec.description, "parameters": spec.schema}
        }),
    };
    specs.iter().map(define).collect()
}

pub(super) fn user(text: String) -> Value {
    json!({"role": "user", "content": text})
}

pub(super) fn request(
    provider: LlmProvider,
    shared: &Map<String, Value>,
    system: Option<&str>,
    history: &[Value],
    tools: &[Value],
) -> Value {
    let mut body = shared.clone();
    let history = match (provider, system) {
        (LlmProvider::OpenAi, Some(system)) => {
            body.insert("instructions".into(), json!(system));
            history.to_vec()
        }
        (LlmProvider::Anthropic, Some(system)) => {
            body.insert("system".into(), json!(system));
            history.to_vec()
        }
        (LlmProvider::OpenAiChat, Some(system)) => {
            let mut messages = vec![json!({"role": "system", "content": system})];
            messages.extend_from_slice(history);
            messages
        }
        (_, None) => history.to_vec(),
    };
    let field = match provider {
        LlmProvider::OpenAi => "input",
        LlmProvider::Anthropic | LlmProvider::OpenAiChat => "messages",
    };
    body.insert(field.into(), Value::Array(history));
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools.to_vec()));
    }
    Value::Object(body)
}

pub(super) fn parse(provider: LlmProvider, response: Value) -> anyhow::Result<Turn> {
    match provider {
        LlmProvider::OpenAi => parse_responses(response),
        LlmProvider::Anthropic => parse_anthropic(response),
        LlmProvider::OpenAiChat => parse_chat(response),
    }
}

/// The history entries that hand the tool results back.
pub(super) fn results(provider: LlmProvider, results: Vec<ToolResult>) -> Vec<Value> {
    match provider {
        LlmProvider::OpenAi => results
            .into_iter()
            .map(|r| json!({"type": "function_call_output", "call_id": r.call_id, "output": r.content}))
            .collect(),
        LlmProvider::Anthropic => {
            let blocks: Vec<Value> = results
                .into_iter()
                .map(|r| {
                    let mut block = json!({
                        "type": "tool_result", "tool_use_id": r.call_id, "content": r.content
                    });
                    if r.is_error {
                        block["is_error"] = json!(true);
                    }
                    block
                })
                .collect();
            vec![json!({"role": "user", "content": blocks})]
        }
        LlmProvider::OpenAiChat => results
            .into_iter()
            .map(|r| json!({"role": "tool", "tool_call_id": r.call_id, "content": r.content}))
            .collect(),
    }
}

fn tokens(usage: &Value, input: &str, output: &str) -> Usage {
    Usage {
        input: usage[input].as_u64().unwrap_or_default(),
        output: usage[output].as_u64().unwrap_or_default(),
    }
}

/// Arguments as JSON text (OpenAI) or as an object (Ollama's own form).
fn arguments(name: &str, value: &Value) -> Result<Value, String> {
    let parsed = match value {
        Value::String(text) if text.trim().is_empty() => Ok(json!({})),
        Value::String(text) => serde_json::from_str(text).map_err(|error| error.to_string()),
        Value::Null => Ok(json!({})),
        other => Ok(other.clone()),
    };
    match parsed {
        Ok(object @ Value::Object(_)) => Ok(object),
        Ok(_) => Err(format!("the arguments of `{name}` are not a JSON object")),
        Err(error) => Err(format!("the arguments of `{name}` are not JSON: {error}")),
    }
}

fn parse_responses(response: Value) -> anyhow::Result<Turn> {
    let status = response["status"].as_str().unwrap_or("completed");
    if status != "completed" {
        let reason = response["incomplete_details"]["reason"]
            .as_str()
            .unwrap_or("no reason given");
        bail!("the response is {status}: {reason}");
    }
    let output = match &response["output"] {
        Value::Array(output) => output.clone(),
        _ => bail!("the response has no output"),
    };
    let calls = output
        .iter()
        .filter(|item| item["type"] == "function_call")
        .map(|item| {
            let name = item["name"].as_str().unwrap_or_default().to_string();
            ToolCall {
                call_id: item["call_id"].as_str().unwrap_or_default().to_string(),
                arguments: arguments(&name, &item["arguments"]),
                name,
            }
        })
        .collect();
    let message = output.iter().rev().find(|item| item["type"] == "message");
    let parts = message.and_then(|message| message["content"].as_array());
    let text = parts
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "output_text")
        .filter_map(|part| part["text"].as_str())
        .collect();
    Ok(Turn {
        assistant: output,
        text,
        calls,
        usage: tokens(&response["usage"], "input_tokens", "output_tokens"),
    })
}

fn parse_anthropic(response: Value) -> anyhow::Result<Turn> {
    if let Some(reason @ ("refusal" | "pause_turn" | "max_tokens")) =
        response["stop_reason"].as_str()
    {
        bail!("the response stopped with {reason}");
    }
    let content = match &response["content"] {
        Value::Array(content) => content.clone(),
        _ => bail!("the response has no content"),
    };
    let calls = content
        .iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| {
            let name = block["name"].as_str().unwrap_or_default().to_string();
            ToolCall {
                call_id: block["id"].as_str().unwrap_or_default().to_string(),
                arguments: arguments(&name, &block["input"]),
                name,
            }
        })
        .collect();
    let text = content
        .iter()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect();
    Ok(Turn {
        assistant: vec![json!({"role": "assistant", "content": content})],
        text,
        calls,
        usage: tokens(&response["usage"], "input_tokens", "output_tokens"),
    })
}

fn parse_chat(response: Value) -> anyhow::Result<Turn> {
    let choice = &response["choices"][0];
    if let Some(reason @ ("length" | "content_filter")) = choice["finish_reason"].as_str() {
        bail!("the response stopped with {reason}");
    }
    let mut message = match &choice["message"] {
        message @ Value::Object(_) => message.clone(),
        _ => bail!("the response has no message"),
    };
    if let Some(refusal) = message["refusal"].as_str() {
        bail!("the model refused: {refusal}");
    }
    let mut calls = Vec::new();
    if let Some(tool_calls) = message["tool_calls"].as_array_mut() {
        let mut taken: HashSet<String> = (tool_calls.iter())
            .filter_map(|call| call["id"].as_str().map(str::to_string))
            .collect();
        for (i, call) in tool_calls.iter_mut().enumerate() {
            // Some servers leave out the id; the result must name the same one.
            if !call["id"].is_string() {
                let mut id = format!("call_{i}");
                while taken.contains(&id) {
                    id.push('_');
                }
                taken.insert(id.clone());
                call["id"] = json!(id);
            }
            let name = call["function"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            calls.push(ToolCall {
                call_id: call["id"].as_str().unwrap_or_default().to_string(),
                arguments: arguments(&name, &call["function"]["arguments"]),
                name,
            });
        }
    }
    let text = message["content"].as_str().unwrap_or_default().to_string();
    Ok(Turn {
        assistant: vec![message],
        text,
        calls,
        usage: tokens(&response["usage"], "prompt_tokens", "completion_tokens"),
    })
}
