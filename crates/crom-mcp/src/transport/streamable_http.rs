use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::Url;
use serde_json::Value;

use crate::config::{redact_text_with_secrets, McpLimits, StreamableHttpMcpServerConfig};
use crate::errors::{McpError, McpResult};
use crate::protocol::{
    parse_json_rpc_value, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
};
use crate::transport::{McpTransport, McpTransportKind};

pub struct StreamableHttpMcpTransport {
    client: reqwest::Client,
    endpoint: Url,
    headers: HeaderMap,
    protocol_version: Option<String>,
    /// Session id returned by a stateful server on the `initialize`
    /// response via the `Mcp-Session-Id` header. Per the Streamable-HTTP
    /// spec the client MUST echo it back on every subsequent request, or
    /// stateful servers reject those requests with 404. Captured inside
    /// `post_json` (which takes `&self`), hence the interior mutability.
    session_id: std::sync::Mutex<Option<String>>,
    timeout: std::time::Duration,
    secrets: Vec<String>,
    /// Per-response byte cap. Once the body stream exceeds this we abort
    /// reading and return `ResultTooLarge` instead of materializing the
    /// full payload into memory.
    max_result_bytes: usize,
}

impl StreamableHttpMcpTransport {
    pub fn new(
        config: &StreamableHttpMcpServerConfig,
        limits: McpLimits,
        secrets: Vec<String>,
    ) -> McpResult<Self> {
        let endpoint = Url::parse(&config.url)
            .map_err(|e| McpError::InvalidConfig(format!("invalid MCP HTTP url: {e}")))?;
        let mut headers = HeaderMap::new();
        for (key, value) in &config.headers {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                McpError::InvalidConfig(format!("invalid MCP HTTP header name {key:?}: {e}"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|e| {
                McpError::InvalidConfig(format!("invalid MCP HTTP header value {key:?}: {e}"))
            })?;
            headers.insert(name, value);
        }
        let timeout = config.request_timeout.unwrap_or(limits.request_timeout);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| McpError::Transport(format!("failed to build HTTP client: {e}")))?;
        // Allow some slack above the structured `max_tool_result_bytes`
        // since the wire response is JSON-RPC-wrapped (envelope + base64
        // for binary content). The 4x factor mirrors the post-parse
        // sanity check in `enforce_non_binary_result_limit`.
        let max_result_bytes = limits
            .max_tool_result_bytes
            .saturating_mul(4)
            .max(64 * 1024);
        Ok(Self {
            client,
            endpoint,
            headers,
            protocol_version: None,
            session_id: std::sync::Mutex::new(None),
            timeout,
            secrets,
            max_result_bytes,
        })
    }

    pub fn redacted_headers(&self) -> Vec<(String, String)> {
        self.headers
            .iter()
            .map(|(k, v)| {
                let value = v.to_str().unwrap_or("[non-utf8]").to_string();
                let redacted = if crate::config::looks_like_secret_key(k.as_str()) {
                    "[REDACTED]".to_string()
                } else {
                    redact_text_with_secrets(&value, &self.secrets)
                };
                (k.as_str().to_string(), redacted)
            })
            .collect()
    }

