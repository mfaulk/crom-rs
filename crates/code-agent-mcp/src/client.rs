use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{McpLimits, McpServerConfig};
use crate::errors::{McpError, McpResult};
use crate::prompts::McpPrompt;
use crate::protocol::{
    ClientCapabilities, ClientInfo, InitializeRequest, InitializeResponse, JsonRpcIdAllocator,
    JsonRpcNotification, JsonRpcRequest, ServerCapabilities, ServerInfo, MCP_PROTOCOL_VERSION,
};
use crate::resources::{McpResource, McpResourceContents};
use crate::tools::{McpToolDefinition, McpToolResult};
use crate::transport::stdio::StdioMcpTransport;
use crate::transport::streamable_http::StreamableHttpMcpTransport;
use crate::transport::{response_value, McpServerLogEntry, McpTransport, McpTransportKind};

/// Hard cap on the number of `*/list` pages we will iterate before giving
/// up. A misbehaving server that always advertises a non-empty
/// `nextCursor` would otherwise hang the client forever; this cap turns
/// that into a structured error instead.
pub const MCP_MAX_LIST_PAGES: usize = 1024;

pub struct McpClient {
    pub server_name: String,
    pub transport_kind: McpTransportKind,
    pub capabilities: ServerCapabilities,
    pub server_info: Option<ServerInfo>,
    pub protocol_version: Option<String>,
    transport: Box<dyn McpTransport>,
    ids: JsonRpcIdAllocator,
    startup_timeout: Duration,
    request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct McpServerInit {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ServerInfo,
    pub instructions: Option<String>,
}

impl McpClient {
    pub async fn connect(
        server_name: impl Into<String>,
        config: &McpServerConfig,
        limits: McpLimits,
        secrets: Vec<String>,
    ) -> McpResult<Self> {
        let server_name = server_name.into();
        let startup_timeout = config.startup_timeout().unwrap_or(limits.startup_timeout);
        let request_timeout = config.request_timeout().unwrap_or(limits.request_timeout);
        let (transport_kind, transport): (McpTransportKind, Box<dyn McpTransport>) = match config {
            McpServerConfig::Stdio(stdio) => {
                let t = StdioMcpTransport::spawn(stdio, limits, secrets).await?;
                (McpTransportKind::Stdio, Box::new(t))
            }
            McpServerConfig::StreamableHttp(http) => {
                let t = StreamableHttpMcpTransport::new(http, limits, secrets)?;
                (McpTransportKind::StreamableHttp, Box::new(t))
            }
        };
        Ok(Self {
            server_name,
            transport_kind,
            capabilities: ServerCapabilities::default(),
            server_info: None,
            protocol_version: None,
            transport,
            ids: JsonRpcIdAllocator::new(),
            startup_timeout,
            request_timeout,
        })
    }

