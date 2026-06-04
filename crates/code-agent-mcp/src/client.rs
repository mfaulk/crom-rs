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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{JsonRpcId, JsonRpcResponse};
    use crate::transport::{McpServerLogEntry, McpTransport, McpTransportKind};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Recorded `(method, params)` pairs for each outgoing request or
    /// notification, shared with the test for inspection after the calls.
    type RecordedCalls = Arc<Mutex<Vec<(String, Option<Value>)>>>;

    /// In-process transport driven by a script of canned responses keyed by
    /// JSON-RPC method. Records every request/notification for assertions so
    /// these tests exercise the real `McpClient` state machine without
    /// spawning a child process or opening a socket.
    #[derive(Default)]
    struct MockTransport {
        /// Per-method FIFO queues of responses to hand back.
        responses: Mutex<std::collections::HashMap<String, VecDeque<JsonRpcResponse>>>,
        /// Method **and params** of every `send_request` call, in order, so
        /// tests can assert on the exact payload the client sent (e.g. the
        /// pagination cursor or a tool's name/arguments) rather than just the
        /// method name.
        requests: RecordedCalls,
        /// Method **and params** of every `send_notification` call, in order.
        notifications: RecordedCalls,
        /// The last protocol version the client pushed via
        /// `set_protocol_version`, exposed through a shared handle so a test
        /// can assert the negotiated version was propagated into the transport.
        protocol_version: Arc<Mutex<Option<String>>>,
        /// When set, every request first sleeps this long (to drive timeouts).
        request_delay: Option<Duration>,
    }

    impl MockTransport {
        fn push(&self, method: &str, response: JsonRpcResponse) {
            self.responses
                .lock()
                .unwrap()
                .entry(method.to_string())
                .or_default()
                .push_back(response);
        }

        fn ok(id: u64, result: Value) -> JsonRpcResponse {
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Some(JsonRpcId::Number(id)),
                result: Some(result),
                error: None,
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        fn kind(&self) -> McpTransportKind {
            McpTransportKind::Stdio
        }

        async fn send_request(&mut self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
            if let Some(delay) = self.request_delay {
                tokio::time::sleep(delay).await;
            }
            self.requests
                .lock()
                .unwrap()
                .push((request.method.clone(), request.params.clone()));
            let mut map = self.responses.lock().unwrap();
            let resp = map
                .get_mut(&request.method)
                .and_then(|q| q.pop_front())
                .unwrap_or_else(|| panic!("no canned response for {}", request.method));
            // Echo back the request id so response_id_matches-style checks in
            // real transports would pass; the mock returns directly anyway.
            Ok(JsonRpcResponse {
                id: request.id.clone().into(),
                ..resp
            })
        }

        async fn send_notification(&mut self, notification: JsonRpcNotification) -> McpResult<()> {
            self.notifications
                .lock()
                .unwrap()
                .push((notification.method.clone(), notification.params.clone()));
            Ok(())
        }

        fn set_protocol_version(&mut self, version: Option<String>) {
            *self.protocol_version.lock().unwrap() = version;
        }

        fn drain_logs(&mut self) -> Vec<McpServerLogEntry> {
            Vec::new()
        }
    }

    fn client_with(transport: MockTransport) -> McpClient {
        McpClient {
            server_name: "mock".into(),
            transport_kind: McpTransportKind::Stdio,
            capabilities: ServerCapabilities::default(),
            server_info: None,
            protocol_version: None,
            transport: Box::new(transport),
            ids: JsonRpcIdAllocator::new(),
            startup_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(5),
        }
    }

    fn init_result(version: &str) -> Value {
        json!({
            "protocolVersion": version,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "0.1.0"}
        })
    }

    #[tokio::test]
    // A successful initialize records the negotiated protocol version, captures
    // capabilities/server-info, pushes the version into the transport, and
    // sends exactly one `notifications/initialized`.
    async fn initialize_negotiates_and_sends_initialized() {
        let transport = MockTransport::default();
        let notes = transport.notifications.clone();
        let reqs = transport.requests.clone();
        let pushed_version = transport.protocol_version.clone();
        transport.push(
            "initialize",
            MockTransport::ok(1, init_result("2025-06-18")),
        );
        let mut client = client_with(transport);

        let init = client.initialize().await.unwrap();
        assert_eq!(init.protocol_version, "2025-06-18");
        assert_eq!(client.protocol_version.as_deref(), Some("2025-06-18"));
        assert_eq!(init.server_info.name, "mock");
        assert!(client.capabilities.tools.is_some());

        // The client must have advertised the protocol version it supports in
        // the `initialize` request params.
        let reqs = reqs.lock().unwrap();
        let (method, params) = &reqs[0];
        assert_eq!(method, "initialize");
        let params = params.as_ref().expect("initialize params present");
        assert_eq!(params["protocolVersion"], json!(MCP_PROTOCOL_VERSION));

        // The negotiated version from the *response* must be propagated into
        // the transport via `set_protocol_version`. If production dropped the
        // `set_protocol_version(Some(...))` call, this handle stays `None` and
        // the assertion fails.
        assert_eq!(
            pushed_version.lock().unwrap().as_deref(),
            Some("2025-06-18"),
        );

        // Exactly one initialized notification, sent after the response.
        let sent = notes.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, "notifications/initialized");
    }

    #[tokio::test]
    // An initialize response with a blank protocolVersion is rejected (the
    // server failed to negotiate) and no initialized notification is sent.
    async fn initialize_rejects_blank_protocol_version() {
        let transport = MockTransport::default();
        let notes = transport.notifications.clone();
        transport.push("initialize", MockTransport::ok(1, init_result("   ")));
        let mut client = client_with(transport);

        let err = client.initialize().await.unwrap_err();
        assert!(matches!(err, McpError::InvalidJsonRpc(_)));
        // No initialized handshake on a failed negotiation.
        assert!(notes.lock().unwrap().is_empty());
    }

    #[tokio::test]
    // A JSON-RPC error response to initialize surfaces as McpError::JsonRpc.
    async fn initialize_propagates_server_error() {
        let transport = MockTransport::default();
        transport.push(
            "initialize",
            JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Some(JsonRpcId::Number(1)),
                result: None,
                error: Some(crate::protocol::JsonRpcError {
                    code: -32603,
                    message: "internal".into(),
                    data: None,
                }),
            },
        );
        let mut client = client_with(transport);
        let err = client.initialize().await.unwrap_err();
        assert!(matches!(err, McpError::JsonRpc { .. }));
    }

    #[tokio::test]
    // list_tools follows `nextCursor` across pages and concatenates the tools
    // from every page in order.
    async fn list_tools_paginates_across_pages() {
        let transport = MockTransport::default();
        let reqs = transport.requests.clone();
        transport.push(
            "tools/list",
            MockTransport::ok(
                0,
                json!({
                    "tools": [{"name": "a", "inputSchema": {"type": "object"}}],
                    "nextCursor": "p2"
                }),
            ),
        );
        transport.push(
            "tools/list",
            MockTransport::ok(
                0,
                json!({
                    "tools": [{"name": "b", "inputSchema": {"type": "object"}}],
                    "nextCursor": ""
                }),
            ),
        );
        let mut client = client_with(transport);

        let tools = client.list_tools().await.unwrap();
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);

        let reqs = reqs.lock().unwrap();
        // Two pages => two requests.
        assert_eq!(reqs.len(), 2);
        // The first page is fetched without a cursor.
        assert_eq!(reqs[0].0, "tools/list");
        assert_eq!(reqs[0].1, Some(json!({})));
        // The second request must carry the cursor the first page returned
        // (`"p2"`). If `list_tools` stopped forwarding `nextCursor`, the second
        // request would omit it (`{}`) and this assertion would fail.
        assert_eq!(reqs[1].0, "tools/list");
        assert_eq!(reqs[1].1, Some(json!({ "cursor": "p2" })));
    }

    #[tokio::test]
    // A server that returns the same `nextCursor` twice is treated as
    // misbehaving and rejected instead of spinning forever.
    async fn list_tools_rejects_non_advancing_cursor() {
        let transport = MockTransport::default();
        transport.push(
            "tools/list",
            MockTransport::ok(0, json!({"tools": [], "nextCursor": "same"})),
        );
        transport.push(
            "tools/list",
            MockTransport::ok(0, json!({"tools": [], "nextCursor": "same"})),
        );
        let mut client = client_with(transport);

        let err = client.list_tools().await.unwrap_err();
        match err {
            McpError::InvalidJsonRpc(msg) => {
                assert!(msg.contains("did not advance"), "got: {msg}");
            }
            other => panic!("expected InvalidJsonRpc, got {other:?}"),
        }
    }

    #[tokio::test]
    // A single page with no cursor terminates after one request.
    async fn list_tools_single_page() {
        let transport = MockTransport::default();
        let reqs = transport.requests.clone();
        transport.push(
            "tools/list",
            MockTransport::ok(
                0,
                json!({"tools": [{"name": "only", "inputSchema": {"type": "object"}}]}),
            ),
        );
        let mut client = client_with(transport);
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(reqs.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    // call_tool forwards name+arguments and shapes the response into an
    // McpToolResult (content + isError flag).
    async fn call_tool_shapes_result() {
        let transport = MockTransport::default();
        let reqs = transport.requests.clone();
        transport.push(
            "tools/call",
            MockTransport::ok(
                0,
                json!({
                    "content": [{"type": "text", "text": "hi"}],
                    "isError": false
                }),
            ),
        );
        let mut client = client_with(transport);
        let result = client.call_tool("echo", json!({"x": 1})).await.unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);

        // The outgoing `tools/call` request must carry the exact tool name and
        // arguments passed in. A regression that sent the wrong name or dropped
        // the arguments would fail here.
        let reqs = reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        let (method, params) = &reqs[0];
        assert_eq!(method, "tools/call");
        assert_eq!(
            params.as_ref().expect("tools/call params present"),
            &json!({ "name": "echo", "arguments": { "x": 1 } }),
        );
    }

    #[tokio::test]
    // A request that outlives the request timeout fails with a Timeout error
    // rather than hanging the caller. The transport sleeps far longer than the
    // configured 50ms budget, so `request_with_timeout` must trip first.
    async fn request_times_out() {
        let transport = MockTransport {
            request_delay: Some(Duration::from_secs(60)),
            ..MockTransport::default()
        };
        transport.push("tools/list", MockTransport::ok(0, json!({"tools": []})));
        let mut client = client_with(transport);
        client.request_timeout = Duration::from_millis(50);

        let err = client.list_tools().await.unwrap_err();
        assert!(matches!(err, McpError::Timeout { .. }));
    }
}
