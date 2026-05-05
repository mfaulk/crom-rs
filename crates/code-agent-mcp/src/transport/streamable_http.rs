use async_trait::async_trait;
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
    timeout: std::time::Duration,
    secrets: Vec<String>,
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
        Ok(Self {
            client,
            endpoint,
            headers,
            protocol_version: None,
            timeout,
            secrets,
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
        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                McpError::timeout(self.timeout)
            } else {
                McpError::Transport(format!("HTTP MCP request failed: {e}"))
            }
        })?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response.text().await.map_err(|e| {
            McpError::Transport(format!("failed reading HTTP MCP response body: {e}"))
        })?;
        if !status.is_success() {
            return Err(McpError::Http {
                status: status.as_u16(),
                body: redact_text_with_secrets(&body, &self.secrets),
            });
        }
        if !expect_response || body.trim().is_empty() {
            return Ok(None);
        }
        let value = if content_type.contains("text/event-stream") {
            parse_sse_json(&body)?
        } else {
            serde_json::from_str::<Value>(&body)?
        };
        match parse_json_rpc_value(value)? {
            JsonRpcMessage::Response(resp) => Ok(Some(resp)),
            other => Err(McpError::InvalidJsonRpc(format!(
                "expected JSON-RPC response, got {other:?}"
            ))),
        }
    }
}

#[async_trait]
impl McpTransport for StreamableHttpMcpTransport {
    fn kind(&self) -> McpTransportKind {
        McpTransportKind::StreamableHttp
    }

    async fn send_request(&mut self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse> {
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

fn parse_sse_json(body: &str) -> McpResult<Value> {
    let mut data = String::new();
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.trim_start());
        }
        if line.trim().is_empty() && !data.trim().is_empty() {
            break;
        }
    }
    if data.trim().is_empty() {
        return Err(McpError::InvalidJsonRpc(
            "text/event-stream response contained no data".into(),
        ));
    }
    serde_json::from_str(&data).map_err(McpError::from)
}
