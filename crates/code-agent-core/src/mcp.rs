use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use code_agent_mcp::{
    agent_tool_name, mcp_result_to_text_and_metadata, mutating_tool_name, McpClient,
    McpConnectionStatus, McpError, McpLimits, McpOptions, McpServerConfig, McpServerStatus,
    McpToolCallCompleted, McpToolCallFailed, McpToolCallStarted, McpToolDefinition, McpToolSummary,
    McpToolsDiscovered,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::errors::ToolError;
use crate::permission::PermissionKind;
use crate::tool::{Tool, ToolContent, ToolContext, ToolRegistry, ToolResult};

#[derive(Default)]
pub struct McpRegistrationReport {
    pub statuses: Vec<McpServerStatus>,
    pub discovered: Vec<McpToolsDiscovered>,
    pub logs: Vec<(String, code_agent_mcp::McpServerLogEntry)>,
}

pub async fn register_mcp_tools(
    registry: &mut ToolRegistry,
    options: &McpOptions,
) -> McpRegistrationReport {
    let mut report = McpRegistrationReport::default();
    if !options.enabled {
        return report;
    }
    let limits = options.limits();
    let secrets = options.secret_values();
    for (server_name, server_config) in options.servers.iter().take(options.max_servers) {
        let transport = transport_kind(server_config);
        let mut status = McpServerStatus {
            name: server_name.clone(),
            status: McpConnectionStatus::Connecting,
            transport,
            protocol_version: None,
            tool_count: 0,
            resource_count: None,
            prompt_count: None,
            server_info: None,
            error: None,
        };
        let mut client =
            match McpClient::connect(server_name.clone(), server_config, limits, secrets.clone())
                .await
            {
                Ok(client) => client,
                Err(err) => {
                    status.status = McpConnectionStatus::Failed;
                    status.error = Some(err.to_string());
                    report.statuses.push(status);
                    continue;
                }
            };
        match client.initialize().await {
            Ok(init) => {
                status.protocol_version = Some(init.protocol_version);
                status.server_info = Some(init.server_info);
            }
            Err(err) => {
                status.status = McpConnectionStatus::Failed;
                status.error = Some(err.to_string());
                report.logs.extend(
                    client
                        .drain_logs()
                        .into_iter()
                        .map(|entry| (server_name.clone(), entry)),
                );
                report.statuses.push(status);
                continue;
            }
        }
        let tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(err) => {
                status.status = McpConnectionStatus::Failed;
                status.error = Some(err.to_string());
                report.logs.extend(
                    client
                        .drain_logs()
                        .into_iter()
                        .map(|entry| (server_name.clone(), entry)),
                );
                report.statuses.push(status);
                continue;
            }
        };
        if tools.len() > options.max_tools_per_server {
            status.status = McpConnectionStatus::Failed;
            status.error = Some(format!(
                "server exposed {} tools, exceeding max_tools_per_server {}",
                tools.len(),
                options.max_tools_per_server
            ));
            report.statuses.push(status);
            continue;
        }
        let client = Arc::new(Mutex::new(client));
        let mut summaries = Vec::new();
        for tool in tools {
            let agent_name = match agent_tool_name(server_name, &tool.name) {
                Ok(name) => name,
                Err(err) => {
                    status.status = McpConnectionStatus::Failed;
                    status.error = Some(err);
                    continue;
                }
            };
            if registry.get(&agent_name).is_some() {
                status.status = McpConnectionStatus::Failed;
                status.error = Some(format!("duplicate MCP tool name {agent_name}"));
                continue;
            }
            summaries.push(McpToolSummary {
                server_name: server_name.clone(),
                remote_tool_name: tool.name.clone(),
                agent_tool_name: agent_name.clone(),
                description: tool.description.clone(),
            });
            registry.register(Arc::new(McpTool::new(
                server_name.clone(),
                transport,
                agent_name,
                tool,
                client.clone(),
                limits,
            )));
        }
        status.status = if status.error.is_some() {
            McpConnectionStatus::Failed
        } else {
            McpConnectionStatus::Connected
        };
        status.tool_count = summaries.len();
        if options.load_resources {
            let mut locked = client.lock().await;
            status.resource_count = locked.list_resources().await.ok().map(|r| r.len());
        }
        if options.load_prompts {
            let mut locked = client.lock().await;
            status.prompt_count = locked.list_prompts().await.ok().map(|p| p.len());
        }
        report.discovered.push(McpToolsDiscovered {
            server_name: server_name.clone(),
            tools: summaries,
        });
        report.logs.extend(
            client
                .lock()
                .await
                .drain_logs()
                .into_iter()
                .map(|entry| (server_name.clone(), entry)),
        );
        report.statuses.push(status);
    }
    report
}

pub fn transport_kind(config: &McpServerConfig) -> code_agent_mcp::McpTransportKind {
    match config {
        McpServerConfig::Stdio(_) => code_agent_mcp::McpTransportKind::Stdio,
        McpServerConfig::StreamableHttp(_) => code_agent_mcp::McpTransportKind::StreamableHttp,
    }
}

