//! Core types, traits, and the agent runtime for code-agent.

pub mod agent;
pub mod agents;
pub mod builtin_hooks;
pub mod checkpoint;
pub mod client;
pub mod compaction;
pub mod conversation;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod mcp;
pub mod options;
pub mod permission;
pub mod plugins;
pub mod policy;
pub mod provider;
pub mod run_id;
pub mod settings;
pub mod skills;
pub mod slash;
pub mod telemetry;
pub mod todo;
pub mod tool;
pub mod tool_catalog;
pub mod tool_rule;
pub mod ui;
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
pub use compaction::{
    compact_conversation, compact_conversation_with_summarizer, deterministic_summary,
    CompactionOptions, CompactionPreservePolicy, CompactionResult,
};
pub use conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
pub use errors::{AgentError, ProviderError, ToolError};
pub use events::{
    AgentEvent, AgentRegistryLoaded, AgentResult, ApprovalPromptResolved, ApprovalPromptShown,
    ApprovalRevisionRequested, ChangesViewed, CheckpointEventPayload, CompactBoundary,
    CompactionTrigger, DiffViewed, DoctorCheckCompleted, DoctorCheckStarted, DoctorSeverity,
    DoctorStatus, HookEvaluated, HookOutcomeKind, InitCompleted, InitStarted, InterruptedEvent,
    NotificationFailed, NotificationSent, PluginComponentFailed, PluginComponentLoaded,
    PluginDisabled, PluginEnabled, PluginInstalled, PluginMarketplaceAdded,
    PluginMarketplaceEntryInstalled, PluginRegistryLoaded, PluginReloaded, PluginUninstalled,
    PluginValidationFailed, PolicyDecisionEvent, ResultSubtype, SessionRecovered,
    SessionRecoveryFailed, SessionRuntimeState, SkillInvocationFailed, SkillLoaded,
    SkillRegistryLoaded, SkillResourceLoaded, SlashCommandCompleted, SlashCommandExpanded,
    SlashCommandFailed, SlashCommandRegistryLoaded, SlashCommandStarted, StatusLineConfigured,
    SubagentCancelled, SubagentEvent as SubagentForwardedEvent, SubagentFailed, SubagentStart,
    SubagentStop, SubagentTimedOut, TodoUpdate, ToolCallCompleted, ToolCallDenied, ToolCallStarted,
    ToolCatalogBuilt, ToolContextBudgetUpdated, ToolDeferred, ToolDisabled, ToolLoaded,
    ToolLoadingModeResolved, ToolSearchCompleted, ToolSearchFailed, ToolSearchStarted,
    UiRendererChanged, UiStarted, UserToolResult,
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
pub use plugins::{
    build_plugin_runtime, discover_manifest_path, install_to_cache, is_plugin_name_safe,
    layout_for_scope, load_plugin_components, make_install, merge_plugin_defaults_into_settings,
    parse_and_validate_source, plugin_cache_dir, plugin_data_dir, prune_orphans, redacted_display,
    remove_data_dir, resolve_safe_plugin_path, substitute_str, substitute_value, uninstall_cache,
    validate_plugin_paths, EffectivePluginConfig, LoadedPluginComponents, LocalMarketplaceError,
    LocalPluginMarketplace, LocalPluginMarketplaceEntry, PluginAuthor, PluginComponentFailure,
    PluginComponentKind, PluginComponentSummary, PluginHookCommand, PluginHookMatcherEntry,
    PluginHooksFile, PluginInstall, PluginInstallError, PluginInstallLayout, PluginLoadContext,
    PluginLoadOptions, PluginLoadedHook, PluginManifest, PluginManifestError,
    PluginMarketplaceOwner, PluginRegistry, PluginRuntimeBuild, PluginRuntimePaths, PluginScope,
    PluginSource, PluginStatus, PluginSubstError, PluginSummary, PluginValidationError,
    PluginValidationFailure, PluginVarContext, COMPAT_PLUGIN_MANIFEST_PATH, MAX_PLUGIN_NAME_LEN,
    PLUGIN_MANIFEST_PATH,
};
pub use policy::{
    approval_key, ApprovalMemory, ApprovalMemorySnapshot, MemoryDecision, PolicyConfig,
    PolicyDecision, PolicyDecisionSource, PolicyEngine, PolicyOutcome, PolicyToolContext,
};
pub use provider::{ModelEvent, ModelProvider, ModelRequest};
pub use run_id::RunId;
pub use settings::{
    AgentsSettingsConfig, CheckpointRetentionConfig, CheckpointingConfig, CompactionSettingsConfig,
    EnabledPluginEntry, HooksConfig, PluginsSettingsConfig, ProjectInstructionsMode, SettingSource,
    Settings, SettingsError, SettingsFile, SkillsSettingsConfig, SlashCommandsSettingsConfig,
    ToolLimitsConfig, ValidationConfig,
};
pub use skills::{
    compat_claude_skills_dir, filesystem_skills_dir, is_identifier_safe_skill_name,
    load_filesystem_skills, parse_skill_md, resource_kind_for, LoadedSkillsSnapshot,
    LoadedSkillsState, SkillDefinition, SkillDiscoveryOptions, SkillDiscoveryPath, SkillError,
    SkillRegistry, SkillResourceKind, SkillResourceSummary, SkillSource, SkillSummary, SkillTool,
    SkillToolInput, SkillToolResult, DEFAULT_MAX_SKILL_MD_BYTES, SKILL_TOOL_NAME,
};
pub use slash::{
    builtin_command_summary, custom_commands_dir, expand_custom_command,
    is_identifier_safe_command_name, load_custom_commands, parse_custom_command,
    parse_slash_command, render_synthetic_skill_prompt, BuiltInSlashCommand,
    CustomSlashCommandDefinition, CustomSlashCommandError, ExpandedToolCallSummary,
    ParsedSlashCommand, SlashBuiltinResult, SlashCommandDispatch, SlashCommandError,
    SlashCommandHandler, SlashCommandRegistry, SlashCommandSource, SlashCommandSummary,
    SlashExpansionOptions, SlashExpansionResult,
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
pub use ui::{
    merge_recovery_into, merge_ui_into, render_status_line, supported_status_line_variables,
    unknown_status_line_variables, ColorCapability, ComposerConfig, ComposerSettingsConfig,
    NotificationConfig, NotificationSettingsConfig, RecoveryConfig, RecoverySettingsConfig,
    StatusLineConfig, StatusLineContext, StatusLineSettingsConfig, SubmitKey, TerminalCapabilities,
    TerminalRenderer, UiConfig, UiSettingsConfig, UiSettingsError, DEFAULT_STATUSLINE_TEMPLATE,
};
pub use usage::TokenUsage;