    pub async fn initialize(&mut self) -> McpResult<McpServerInit> {
        let params = InitializeRequest {
            protocol_version: MCP_PROTOCOL_VERSION.into(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "code-agent".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
        };
        let value = self
            .request_with_timeout(
                "initialize",
                Some(serde_json::to_value(params)?),
                self.startup_timeout,
            )
            .await?;
        let response: InitializeResponse = serde_json::from_value(value)?;
        if response.protocol_version.trim().is_empty() {
            return Err(McpError::InvalidJsonRpc(
                "initialize response missing protocolVersion".into(),
            ));
        }
        self.protocol_version = Some(response.protocol_version.clone());
        self.capabilities = response.capabilities.clone();
        self.server_info = Some(response.server_info.clone());
        self.transport
            .set_protocol_version(Some(response.protocol_version.clone()));
        self.send_notification("notifications/initialized", None)
            .await?;
        Ok(McpServerInit {
            protocol_version: response.protocol_version,
            capabilities: response.capabilities,
            server_info: response.server_info,
            instructions: response.instructions,
        })
    }

    pub async fn list_tools(&mut self) -> McpResult<Vec<McpToolDefinition>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MCP_MAX_LIST_PAGES {
            let params = cursor
                .as_ref()
                .map(|c| json!({ "cursor": c }))
                .or_else(|| Some(json!({})));
            let value = self.request("tools/list", params).await?;
            let page: ListToolsResponse = serde_json::from_value(value)?;
            out.extend(page.tools);
            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    if Some(&next) == cursor.as_ref() {
                        // Server is returning the same cursor twice in a row.
                        // Treat as a misbehaving server rather than spinning.
                        return Err(McpError::InvalidJsonRpc(
                            "tools/list pagination cursor did not advance".into(),
                        ));
                    }
                    cursor = Some(next);
                }
                _ => return Ok(out),
            }
        }
        Err(McpError::InvalidJsonRpc(format!(
            "tools/list exceeded {MCP_MAX_LIST_PAGES} pages"
        )))
    }

    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> McpResult<McpToolResult> {
        let value = self
            .request(
                "tools/call",
                Some(json!({ "name": name, "arguments": arguments })),
            )
            .await?;
        serde_json::from_value(value).map_err(McpError::from)
    }

    pub async fn list_resources(&mut self) -> McpResult<Vec<McpResource>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MCP_MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let value = self.request("resources/list", params).await?;
            let page: ListResourcesResponse = serde_json::from_value(value)?;
            out.extend(page.resources);
            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    if Some(&next) == cursor.as_ref() {
                        return Err(McpError::InvalidJsonRpc(
                            "resources/list pagination cursor did not advance".into(),
                        ));
                    }
                    cursor = Some(next);
                }
                _ => return Ok(out),
            }
        }
        Err(McpError::InvalidJsonRpc(format!(
            "resources/list exceeded {MCP_MAX_LIST_PAGES} pages"
        )))
    }

    pub async fn read_resource(&mut self, uri: &str) -> McpResult<Vec<McpResourceContents>> {
        let value = self
            .request("resources/read", Some(json!({ "uri": uri })))
            .await?;
        let response: ReadResourceResponse = serde_json::from_value(value)?;
        Ok(response.contents)
    }

    pub async fn list_prompts(&mut self) -> McpResult<Vec<McpPrompt>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MCP_MAX_LIST_PAGES {
            let params = cursor.as_ref().map(|c| json!({ "cursor": c }));
            let value = self.request("prompts/list", params).await?;
            let page: ListPromptsResponse = serde_json::from_value(value)?;
            out.extend(page.prompts);
            match page.next_cursor {
                Some(next) if !next.is_empty() => {
                    if Some(&next) == cursor.as_ref() {
                        return Err(McpError::InvalidJsonRpc(
                            "prompts/list pagination cursor did not advance".into(),
                        ));
                    }
                    cursor = Some(next);
                }
                _ => return Ok(out),
            }
        }
        Err(McpError::InvalidJsonRpc(format!(
            "prompts/list exceeded {MCP_MAX_LIST_PAGES} pages"
        )))
    }

    pub fn drain_logs(&mut self) -> Vec<McpServerLogEntry> {
        self.transport.drain_logs()
    }

    pub async fn shutdown(&mut self) -> McpResult<()> {
        self.transport.shutdown().await
    }

    async fn request(&mut self, method: &str, params: Option<Value>) -> McpResult<Value> {
        self.request_with_timeout(method, params, self.request_timeout)
            .await
    }

    async fn request_with_timeout(
        &mut self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> McpResult<Value> {
        let request = JsonRpcRequest::new(self.ids.next_id(), method, params);
        let started = Instant::now();
        let response = tokio::time::timeout(timeout, self.transport.send_request(request))
            .await
            .map_err(|_| McpError::timeout(timeout))??;
        tracing::debug!(
            server = %self.server_name,
            method,
            duration_ms = started.elapsed().as_millis() as u64,
            "MCP request completed"
        );
        response_value(response)
    }

    async fn send_notification(&mut self, method: &str, params: Option<Value>) -> McpResult<()> {
        let notification = JsonRpcNotification::new(method, params);
        tokio::time::timeout(
            self.request_timeout,
            self.transport.send_notification(notification),
        )
        .await
        .map_err(|_| McpError::timeout(self.request_timeout))?
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListToolsResponse {
    #[serde(default)]
    tools: Vec<McpToolDefinition>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResourcesResponse {
    #[serde(default)]
    resources: Vec<McpResource>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReadResourceResponse {
    #[serde(default)]
    contents: Vec<McpResourceContents>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListPromptsResponse {
    #[serde(default)]
    prompts: Vec<McpPrompt>,
    #[serde(default, rename = "nextCursor")]
    next_cursor: Option<String>,
}
