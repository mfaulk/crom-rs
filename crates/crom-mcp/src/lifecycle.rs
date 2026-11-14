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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // The connection-status enum serializes snake_case so persisted/streamed
    // status JSON stays stable.
    fn connection_status_serde() {
        for (variant, wire) in [
            (McpConnectionStatus::Disabled, "disabled"),
            (McpConnectionStatus::Connecting, "connecting"),
            (McpConnectionStatus::Connected, "connected"),
            (McpConnectionStatus::Failed, "failed"),
        ] {
            assert_eq!(
                serde_json::to_value(variant).unwrap(),
                serde_json::json!(wire)
            );
            let back: McpConnectionStatus =
                serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(back, variant);
        }
    }

    #[test]
    // A status payload omitting the optional count/info/error fields still
    // deserializes, defaulting those fields to None (forward/back compat).
    fn server_status_defaults_optional_fields() {
        let status: McpServerStatus = serde_json::from_value(serde_json::json!({
            "name": "srv",
            "status": "connected",
            "transport": "stdio",
            "tool_count": 3
        }))
        .unwrap();
        assert_eq!(status.name, "srv");
        assert_eq!(status.status, McpConnectionStatus::Connected);
        assert_eq!(status.transport, McpTransportKind::Stdio);
        assert_eq!(status.tool_count, 3);
        assert!(status.protocol_version.is_none());
        assert!(status.resource_count.is_none());
        assert!(status.prompt_count.is_none());
        assert!(status.server_info.is_none());
        assert!(status.error.is_none());
    }

    #[test]
    // The full status round-trips through JSON without losing fields.
    fn server_status_roundtrip_preserves_fields() {
        let status = McpServerStatus {
            name: "srv".into(),
            status: McpConnectionStatus::Failed,
            transport: McpTransportKind::StreamableHttp,
            protocol_version: Some("2025-06-18".into()),
            tool_count: 0,
            resource_count: Some(2),
            prompt_count: Some(1),
            server_info: Some(ServerInfo {
                name: "x".into(),
                version: "1".into(),
            }),
            error: Some("boom".into()),
        };
        let json = serde_json::to_value(&status).unwrap();
        let back: McpServerStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back.status, McpConnectionStatus::Failed);
        assert_eq!(back.transport, McpTransportKind::StreamableHttp);
        assert_eq!(back.resource_count, Some(2));
        assert_eq!(back.error.as_deref(), Some("boom"));
    }
}
