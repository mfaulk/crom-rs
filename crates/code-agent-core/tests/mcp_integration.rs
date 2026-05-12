use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use code_agent_core::{
    query, AgentEvent, AgentOptions, McpOptions, ModelEvent, ModelProvider, ModelRequest,
    PermissionMode, ProviderError, ResultSubtype, StdioMcpServerConfig, ToolRegistry, ToolRule,
};
use futures::stream::{self, BoxStream, StreamExt};

fn fake_mcp_options() -> McpOptions {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../code-agent-mcp/tests/fixtures/fake-mcp-server/Cargo.toml");
    McpOptions {
        enabled: true,
        servers: BTreeMap::from([(
            "fake".into(),
            code_agent_mcp::McpServerConfig::Stdio(StdioMcpServerConfig {
                command: "cargo".into(),
                args: vec![
                    "run".into(),
                    "--quiet".into(),
                    "--manifest-path".into(),
                    manifest.display().to_string(),
                ],
                env: BTreeMap::new(),
                cwd: None,
                startup_timeout: Some(Duration::from_secs(5)),
                request_timeout: Some(Duration::from_secs(5)),
            }),
        )]),
        ..McpOptions::default()
    }
}

#[derive(Clone)]
struct CapturingProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    tool_names: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            tool_names: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelProvider for CapturingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        *self.tool_names.lock().unwrap() = req.tools.iter().map(|t| t.name.clone()).collect();
        let events = self.turns.lock().unwrap().remove(0);
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

#[tokio::test]
// Discovered MCP tools are registered with `mcp__server__tool` names.
async fn discovered_mcp_tools_register_as_agent_tools() {
    let provider = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let options = AgentOptions {
        mcp: fake_mcp_options(),
        allowed_tools: vec!["mcp__fake__list_issues".into()],
        allowed_tool_rules: vec![ToolRule::ExactTool("mcp__fake__list_issues".into())],
        max_turns: Some(1),
        ..AgentOptions::default()
    };
    let stream = query("hello", options, provider.clone(), ToolRegistry::new())
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(provider
        .tool_names
        .lock()
        .unwrap()
        .contains(&"mcp__fake__list_issues".to_string()));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::McpServerConnected(_))));
}

#[tokio::test]
// MCP tool calls flow through the normal agent loop and return tool results.
async fn mcp_tool_call_runs_through_agent_loop() {
    let provider = Arc::new(CapturingProvider::new(vec![
        vec![
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "toolu_1".into(),
                name: "mcp__fake__list_issues".into(),
                input: serde_json::json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta {
                text: "summarized".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let options = AgentOptions {
        mcp: fake_mcp_options(),
        allowed_tools: vec!["mcp__fake__list_issues".into()],
        allowed_tool_rules: vec![ToolRule::ExactTool("mcp__fake__list_issues".into())],
        permission_mode: PermissionMode::Default,
        max_turns: Some(3),
        ..AgentOptions::default()
    };
    let stream = query("use mcp", options, provider, ToolRegistry::new())
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::UserToolResult(r)
                if r.tool_name == "mcp__fake__list_issues"
                    && r.content.contains("Parser trims too late")
        )
    }));
    assert!(events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Result(r) if matches!(r.subtype, ResultSubtype::Success)
        )
    }));
}

/// Tiny MCP server that injects a synthetic init delay so the test can
/// measure whether discovery runs in parallel.
async fn start_slow_mcp(init_delay: Duration) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut header_bytes = Vec::new();
                let mut buf = [0u8; 1];
                while stream.read_exact(&mut buf).await.is_ok() {
                    header_bytes.push(buf[0]);
                    if header_bytes.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8_lossy(&header_bytes).to_string();
                let len = headers
                    .lines()
                    .find_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let _ = stream.read_exact(&mut body).await;
                let request: serde_json::Value = match serde_json::from_slice(&body) {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let payload = match method {
                    "initialize" => {
                        tokio::time::sleep(init_delay).await;
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "protocolVersion": "2025-06-18",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "slow", "version": "0.1.0"}
                            }
                        })
                    }
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"tools": [
                            {"name": "noop", "description": "", "inputSchema": {"type": "object"}}
                        ]}
                    }),
                    _ => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                };
                if request.get("id").is_none() {
                    let resp =
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                    let _ = stream.write_all(resp).await;
                    return;
                }
                let body = payload.to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[tokio::test]
