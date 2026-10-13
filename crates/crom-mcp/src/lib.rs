pub mod client;
pub mod config;
pub mod errors;
pub mod lifecycle;
pub mod prompts;
pub mod protocol;
pub mod registry;
pub mod resources;
pub mod tools;
pub mod transport;

pub use client::{McpClient, McpServerInit, MCP_MAX_LIST_PAGES};
pub use config::{
    load_mcp_options, merge_mcp_options, redact_json_value, redact_text_with_secrets,
    redact_value_with_secrets, McpConfigFile, McpExternalEffectPolicy, McpLimits, McpOptions,
    McpServerConfig, McpSettingsConfig, StdioMcpServerConfig, StreamableHttpMcpServerConfig,
};
pub use errors::{McpError, McpResult};
pub use lifecycle::{
    McpConnectionStatus, McpServerConnected, McpServerConnecting, McpServerFailed, McpServerLog,
    McpServerStatus, McpToolCallCompleted, McpToolCallFailed, McpToolCallStarted,
    McpToolsDiscovered,
};
pub use prompts::McpPrompt;
pub use protocol::{
    ClientCapabilities, ClientInfo, InitializeRequest, InitializeResponse, JsonRpcError, JsonRpcId,
    JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, McpContent,
    ServerCapabilities, ServerInfo,
};
pub use registry::{
    agent_tool_name, is_identifier_safe, mutating_tool_name, parse_agent_tool_name,
    McpRegisteredTool, McpToolNameParts, McpToolSummary,
};
pub use resources::{McpResource, McpResourceContents};
pub use tools::{mcp_result_to_text_and_metadata, McpToolDefinition, McpToolResult};
pub use transport::{McpLogStream, McpServerLogEntry, McpTransportKind};
