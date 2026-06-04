use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use code_agent_mcp::{
    McpClient, McpError, McpLimits, McpServerConfig, StreamableHttpMcpServerConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum Mode {
    Json,
    Sse,
    Timeout,
    Error,
    /// Stateful server: assigns an `Mcp-Session-Id` on the first response and
    /// 404s any later request that does not echo it back.
    Session,
}

async fn start_server(mode: Mode) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let headers_seen = Arc::new(Mutex::new(Vec::new()));
    let seen = headers_seen.clone();
    // Server-side record of the session id handed out in Session mode.
    let assigned_session: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let seen = seen.clone();
            let assigned_session = assigned_session.clone();
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
                seen.lock().unwrap().push(headers.clone());
                let incoming_session = headers.lines().find_map(|line| {
                    let (k, v) = line.split_once(':')?;
                    k.eq_ignore_ascii_case("mcp-session-id")
                        .then(|| v.trim().to_string())
                });
                if matches!(mode, Mode::Session) {
                    // Once a session is assigned, every request must echo it.
                    let expected = assigned_session.lock().unwrap().clone();
                    if let Some(expected) = expected {
                        if incoming_session.as_deref() != Some(expected.as_str()) {
                            let _ = write_raw(
                                &mut stream,
                                "HTTP/1.1 404 Not Found\r\nContent-Length: 7\r\nConnection: close\r\n\r\nmissing",
                            )
                            .await;
                            return;
                        }
                    }
                }
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
                if matches!(mode, Mode::Timeout) {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    return;
                }
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if request.get("id").is_none() {
                    let _ = write_response(&mut stream, "application/json", "{}").await;
                    return;
                }
                if matches!(mode, Mode::Error) {
                    let _ = write_raw(
                        &mut stream,
                        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nfail",
                    )
                    .await;
                    return;
                }
                let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let payload = match method {
                    "initialize" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "http-fake", "version": "0.1.0"}
                        }
                    }),
                    "tools/list" => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "search",
                                "description": "Search",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    }),
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                };
                let body = payload.to_string();
                if matches!(mode, Mode::Session) && method == "initialize" {
                    // Assign and return a session id on the initialize response.
                    let session = "sess-abc-123".to_string();
                    *assigned_session.lock().unwrap() = Some(session.clone());
                    let _ = write_raw(
                        &mut stream,
                        &format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: {session}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        ),
                    )
                    .await;
                } else if matches!(mode, Mode::Sse) {
                    let event = format!("event: message\ndata: {body}\n\n");
                    let _ = write_response(&mut stream, "text/event-stream", &event).await;
                } else {
                    let _ = write_response(&mut stream, "application/json", &body).await;
                }
            });
        }
    });
    (format!("http://{addr}/mcp"), headers_seen)
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    write_raw(
        stream,
        &format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

async fn write_raw(stream: &mut tokio::net::TcpStream, response: &str) -> std::io::Result<()> {
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn http_client(url: String, timeout: Duration) -> McpClient {
    let cfg = StreamableHttpMcpServerConfig {
        url,
        headers: BTreeMap::new(),
        startup_timeout: Some(timeout),
        request_timeout: Some(timeout),
    };
    McpClient::connect(
        "docs",
        &McpServerConfig::StreamableHttp(cfg),
        McpLimits::default(),
        Vec::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
// HTTP transport sends POST JSON-RPC and parses application/json responses.
async fn post_json_and_parse_application_json() {
    let (url, _) = start_server(Mode::Json).await;
    let mut client = http_client(url, Duration::from_secs(2)).await;
    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "search");
}

#[tokio::test]
// HTTP transport parses text/event-stream responses carrying JSON-RPC data.
async fn parses_sse_response() {
    let (url, _) = start_server(Mode::Sse).await;
    let mut client = http_client(url, Duration::from_secs(2)).await;
    let init = client.initialize().await.unwrap();
    assert_eq!(init.server_info.name, "http-fake");
}

#[tokio::test]
// HTTP transport includes the MCP protocol header after initialization.
async fn includes_protocol_version_header_after_negotiation() {
    let (url, headers) = start_server(Mode::Json).await;
    let mut client = http_client(url, Duration::from_secs(2)).await;
    client.initialize().await.unwrap();
    client.list_tools().await.unwrap();
    let joined = headers.lock().unwrap().join("\n").to_ascii_lowercase();
    assert!(joined.contains("mcp-protocol-version: 2025-06-18"));
}

#[tokio::test]
// HTTP request timeout returns a structured MCP timeout error.
async fn timeout_works() {
    let (url, _) = start_server(Mode::Timeout).await;
    let mut client = http_client(url, Duration::from_millis(50)).await;
    let err = client.initialize().await.unwrap_err();
    assert!(matches!(err, McpError::Timeout { .. }));
}

#[tokio::test]
// HTTP non-success status codes become structured MCP HTTP errors.
async fn http_error_is_structured() {
    let (url, _) = start_server(Mode::Error).await;
    let mut client = http_client(url, Duration::from_secs(2)).await;
    let err = client.initialize().await.unwrap_err();
    assert!(matches!(err, McpError::Http { status: 500, .. }));
}

#[tokio::test]
// A stateful server assigns an Mcp-Session-Id on initialize and 404s any
// later request that omits it; the transport must capture and resend it (#42).
async fn captures_and_resends_session_id() {
    let (url, headers) = start_server(Mode::Session).await;
    let mut client = http_client(url, Duration::from_secs(2)).await;
    client.initialize().await.unwrap();
    // Without resending the captured session id, this would 404.
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "search");

    let recorded = headers.lock().unwrap();
    // The initialize request has no session id yet; the follow-up tools/list
    // request must echo the assigned one.
    let last = recorded.last().unwrap().to_ascii_lowercase();
    assert!(
        last.contains("mcp-session-id: sess-abc-123"),
        "tools/list request must resend the session id, got:\n{last}"
    );
}

#[test]
// Redacted summaries hide Authorization headers.
fn authorization_header_is_redacted() {
    let options = code_agent_mcp::McpOptions {
        enabled: true,
        servers: BTreeMap::from([(
            "docs".into(),
            McpServerConfig::StreamableHttp(StreamableHttpMcpServerConfig {
                url: "http://127.0.0.1/mcp".into(),
                headers: BTreeMap::from([("Authorization".into(), "Bearer abc123".into())]),
                startup_timeout: None,
                request_timeout: None,
            }),
        )]),
        ..code_agent_mcp::McpOptions::default()
    };
    assert_eq!(
        options.redacted_summary()["servers"]["docs"]["headers"]["Authorization"],
        "[REDACTED]"
    );
}
