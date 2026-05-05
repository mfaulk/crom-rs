//! Core types, traits, and the agent loop for code-agent.

pub mod agent;
pub mod conversation;
pub mod errors;
pub mod events;
pub mod options;
pub mod permission;
pub mod provider;
pub mod tool;
pub mod usage;

pub use agent::{query, Agent, AgentBuilder};
pub use conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
pub use errors::{AgentError, ProviderError, ToolError};
pub use events::{
    AgentEvent, AgentResult, ResultSubtype, ToolCallCompleted, ToolCallDenied, ToolCallStarted,
    UserToolResult,
};
pub use options::{AgentOptions, SessionId};
pub use permission::{
    PermissionCallback, PermissionDecision, PermissionKind, PermissionMode, PermissionRequest,
};
pub use provider::{ModelEvent, ModelProvider, ModelRequest};
pub use tool::{Tool, ToolContent, ToolContext, ToolRegistry, ToolResult};
pub use usage::TokenUsage;
