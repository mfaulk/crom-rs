use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use code_agent_mcp::{
    McpClient, McpError, McpLimits, McpServerConfig, StreamableHttpMcpServerConfig,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a tiny HTTP server that always responds with a body of
/// `body_len` bytes. The atomic returned tracks how many bytes were
/// actually written before the connection was closed — useful to verify
/// the client stopped reading early.
async fn start_oversize_server(
    body_len: usize,
) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bytes_sent = Arc::new(AtomicUsize::new(0));
    let sent = bytes_sent.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let sent = sent.clone();
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
                if request.get("id").is_none() {
                    let resp = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
                    let _ = stream.write_all(resp).await;
                    return;
                }
                let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
                if method == "initialize" {
                    let payload = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request.get("id"),
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"tools": {}},
                            "serverInfo": {"name": "big", "version": "0.1.0"}
                        }
                    })
                    .to_string();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(), payload
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    return;
                }
                // For any other method (e.g. tools/call): send an enormous
                // body. Stream chunks so the test can observe that the
                // client tears the connection down before the full body
                // is sent.
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body_len
                );
                if stream.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                let chunk_size = 4096usize;
                let chunk = vec![b'x'; chunk_size];
                let mut remaining = body_len;
                while remaining > 0 {
                    let n = remaining.min(chunk_size);
                    if stream.write_all(&chunk[..n]).await.is_err() {
                        break;
                    }
                    sent.fetch_add(n, Ordering::AcqRel);
                    remaining -= n;
                }
                let _ = stream.shutdown().await;
            });
        }
    });
    (format!("http://{addr}/mcp"), bytes_sent)
}

#[tokio::test]
// `tools/call` returns `ResultTooLarge` once the body stream exceeds the
// configured per-result cap, without buffering the full response in
// memory. The server-side byte counter must record fewer bytes than the
// full advertised body length.
async fn http_response_exceeding_limit_returns_result_too_large() {
    // Make `max_tool_result_bytes` small so the streaming cap (4x) is
    // well below the 4 MiB body we plan to send.
    let limits = McpLimits {
        max_tool_result_bytes: 16 * 1024,
        ..McpLimits::default()
    };
    let body_len = 4 * 1024 * 1024; // 4 MiB
    let (url, sent) = start_oversize_server(body_len).await;

    let cfg = StreamableHttpMcpServerConfig {
        url,
        headers: BTreeMap::new(),
        startup_timeout: Some(Duration::from_secs(5)),
        request_timeout: Some(Duration::from_secs(5)),
    };
    let mut client = McpClient::connect(
        "big",
        &McpServerConfig::StreamableHttp(cfg),
        limits,
        Vec::new(),
    )
    .await
    .unwrap();
    client.initialize().await.unwrap();

    let err = client
        .call_tool("anything", serde_json::json!({}))
        .await
        .unwrap_err();
    let too_large = matches!(err, McpError::ResultTooLarge { .. });
    assert!(too_large, "expected ResultTooLarge, got: {err:?}");

    // The server-side byte counter must be smaller than the full body,
    // confirming the client tore the connection down rather than reading
    // the full 4 MiB.
    let n = sent.load(Ordering::Acquire);
    assert!(
        n < body_len,
        "client should stop reading once limit is exceeded; server wrote {n} of {body_len} bytes"
    );
}
