//! Core types, traits, and the agent runtime for code-agent.

pub mod agent;
pub mod builtin_hooks;
pub mod client;
pub mod conversation;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod options;
pub mod permission;
pub mod policy;
pub mod provider;
pub mod run_id;
pub mod settings;
pub mod telemetry;
pub mod todo;
pub mod tool;
pub mod tool_rule;
pub mod usage;

pub use agent::{query, Agent, AgentBuilder, SessionSink, TelemetrySink};
pub use builtin_hooks::{
    redact_secrets_in_value, AuditEntry, AuditLogHook, BlockDangerousBashHook,
    BlockEnvFileWritesHook, MaxPatchSizeHook, SecretRedactionHook,
};
pub use client::{AgentClient, AgentClientBuilder, SessionStart};
pub use conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
pub use errors::{AgentError, ProviderError, ToolError};
pub use events::{
    AgentEvent, AgentResult, HookEvaluated, HookOutcomeKind, InterruptedEvent, PolicyDecisionEvent,
    ResultSubtype, TodoUpdate, ToolCallCompleted, ToolCallDenied, ToolCallStarted, UserToolResult,
};
pub use hooks::{
    aggregate, Hook, HookAddedContext, HookAggregateDecision, HookCallRecord, HookError,
    HookEventName, HookInput, HookMatcher, HookOutput, Hooks, StopReason, ToolMatcher,
};
pub use options::{AgentOptions, SessionId};
pub use permission::{
    ApprovalMemoryMode, PermissionCallback, PermissionDecision, PermissionKind, PermissionMode,
    PermissionRequest,
};
pub use policy::{
    approval_key, ApprovalMemory, ApprovalMemorySnapshot, MemoryDecision, PolicyConfig,
    PolicyDecision, PolicyDecisionSource, PolicyEngine, PolicyOutcome, PolicyToolContext,
};
pub use provider::{ModelEvent, ModelProvider, ModelRequest};
pub use run_id::RunId;
pub use settings::{
    HooksConfig, ProjectInstructionsMode, SettingSource, Settings, SettingsError, SettingsFile,
    ToolLimitsConfig,
};
pub use telemetry::{AgentTelemetryEvent, ModelPricing, SessionLoadKind, TelemetryOutcome};
pub use todo::{TodoError, TodoItem, TodoList, TodoStatus};
pub use tool::{Tool, ToolContent, ToolContext, ToolRegistry, ToolResult};
pub use tool_rule::ToolRule;
pub use usage::TokenUsage;