pub fn tool_metadata(
    tool_name: &str,
    transport: Option<code_agent_mcp::McpTransportKind>,
) -> Value {
    let Some(parts) = code_agent_mcp::parse_agent_tool_name(tool_name) else {
        return Value::Null;
    };
    let mut warning =
        "MCP tool effects are external and are not covered by workspace checkpointing.".to_string();
    if mutating_tool_name(&parts.remote_tool_name) {
        warning =
            "This MCP tool may modify external state. Workspace rewind cannot undo it.".to_string();
    }
    json!({
        "mcp": {
            "server_name": parts.server_name,
            "remote_tool_name": parts.remote_tool_name,
            "agent_tool_name": tool_name,
            "transport": transport.map(|t| t.to_string()),
            "external_effect_warning": warning,
            "checkpoint_coverage": "not_applicable",
        }
    })
}

pub struct McpTool {
    server_name: String,
    remote_tool_name: String,
    agent_tool_name: String,
    description: String,
    input_schema: Value,
    output_schema: Option<Value>,
    annotations: Option<Value>,
    transport: code_agent_mcp::McpTransportKind,
    client: Arc<Mutex<McpClient>>,
    limits: McpLimits,
}

impl McpTool {
    fn new(
        server_name: String,
        transport: code_agent_mcp::McpTransportKind,
        agent_tool_name: String,
        definition: McpToolDefinition,
        client: Arc<Mutex<McpClient>>,
        limits: McpLimits,
    ) -> Self {
        let description = definition.description.clone().unwrap_or_else(|| {
            format!(
                "External MCP tool {}.{}; effects are not covered by workspace checkpointing.",
                server_name, definition.name
            )
        });
        Self {
            server_name,
            remote_tool_name: definition.name,
            agent_tool_name,
            description,
            input_schema: definition.input_schema,
            output_schema: definition.output_schema,
            annotations: definition.annotations,
            transport,
            client,
            limits,
        }
    }

    pub fn call_started(&self) -> McpToolCallStarted {
        McpToolCallStarted {
            server_name: self.server_name.clone(),
            remote_tool_name: self.remote_tool_name.clone(),
            agent_tool_name: self.agent_tool_name.clone(),
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.agent_tool_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::External
    }

    fn approval_summary(&self, input: &Value) -> String {
        let mut warning =
            "Warning: MCP tool effects are external and are not covered by workspace checkpointing."
                .to_string();
        if mutating_tool_name(&self.remote_tool_name) {
            warning =
                "Warning: This MCP tool may modify external state. Workspace rewind cannot undo it."
                    .to_string();
        }
        format!(
            "Tool: {}\nServer: {}\nRemote tool: {}\nTransport: {}\nArguments: {}\nExternal access: yes\nCheckpoint coverage: not applicable\n{}",
            self.agent_tool_name,
            self.server_name,
            self.remote_tool_name,
            self.transport,
            input,
            warning
        )
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let started = Instant::now();
        let mut client = self.client.lock().await;
        let result = client
            .call_tool(&self.remote_tool_name, input)
            .await
            .map_err(tool_error_from_mcp)?;
        let (text, metadata, result_bytes, truncated) =
            mcp_result_to_text_and_metadata(&result, self.limits.max_tool_result_bytes)
                .map_err(tool_error_from_mcp)?;
        let metadata = json!({
            "mcp": {
                "server_name": self.server_name,
                "remote_tool_name": self.remote_tool_name,
                "agent_tool_name": self.agent_tool_name,
                "transport": self.transport,
                "duration_ms": started.elapsed().as_millis() as u64,
                "result_bytes": result_bytes,
                "truncated": truncated,
                "structured_content": metadata.get("mcp").and_then(|m| m.get("structured_content")).cloned().unwrap_or(Value::Null),
                "output_schema": self.output_schema,
                "annotations": self.annotations,
                "checkpoint_coverage": "not_applicable",
            }
        });
        Ok(ToolResult {
            content: vec![ToolContent::Text(text)],
            is_error: result.is_error,
            metadata,
        })
    }
}

fn tool_error_from_mcp(err: McpError) -> ToolError {
    ToolError::Other(format!("MCP error: {err}"))
}

pub fn mcp_completion_from_result(
    tool_name: &str,
    result: &ToolResult,
    duration_ms: u64,
) -> Option<McpToolCallCompleted> {
    let parts = code_agent_mcp::parse_agent_tool_name(tool_name)?;
    Some(McpToolCallCompleted {
        server_name: parts.server_name,
        remote_tool_name: parts.remote_tool_name,
        agent_tool_name: tool_name.to_string(),
        duration_ms,
        is_error: result.is_error,
        result_bytes: result.flatten_text().len(),
    })
}

pub fn mcp_failure_from_error(tool_name: &str, error: &str) -> Option<McpToolCallFailed> {
    let parts = code_agent_mcp::parse_agent_tool_name(tool_name)?;
    Some(McpToolCallFailed {
        server_name: parts.server_name,
        remote_tool_name: parts.remote_tool_name,
        agent_tool_name: tool_name.to_string(),
        error: error.to_string(),
    })
}