// MCP server discovery runs in parallel: with three servers that each
// add a 500ms init delay, the registration phase finishes in well under
// 3*500ms (the sequential bound).
async fn mcp_discovery_runs_in_parallel() {
    let delay = Duration::from_millis(500);
    let urls = [
        start_slow_mcp(delay).await,
        start_slow_mcp(delay).await,
        start_slow_mcp(delay).await,
    ];

    let mut servers = BTreeMap::new();
    for (i, url) in urls.iter().enumerate() {
        servers.insert(
            format!("s{i}"),
            code_agent_mcp::McpServerConfig::StreamableHttp(
                code_agent_mcp::StreamableHttpMcpServerConfig {
                    url: url.clone(),
                    headers: BTreeMap::new(),
                    startup_timeout: Some(Duration::from_secs(5)),
                    request_timeout: Some(Duration::from_secs(5)),
                },
            ),
        );
    }
    let options = McpOptions {
        enabled: true,
        servers,
        ..McpOptions::default()
    };

    let mut registry = ToolRegistry::new();
    let started = Instant::now();
    let report = code_agent_core::register_mcp_tools(&mut registry, &options).await;
    let elapsed = started.elapsed();

    assert_eq!(report.statuses.len(), 3);
    assert!(
        report
            .statuses
            .iter()
            .all(|s| matches!(s.status, code_agent_mcp::McpConnectionStatus::Connected)),
        "all servers should connect, got: {:?}",
        report
            .statuses
            .iter()
            .map(|s| (&s.name, &s.status, &s.error))
            .collect::<Vec<_>>()
    );
    // Sequential would be ~1500ms. Parallel completes in ~500ms with
    // some overhead. The generous 1100ms ceiling tolerates CI jitter
    // while still strongly distinguishing the two regimes.
    assert!(
        elapsed < Duration::from_millis(1100),
        "discovery must run in parallel: elapsed={elapsed:?}"
    );
}

#[tokio::test]
// When MCP discovery fails (initialize returns malformed stdout), the
// stderr drained from the failing server is surfaced via
// `report.logs` so operators can diagnose the failure. The parallel
// discovery path previously dropped these logs on the floor, which was
// a regression vs. the sequential path.
async fn failed_discovery_surfaces_drained_logs() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../code-agent-mcp/tests/fixtures/fake-mcp-server/Cargo.toml");
    let mut env = BTreeMap::new();
    env.insert("FAKE_MCP_MODE".into(), "invalid-json-with-stderr".into());
    let options = McpOptions {
        enabled: true,
        servers: BTreeMap::from([(
            "broken".into(),
            code_agent_mcp::McpServerConfig::Stdio(StdioMcpServerConfig {
                command: "cargo".into(),
                args: vec![
                    "run".into(),
                    "--quiet".into(),
                    "--manifest-path".into(),
                    manifest.display().to_string(),
                ],
                env,
                cwd: None,
                startup_timeout: Some(Duration::from_secs(5)),
                request_timeout: Some(Duration::from_secs(5)),
            }),
        )]),
        ..McpOptions::default()
    };

    let mut registry = ToolRegistry::new();
    let report = code_agent_core::register_mcp_tools(&mut registry, &options).await;

    assert_eq!(report.statuses.len(), 1);
    assert!(
        matches!(
            report.statuses[0].status,
            code_agent_mcp::McpConnectionStatus::Failed
        ),
        "discovery against an invalid-json server should fail: {:?}",
        report.statuses[0]
    );
    assert!(
        report.logs.iter().any(|(name, entry)| name == "broken"
            && entry.message.contains("synthetic discovery failure log")),
        "failure-path logs must propagate to the report; got: {:?}",
        report.logs
    );
}
