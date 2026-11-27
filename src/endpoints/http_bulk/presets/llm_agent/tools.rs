//  mq-bridge
//  © Copyright 2026, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! The tools of an `llm_agent`: endpoints that answer, and the tools of MCP servers.

use super::adapters::{ToolCall, ToolResult, ToolSpec};
use crate::endpoints::create_publisher_from_route;
use crate::errors::PublisherError;
use crate::middleware::lookup::answer_of;
use crate::models::{LlmMcpServer, LlmTool, TlsConfig};
use crate::outcomes::Sent;
use crate::traits::MessagePublisher;
use crate::CanonicalMessage;
use anyhow::{anyhow, bail, Context};
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

type McpClient = RunningService<RoleClient, ()>;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long one MCP tool call may take before the message is retried.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

/// A failed call: `Model` goes back to the model as an error result, `Message` fails the message.
enum Failure {
    Model(String),
    Message(PublisherError),
}

impl From<PublisherError> for Failure {
    fn from(error: PublisherError) -> Self {
        match error {
            PublisherError::NonRetryable(error) => Failure::Model(format!("{error:#}")),
            error => Failure::Message(error),
        }
    }
}

enum Runner {
    Endpoint(Arc<dyn MessagePublisher>),
    Mcp(Arc<McpServer>),
}

pub(super) struct Tools {
    pub specs: Vec<ToolSpec>,
    runners: HashMap<String, Runner>,
}

impl Tools {
    pub(super) async fn new(
        route_name: &str,
        tools: &[LlmTool],
        servers: &[LlmMcpServer],
    ) -> anyhow::Result<Self> {
        let mut found = Self {
            specs: Vec::new(),
            runners: HashMap::new(),
        };
        for tool in tools {
            if let Some(reason) = crate::endpoints::never_replies(&tool.endpoint, 0) {
                bail!(
                    "tool `{}`: {reason}; a tool's endpoint must answer",
                    tool.name
                );
            }
            // Box::pin breaks the recursive async type, as in the lookup middleware.
            let publisher = Box::pin(create_publisher_from_route(route_name, &tool.endpoint))
                .await
                .with_context(|| format!("tool `{}`", tool.name))?;
            let spec = ToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone().unwrap_or_default(),
                schema: tool
                    .schema
                    .clone()
                    .unwrap_or_else(|| json!({"type": "object"})),
            };
            found.add(spec, Runner::Endpoint(publisher), "the endpoint tools")?;
        }
        for server in servers {
            let client = connect(server).await?;
            let listing = format!("listing the tools of MCP server `{}`", server.name);
            let listed = tokio::time::timeout(CONNECT_TIMEOUT, client.list_all_tools())
                .await
                .map_err(|_| anyhow!("no answer within {}s", CONNECT_TIMEOUT.as_secs()))
                .context(listing.clone())?
                .context(listing)?;
            if let Some(allowed) = &server.allowed_tools {
                if let Some(missing) = allowed
                    .iter()
                    .find(|name| !listed.iter().any(|tool| tool.name == name.as_str()))
                {
                    bail!("MCP server `{}` has no tool `{missing}`", server.name);
                }
            }
            let server = Arc::new(McpServer {
                config: server.clone(),
                client: RwLock::new(Arc::new(client)),
            });
            for tool in listed {
                let allowed = server.config.allowed_tools.as_ref();
                if allowed.is_some_and(|allowed| !allowed.iter().any(|name| *name == tool.name)) {
                    continue;
                }
                let spec = ToolSpec {
                    name: tool.name.to_string(),
                    description: tool.description.as_deref().unwrap_or_default().to_string(),
                    schema: Value::Object((*tool.input_schema).clone()),
                };
                let origin = format!("MCP server `{}`", server.config.name);
                found.add(spec, Runner::Mcp(server.clone()), &origin)?;
            }
        }
        Ok(found)
    }

    fn add(&mut self, spec: ToolSpec, runner: Runner, origin: &str) -> anyhow::Result<()> {
        if self.runners.contains_key(&spec.name) {
            bail!(
                "two tools are named `{}` (the second from {origin}); tool names must be unique",
                spec.name
            );
        }
        self.runners.insert(spec.name.clone(), runner);
        self.specs.push(spec);
        Ok(())
    }

    /// The result for the model; an error only when the whole message must fail.
    pub(super) async fn call(&self, call: ToolCall) -> Result<ToolResult, PublisherError> {
        let failed = |content: String| ToolResult {
            call_id: call.call_id.clone(),
            content,
            is_error: true,
        };
        let arguments = match &call.arguments {
            Ok(arguments) => arguments.clone(),
            Err(error) => return Ok(failed(error.clone())),
        };
        let outcome = match self.runners.get(&call.name) {
            None => return Ok(failed(format!("there is no tool `{}`", call.name))),
            Some(Runner::Endpoint(publisher)) => {
                endpoint_call(&call.name, publisher, arguments).await
            }
            Some(Runner::Mcp(server)) => server.call(&call.name, arguments).await,
        };
        match outcome {
            Ok((content, is_error)) => Ok(ToolResult {
                call_id: call.call_id.clone(),
                content,
                is_error,
            }),
            Err(Failure::Model(error)) => Ok(failed(error)),
            Err(Failure::Message(error)) => Err(error),
        }
    }
}

