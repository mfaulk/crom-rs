use serde::{Deserialize, Serialize};

use crate::protocol::ServerInfo;
use crate::registry::McpToolSummary;
use crate::transport::{McpLogStream, McpTransportKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: McpConnectionStatus,
    pub transport: McpTransportKind,
    #[serde(default)]
    pub protocol_version: Option<String>,
    pub tool_count: usize,
    #[serde(default)]
    pub resource_count: Option<usize>,
    #[serde(default)]
    pub prompt_count: Option<usize>,
    #[serde(default)]
    pub server_info: Option<ServerInfo>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpConnectionStatus {
    Disabled,
    Connecting,
    Connected,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConnecting {
    pub server_name: String,
    pub transport: McpTransportKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConnected {
    pub server_name: String,
    pub transport: McpTransportKind,
    pub protocol_version: String,
    pub tool_count: usize,
    #[serde(default)]
    pub resource_count: Option<usize>,
    #[serde(default)]
    pub prompt_count: Option<usize>,
    #[serde(default)]
    pub server_info: Option<ServerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerFailed {
    pub server_name: String,
    pub transport: McpTransportKind,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolsDiscovered {
    pub server_name: String,
    pub tools: Vec<McpToolSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallStarted {
    pub server_name: String,
    pub remote_tool_name: String,
    pub agent_tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallCompleted {
    pub server_name: String,
    pub remote_tool_name: String,
    pub agent_tool_name: String,
    pub duration_ms: u64,
    pub is_error: bool,
    pub result_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolCallFailed {
    pub server_name: String,
    pub remote_tool_name: String,
    pub agent_tool_name: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerLog {
    pub server_name: String,
    pub stream: McpLogStream,
    pub message: String,
    pub truncated: bool,
}
