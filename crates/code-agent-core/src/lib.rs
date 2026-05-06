//! Core types, traits, and the agent runtime for code-agent.

pub mod agent;
pub mod agents;
pub mod builtin_hooks;
pub mod checkpoint;
pub mod client;
pub mod conversation;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod mcp;
pub mod options;
pub mod permission;
pub mod policy;
pub mod provider;
pub mod run_id;
pub mod settings;
pub mod telemetry;
pub mod todo;
pub mod tool;
pub mod tool_catalog;
pub mod tool_rule;
pub mod usage;

pub use agent::{query, Agent, AgentBuilder, SessionSink, TelemetrySink};
pub use agents::{
    builtin_starter_definitions, filesystem_agents_dir, load_filesystem_agents,
    load_filesystem_agents_strict, AgentConcurrencyHint, AgentDefinition, AgentDefinitionError,
    AgentDefinitionSummary, AgentOutputMode, AgentRegistry, AgentRuntimeOptions, AgentTool,
    AgentToolInput, AgentToolResult, ChildSessionMetadata, ChildSessionSinkFactory,
    SubagentCheckpointPolicy, SubagentContextPolicy, SubagentEventVisibility, SubagentRunHandle,
    SubagentRuntime, SubagentStatus, SUBAGENT_TOOL_NAME,
};
pub use builtin_hooks::{
    redact_secrets_in_value, AuditEntry, AuditLogHook, BlockDangerousBashHook,
    BlockEnvFileWritesHook, BlockMcpServerHook, BlockMcpToolNameHook, BlockMcpWriteLikeToolHook,
    MaxPatchSizeHook, McpAuditLogHook, McpResultSizeLimitHook, McpSecretRedactionHook,
    SecretRedactionHook,
};
pub use checkpoint::{
    CheckpointApi, CheckpointBashAction, CheckpointCaptureResult, CheckpointHandle,
};
pub use client::{AgentClient, AgentClientBuilder, AgentEventStream, SessionStart};
pub use code_agent_mcp::{
    McpConnectionStatus, McpExternalEffectPolicy, McpLimits, McpOptions, McpServerConfig,
    McpServerConnected, McpServerConnecting, McpServerFailed, McpServerLog, McpServerStatus,
    McpSettingsConfig, McpToolCallCompleted, McpToolCallFailed, McpToolCallStarted, McpToolSummary,
    McpToolsDiscovered, McpTransportKind, StdioMcpServerConfig, StreamableHttpMcpServerConfig,
};
pub use conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
pub use errors::{AgentError, ProviderError, ToolError};
pub use events::{
    AgentEvent, AgentRegistryLoaded, AgentResult, CheckpointEventPayload, HookEvaluated,
    HookOutcomeKind, InterruptedEvent, PolicyDecisionEvent, ResultSubtype, SubagentCancelled,
    SubagentEvent as SubagentForwardedEvent, SubagentFailed, SubagentStart, SubagentStop,
    SubagentTimedOut, TodoUpdate, ToolCallCompleted, ToolCallDenied, ToolCallStarted,
    ToolCatalogBuilt, ToolContextBudgetUpdated, ToolDeferred, ToolDisabled, ToolLoaded,
    ToolLoadingModeResolved, ToolSearchCompleted, ToolSearchFailed, ToolSearchStarted,
    UserToolResult,
};
pub use hooks::{
    aggregate, Hook, HookAddedContext, HookAggregateDecision, HookCallRecord, HookError,
    HookEventName, HookInput, HookMatcher, HookOutput, Hooks, StopReason, ToolMatcher,
};
pub use mcp::{mcp_completion_from_result, mcp_failure_from_error, register_mcp_tools, McpTool};
pub use options::{is_valid_session_id_str, AgentOptions, SessionId};
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
    AgentsSettingsConfig, CheckpointRetentionConfig, CheckpointingConfig, HooksConfig,
    ProjectInstructionsMode, SettingSource, Settings, SettingsError, SettingsFile,
    ToolLimitsConfig, ValidationConfig,
};
pub use telemetry::{AgentTelemetryEvent, ModelPricing, SessionLoadKind, TelemetryOutcome};
pub use todo::{TodoError, TodoItem, TodoList, TodoStatus};
pub use tool::{Tool, ToolContent, ToolContext, ToolRegistry, ToolResult};
pub use tool_catalog::{
    LoadedToolState, NativeToolSearchMode, ResolvedToolLoadingMode, SearchToolsDefinitionTool,
    SearchToolsTool, ToolCatalog, ToolCatalogBuilder, ToolCatalogSummary, ToolCategorySummary,
    ToolContextBudget, ToolDescriptor, ToolDescriptorPreview, ToolListFilter, ToolLoadPolicy,
    ToolLoadReason, ToolLoadingDecision, ToolLoadingMode, ToolRiskProfile, ToolSearchError,
    ToolSearchMatch, ToolSearchMatchSummary, ToolSearchOptions, ToolSearchProviderConfig,
    ToolSearchQuery, ToolSearchResult, ToolSearchRuntime, ToolSearchSettingsConfig,
    ToolSearchStrategy, ToolSource, ToolSourceFilter, ToolVisibility, ToolVisibilityContext,
    SEARCH_TOOLS_NAME,
};
pub use tool_rule::ToolRule;
pub use usage::TokenUsage;