    async fn post_json(
        &self,
        value: Value,
        expect_response: bool,
    ) -> McpResult<Option<JsonRpcResponse>> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&value)
            .timeout(self.timeout);
        if let Some(version) = &self.protocol_version {
            request = request.header("MCP-Protocol-Version", version);
        }
        // Echo the negotiated session id (if any) on every request so
        // stateful servers don't 404 follow-up calls.
        if let Some(session) = self
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                McpError::timeout(self.timeout)
            } else {
                McpError::Transport(format!("HTTP MCP request failed: {e}"))
            }
        })?;
        let status = response.status();
        // Capture the session id assigned by a stateful server (typically on
        // the `initialize` response). Only the first one is stored; later
        // responses don't reassign it.
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            let mut stored = self.session_id.lock().unwrap_or_else(|e| e.into_inner());
            if stored.is_none() {
                *stored = Some(session.to_string());
            }
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // Drain the body as a chunked stream so we can stop reading the
        // moment we exceed `max_result_bytes`. Buffering the full body up
        // front would let a multi-MB response materialize in memory before
        // the limit check ran.
        let limit = self.max_result_bytes;
        let mut byte_stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk.map_err(|e| {
                if e.is_timeout() {
                    McpError::timeout(self.timeout)
                } else {
                    McpError::Transport(format!("failed reading HTTP MCP response body: {e}"))
                }
            })?;
            if buf.len().saturating_add(chunk.len()) > limit {
                return Err(McpError::ResultTooLarge {
                    actual: buf.len().saturating_add(chunk.len()),
                    limit,
                });
            }
            buf.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(buf)
            .map_err(|e| McpError::Transport(format!("HTTP MCP response body not utf-8: {e}")))?;
        if !status.is_success() {
            return Err(McpError::Http {
                status: status.as_u16(),
                body: redact_text_with_secrets(&body, &self.secrets),
            });
        }
        if !expect_response || body.trim().is_empty() {
            return Ok(None);
        }
        if content_type.contains("text/event-stream") {
            // A streamable-HTTP server may emit notification/progress events
            // ahead of the actual JSON-RPC response, so scan every SSE event
            // and return the response rather than blindly taking the first.
            parse_sse_response(&body).map(Some)
        } else {
            let value = serde_json::from_str::<Value>(&body)?;
            match parse_json_rpc_value(value)? {
                JsonRpcMessage::Response(resp) => Ok(Some(resp)),
                other => Err(McpError::InvalidJsonRpc(format!(
                    "expected JSON-RPC response, got {other:?}"
                ))),
            }
        }
    }
}

#[async_trait]
impl McpTransport for StreamableHttpMcpTransport {
    fn kind(&self) -> McpTransportKind {
        McpTransportKind::StreamableHttp
    }

    async fn send_request(&mut self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        self.send_request_concurrent(request).await
    }

    // Each Streamable-HTTP request is an independent HTTP round-trip over a
    // cloneable `reqwest::Client`, and `post_json` already takes `&self`
    // (session-id capture uses interior mutability), so concurrent in-flight
    // requests are correlated by the HTTP layer rather than a shared reader.
    fn supports_concurrent_requests(&self) -> bool {
        true
    }

    async fn send_request_concurrent(&self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
        let value = serde_json::to_value(request)?;
        self.post_json(value, true)
            .await?
            .ok_or_else(|| McpError::InvalidJsonRpc("HTTP request returned no response".into()))
    }

    async fn send_notification(&mut self, notification: JsonRpcNotification) -> McpResult<()> {
        let value = serde_json::to_value(notification)?;
        let _ = self.post_json(value, false).await?;
        Ok(())
    }

    fn set_protocol_version(&mut self, version: Option<String>) {
        self.protocol_version = version;
    }
}

/// Iterate every SSE event in `body` and return the first one whose `data`
/// parses as a JSON-RPC *response* (as opposed to a leading notification or
/// progress event). Errors only if no event in the body is a response.
fn parse_sse_response(body: &str) -> McpResult<JsonRpcResponse> {
    let mut saw_event = false;
    let mut last_err: Option<McpError> = None;
    for data in sse_event_data(body) {
        saw_event = true;
        let value: Value = match serde_json::from_str(&data) {
            Ok(v) => v,
            Err(e) => {
                last_err = Some(McpError::from(e));
                continue;
            }
        };
        match parse_json_rpc_value(value) {
            Ok(JsonRpcMessage::Response(resp)) => return Ok(resp),
            Ok(_) => {}
            Err(e) => last_err = Some(e),
        }
    }
    if !saw_event {
        return Err(McpError::InvalidJsonRpc(
            "text/event-stream response contained no data".into(),
        ));
    }
    Err(last_err.unwrap_or_else(|| {
        McpError::InvalidJsonRpc(
            "text/event-stream response contained no JSON-RPC response event".into(),
        )
    }))
}

