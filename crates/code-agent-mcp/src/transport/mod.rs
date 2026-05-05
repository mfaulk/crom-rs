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