async fn endpoint_call(
    name: &str,
    publisher: &Arc<dyn MessagePublisher>,
    arguments: Value,
) -> Result<(String, bool), Failure> {
    let request = CanonicalMessage::new(arguments.to_string().into_bytes(), None);
    let sent = publisher.send(request).await?;
    if matches!(sent, Sent::Ack) {
        return Err(Failure::Message(PublisherError::NonRetryable(anyhow!(
            "tool `{name}`: its endpoint gives no answer; use one that replies"
        ))));
    }
    let content = match answer_of(sent)? {
        None => "no result".to_string(),
        Some(Value::String(text)) => text,
        Some(value) => value.to_string(),
    };
    Ok((content, false))
}

/// One MCP server; a broken connection is set up again for the next call.
struct McpServer {
    config: LlmMcpServer,
    client: RwLock<Arc<McpClient>>,
}

impl McpServer {
    async fn call(&self, tool: &str, arguments: Value) -> Result<(String, bool), Failure> {
        let Value::Object(arguments) = arguments else {
            return Err(Failure::Model("the arguments are not a JSON object".into()));
        };
        let client = self.client.read().await.clone();
        let params = CallToolRequestParams::new(tool.to_string()).with_arguments(arguments);
        let Ok(called) = tokio::time::timeout(CALL_TIMEOUT, client.call_tool(params)).await else {
            return Err(Failure::Message(PublisherError::Retryable(anyhow!(
                "MCP server `{}`: `{tool}` gave no result within {}s",
                self.config.name,
                CALL_TIMEOUT.as_secs()
            ))));
        };
        match called {
            Ok(result) => {
                let result = serde_json::to_value(&result).unwrap_or_default();
                Ok((tool_text(&result), result["isError"] == true))
            }
            Err(rmcp::ServiceError::McpError(error)) => {
                Err(Failure::Model(error.message.into_owned()))
            }
            Err(error) => {
                let error = anyhow!("MCP server `{}`: {error}", self.config.name);
                self.reconnect(&client).await;
                Err(Failure::Message(PublisherError::Retryable(error)))
            }
        }
    }

    async fn reconnect(&self, broken: &Arc<McpClient>) {
        let mut client = self.client.write().await;
        if !Arc::ptr_eq(&client, broken) {
            return;
        }
        match connect(&self.config).await {
            Ok(fresh) => *client = Arc::new(fresh),
            Err(error) => tracing::warn!(
                server = %self.config.name,
                "llm_agent could not reconnect to its MCP server: {error:#}"
            ),
        }
    }
}

/// The text parts of a tool result; else its structured content, else all of it.
fn tool_text(result: &Value) -> String {
    let texts: Vec<&str> = result["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect();
    if !texts.is_empty() {
        return texts.join("\n");
    }
    match &result["structuredContent"] {
        Value::Null => result["content"].to_string(),
        structured => structured.to_string(),
    }
}

async fn connect(server: &LlmMcpServer) -> anyhow::Result<McpClient> {
    let name = &server.name;
    let client = match (&server.url, &server.command) {
        (Some(url), None) => {
            let mut config = StreamableHttpClientTransportConfig::with_uri(url.as_str());
            if let Some(token) = &server.token {
                config = config.auth_header(token.clone());
            }
            let builder = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10));
            let builder =
                crate::support::tls::reqwest_tls(builder, &TlsConfig::default(), "llm_agent")?;
            let transport = StreamableHttpClientTransport::with_client(builder.build()?, config);
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)).await
        }
        (None, Some(command)) => {
            if server.token.is_some() {
                bail!("MCP server `{name}`: 'token' needs 'url'");
            }
            let mut process = tokio::process::Command::new(command);
            process.args(&server.args).envs(&server.env);
            let transport = TokioChildProcess::new(process)
                .with_context(|| format!("MCP server `{name}`: starting `{command}`"))?;
            tokio::time::timeout(CONNECT_TIMEOUT, ().serve(transport)).await
        }
        _ => bail!("MCP server `{name}` needs either 'url' or 'command'"),
    };
    client
        .map_err(|_| anyhow!("no MCP handshake within {}s", CONNECT_TIMEOUT.as_secs()))
        .and_then(|client| client.map_err(anyhow::Error::from))
        .with_context(|| format!("connecting to MCP server `{name}`"))
}
