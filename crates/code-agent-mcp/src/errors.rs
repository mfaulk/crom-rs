use std::path::PathBuf;

use thiserror::Error;

use crate::protocol::{JsonRpcError, JsonRpcId};

pub type McpResult<T> = Result<T, McpError>;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("MCP config at {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("invalid MCP config: {0}")]
    InvalidConfig(String),
    #[error("missing required environment variable {var} in {field}")]
    MissingEnv { var: String, field: String },
    #[error("MCP transport error: {0}")]
    Transport(String),
    #[error("MCP server exited: {0}")]
    ServerExited(String),
    #[error("MCP request timed out after {seconds}s")]
    Timeout { seconds: u64 },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid JSON-RPC message: {0}")]
    InvalidJsonRpc(String),
    #[error("JSON-RPC error for request {id:?}: {error}")]
    JsonRpc {
        id: Option<JsonRpcId>,
        error: JsonRpcError,
    },
    #[error("HTTP MCP error: status {status}: {body}")]
    Http { status: u16, body: String },
    #[error("MCP server {server_name} failed: {message}")]
    ServerFailed {
        server_name: String,
        message: String,
    },
    #[error("MCP result exceeded limit: {actual} bytes > {limit} bytes")]
    ResultTooLarge { actual: usize, limit: usize },
}

impl McpError {
    pub fn config(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Self::Config {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn timeout(duration: std::time::Duration) -> Self {
        Self::Timeout {
            seconds: duration.as_secs(),
        }
    }
}
