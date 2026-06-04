pub mod stdio;
pub mod streamable_http;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::McpResult;
use crate::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcResponse};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpTransportKind {
    Stdio,
    StreamableHttp,
}

impl std::fmt::Display for McpTransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => f.write_str("stdio"),
            Self::StreamableHttp => f.write_str("http"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpLogStream {
    Stderr,
    Stdout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerLogEntry {
    pub stream: McpLogStream,
    pub message: String,
    pub truncated: bool,
}

#[async_trait]
pub trait McpTransport: Send {
    fn kind(&self) -> McpTransportKind;

    async fn send_request(&mut self, request: JsonRpcRequest) -> McpResult<JsonRpcResponse>;

    async fn send_notification(&mut self, notification: JsonRpcNotification) -> McpResult<()>;

    fn set_protocol_version(&mut self, version: Option<String>);

    fn drain_logs(&mut self) -> Vec<McpServerLogEntry> {
        Vec::new()
    }

    async fn shutdown(&mut self) -> McpResult<()> {
        Ok(())
    }
}

pub fn response_value(response: JsonRpcResponse) -> McpResult<Value> {
    if let Some(error) = response.error {
        return Err(crate::errors::McpError::JsonRpc {
            id: response.id,
            error,
        });
    }
    response
        .result
        .ok_or_else(|| crate::errors::McpError::InvalidJsonRpc("response missing result".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::McpError;
    use crate::protocol::{JsonRpcError, JsonRpcId, JsonRpcResponse};

    fn resp(result: Option<Value>, error: Option<JsonRpcError>) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: Some(JsonRpcId::Number(1)),
            result,
            error,
        }
    }

    #[test]
    // A success response yields its `result` value.
    fn response_value_returns_result() {
        let v = response_value(resp(Some(serde_json::json!({"ok": 1})), None)).unwrap();
        assert_eq!(v, serde_json::json!({"ok": 1}));
    }

    #[test]
    // A response carrying a JSON-RPC error is surfaced as McpError::JsonRpc,
    // preserving both the request id and the error code/message.
    fn response_value_surfaces_jsonrpc_error() {
        let err = response_value(resp(
            None,
            Some(JsonRpcError {
                code: -32601,
                message: "method not found".into(),
                data: None,
            }),
        ))
        .unwrap_err();
        match err {
            McpError::JsonRpc { id, error } => {
                assert_eq!(id, Some(JsonRpcId::Number(1)));
                assert_eq!(error.code, -32601);
            }
            other => panic!("expected JsonRpc error, got {other:?}"),
        }
    }

    #[test]
    // A response with neither result nor error is an InvalidJsonRpc protocol
    // violation, not a silent empty success.
    fn response_value_rejects_missing_result() {
        let err = response_value(resp(None, None)).unwrap_err();
        assert!(matches!(err, McpError::InvalidJsonRpc(_)));
    }

    #[test]
    // The transport kind renders to the wire-stable short forms used in logs
    // and status output.
    fn transport_kind_display() {
        assert_eq!(McpTransportKind::Stdio.to_string(), "stdio");
        assert_eq!(McpTransportKind::StreamableHttp.to_string(), "http");
    }

    #[test]
    // The transport kind serializes snake_case so persisted status JSON stays
    // stable across versions.
    fn transport_kind_serde_roundtrip() {
        let v = serde_json::to_value(McpTransportKind::StreamableHttp).unwrap();
        assert_eq!(v, serde_json::json!("streamable_http"));
        let back: McpTransportKind = serde_json::from_value(v).unwrap();
        assert_eq!(back, McpTransportKind::StreamableHttp);
    }

    #[test]
    // The log-stream discriminant serializes snake_case for stable log records.
    fn log_stream_serde() {
        assert_eq!(
            serde_json::to_value(McpLogStream::Stderr).unwrap(),
            serde_json::json!("stderr")
        );
        assert_eq!(
            serde_json::to_value(McpLogStream::Stdout).unwrap(),
            serde_json::json!("stdout")
        );
    }
}
