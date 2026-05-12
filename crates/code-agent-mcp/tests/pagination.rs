use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use code_agent_mcp::{
    McpClient, McpLimits, McpServerConfig, StreamableHttpMcpServerConfig, MCP_MAX_LIST_PAGES,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Always returns a fresh, distinct `nextCursor` so a naive paginator
/// loops forever. The counter is observable to the test.
async fn start_endless_pager() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_task = calls.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let calls = calls_for_task.clone();
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
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                let id = request
                    .get("id")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if request.get("id").is_none() {
                    let _ = write_response(&mut stream, "application/json", "{}").await;
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
                            "serverInfo": {"name": "endless", "version": "0.1.0"}
                        }
                    }),
                    "tools/list" => {
                        let n = calls.fetch_add(1, Ordering::AcqRel);
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": {
                                "tools": [{
                                    "name": format!("t{n}"),
                                    "description": "endless",
                                    "inputSchema": {"type": "object"}
                                }],
                                "nextCursor": format!("page-{}", n + 1)
                            }
                        })
                    }
                    _ => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {}
                    }),
                };
                let body = payload.to_string();
                let _ = write_response(&mut stream, "application/json", &body).await;
            });
        }
    });
    (format!("http://{addr}/mcp"), calls)
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

async fn http_client(url: String) -> McpClient {
    let cfg = StreamableHttpMcpServerConfig {
        url,
        headers: BTreeMap::new(),
        startup_timeout: Some(Duration::from_secs(5)),
        request_timeout: Some(Duration::from_secs(5)),
    };
    McpClient::connect(
        "endless",
        &McpServerConfig::StreamableHttp(cfg),
        McpLimits::default(),
        Vec::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
// `tools/list` pagination terminates with an error rather than hanging
// when the server keeps advertising new cursors past MCP_MAX_LIST_PAGES.
async fn list_tools_caps_at_max_pages() {
    let (url, calls) = start_endless_pager().await;
    let mut client = http_client(url).await;
    client.initialize().await.unwrap();
    let err = client.list_tools().await.unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("exceeded") && msg.contains("pages"),
        "expected page-limit error, got: {msg}"
    );
    // The server should have been hit roughly `MCP_MAX_LIST_PAGES` times
    // — at minimum the cap, at most cap + a small grace.
    let n = calls.load(Ordering::Acquire);
    assert!(
        n >= MCP_MAX_LIST_PAGES && n <= MCP_MAX_LIST_PAGES + 2,
        "pagination should stop near {MCP_MAX_LIST_PAGES} pages, got {n}"
    );
}