/// Split an SSE body into the concatenated `data:` payload of each event.
/// Events are delimited by blank lines; multi-line `data:` fields within an
/// event are joined with `\n` per the SSE spec.
fn sse_event_data(body: &str) -> Vec<String> {
    let mut events = Vec::new();
    let mut current = String::new();
    let mut has_data = false;
    for line in body.lines() {
        if line.trim().is_empty() {
            if has_data {
                events.push(std::mem::take(&mut current));
            }
            current.clear();
            has_data = false;
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            if has_data {
                current.push('\n');
            }
            current.push_str(rest.trim_start());
            has_data = true;
        }
    }
    if has_data {
        events.push(current);
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // A leading notification SSE event must not shadow the real response: the
    // parser scans all events and returns the JSON-RPC response.
    fn parse_sse_response_skips_leading_notification() {
        let body = "event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\
                    \n\
                    event: message\n\
                    data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\
                    \n";
        let resp = parse_sse_response(body).expect("response event must be selected");
        assert_eq!(resp.id, Some(crate::protocol::JsonRpcId::Number(7)));
        assert_eq!(resp.result, Some(serde_json::json!({"ok": true})));
    }

    #[test]
    // Multi-line data fields within a single event are joined per the SSE spec.
    fn parse_sse_response_joins_multiline_data() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":1,\n\
                    data: \"result\":{\"v\":2}}\n\
                    \n";
        let resp = parse_sse_response(body).expect("multiline response parses");
        assert_eq!(resp.id, Some(crate::protocol::JsonRpcId::Number(1)));
    }

    #[test]
    // A body containing only notifications yields an error, not a false response.
    fn parse_sse_response_errors_without_response() {
        let body = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n";
        assert!(parse_sse_response(body).is_err());
    }

    #[test]
    // An empty (no-event) SSE body is reported as "contained no data" rather
    // than a generic parse failure.
    fn parse_sse_response_errors_on_empty_body() {
        let err = parse_sse_response("").unwrap_err();
        match err {
            McpError::InvalidJsonRpc(msg) => assert!(msg.contains("no data"), "got: {msg}"),
            other => panic!("expected InvalidJsonRpc, got {other:?}"),
        }
    }

    #[test]
    // A body whose only event has malformed JSON propagates the parse error
    // (not the "no response event" message), since an event was seen.
    fn parse_sse_response_propagates_event_parse_error() {
        let body = "data: {not json}\n\n";
        let err = parse_sse_response(body).unwrap_err();
        // serde_json error becomes McpError::Json via From.
        assert!(matches!(err, McpError::Json(_)));
    }

    // ---- sse_event_data ----

    #[test]
    // Blank lines delimit events; each event's `data:` field is one entry.
    fn sse_event_data_splits_on_blank_lines() {
        let body = "data: a\n\ndata: b\n\n";
        assert_eq!(sse_event_data(body), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    // Multi-line `data:` fields within one event are newline-joined per spec.
    fn sse_event_data_joins_multiline_within_event() {
        let body = "data: line1\ndata: line2\n\n";
        assert_eq!(sse_event_data(body), vec!["line1\nline2".to_string()]);
    }

    #[test]
    // A trailing event without a closing blank line is still flushed.
    fn sse_event_data_flushes_unterminated_trailing_event() {
        let body = "data: only";
        assert_eq!(sse_event_data(body), vec!["only".to_string()]);
    }

    #[test]
    // Non-`data:` SSE fields (event:, id:, comments) are ignored, and an event
    // with no data field produces no entry.
    fn sse_event_data_ignores_non_data_fields() {
        let body = "event: message\nid: 1\n: keep-alive comment\n\n";
        assert!(sse_event_data(body).is_empty());
    }

    #[test]
    // Leading whitespace after `data:` is stripped (trim_start); trailing
    // whitespace is preserved.
    fn sse_event_data_strips_leading_whitespace() {
        let body = "data:  spaced \n\n";
        assert_eq!(sse_event_data(body), vec!["spaced ".to_string()]);
    }
}
