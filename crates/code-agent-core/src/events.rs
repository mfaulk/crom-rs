use serde::{Deserialize, Serialize};

use crate::conversation::{AssistantMessage, SystemInit, ToolCall};
use crate::hooks::HookEventName;
use crate::options::SessionId;
use crate::plugins::{PluginComponentKind, PluginScope, PluginSource, PluginStatus, PluginSummary};
use crate::policy::{PolicyDecisionSource, PolicyOutcome};
use crate::run_id::RunId;
use crate::todo::TodoList;
use crate::tool_catalog::{
    ResolvedToolLoadingMode, ToolContextBudget, ToolLoadReason, ToolLoadingMode,
    ToolSearchMatchSummary, ToolSearchStrategy, ToolSource, ToolSourceFilter,
};
use crate::usage::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    SystemInit(SystemInit),
    AssistantMessage(AssistantMessage),
    AssistantTextDelta(String),
    ToolCallStarted(ToolCallStarted),
    ToolCallCompleted(ToolCallCompleted),
    ToolCallDenied(ToolCallDenied),
    UserToolResult(UserToolResult),
    HookEvaluated(HookEvaluated),
    PolicyDecision(PolicyDecisionEvent),
    TodoUpdate(TodoUpdate),
    Interrupted(InterruptedEvent),
    /// Emitted by the checkpoint subsystem (M3). The payload's `event` field
    /// names the concrete checkpoint event (e.g. `created`,
    /// `file_snapshot_captured`, `rewind_completed`); structured data is
    /// flattened alongside it.
    CheckpointEvent(CheckpointEventPayload),
    McpServerConnecting(code_agent_mcp::McpServerConnecting),
    McpServerConnected(code_agent_mcp::McpServerConnected),
    McpServerFailed(code_agent_mcp::McpServerFailed),
    McpToolsDiscovered(code_agent_mcp::McpToolsDiscovered),
    McpToolCallStarted(code_agent_mcp::McpToolCallStarted),
    McpToolCallCompleted(code_agent_mcp::McpToolCallCompleted),
    McpToolCallFailed(code_agent_mcp::McpToolCallFailed),
    McpServerLog(code_agent_mcp::McpServerLog),
    ToolCatalogBuilt(ToolCatalogBuilt),
    ToolSearchStarted(ToolSearchStarted),
    ToolSearchCompleted(ToolSearchCompleted),
    ToolLoaded(ToolLoaded),
    ToolDeferred(ToolDeferred),
    ToolDisabled(ToolDisabled),
    ToolSearchFailed(ToolSearchFailed),
    ToolContextBudgetUpdated(ToolContextBudgetUpdated),
    ToolLoadingModeResolved(ToolLoadingModeResolved),
    /// Subagent registry summary, emitted once after agents are registered.
    AgentRegistryLoaded(AgentRegistryLoaded),
    /// Subagent run started.
    SubagentStart(SubagentStart),
    /// Forwarded child event (only when `event_visibility = StreamChildEvents`).
    SubagentEvent(SubagentEvent),
    /// Subagent run completed (with any terminal status).
    SubagentStop(SubagentStop),
    /// Subagent failed (provider, hook, or tool error).
    SubagentFailed(SubagentFailed),
    /// Subagent was cancelled by parent or operator.
    SubagentCancelled(SubagentCancelled),
    /// Subagent exceeded its per-run timeout.
    SubagentTimedOut(SubagentTimedOut),
    /// Skill registry summary, emitted once after Skills are registered.
    SkillRegistryLoaded(SkillRegistryLoaded),
    /// Skill loaded into the current run.
    SkillLoaded(SkillLoaded),
    /// Skill resource loaded with safe summary.
    SkillResourceLoaded(SkillResourceLoaded),
    /// Skill invocation attempted but failed.
    SkillInvocationFailed(SkillInvocationFailed),
    /// Slash-command registry summary, emitted once after commands load.
    SlashCommandRegistryLoaded(SlashCommandRegistryLoaded),
    /// Slash command begun (just after parsing).
    SlashCommandStarted(SlashCommandStarted),
    /// Slash command body successfully expanded into a final prompt.
    SlashCommandExpanded(SlashCommandExpanded),
    /// Slash command finished dispatching (handler returned).
    SlashCommandCompleted(SlashCommandCompleted),
    /// Slash command failed (parsing, expansion, or handler).
    SlashCommandFailed(SlashCommandFailed),
    /// Manual conversation compaction completed.
    CompactBoundary(CompactBoundary),
    /// Plugin registry summary, emitted once during initialization.
    PluginRegistryLoaded(PluginRegistryLoaded),
    /// Plugin installed into a scope.
    PluginInstalled(PluginInstalled),
    /// Plugin uninstalled from a scope.
    PluginUninstalled(PluginUninstalled),
    /// Plugin enabled in its scope.
    PluginEnabled(PluginEnabled),
    /// Plugin disabled in its scope.
    PluginDisabled(PluginDisabled),
    /// Plugin manifests/components reloaded.
    PluginReloaded(PluginReloaded),
    /// One plugin component (skill/command/agent/hook/MCP/setting/bin) loaded.
    PluginComponentLoaded(PluginComponentLoaded),
    /// One plugin component failed to load.
    PluginComponentFailed(PluginComponentFailed),
    /// Plugin manifest validation failed.
    PluginValidationFailed(PluginValidationFailed),
    /// Local marketplace file added to the session/workspace.
    PluginMarketplaceAdded(PluginMarketplaceAdded),
    /// A plugin was installed via a local marketplace entry.
    PluginMarketplaceEntryInstalled(PluginMarketplaceEntryInstalled),
    /// M9: terminal UI started for a session.
    UiStarted(UiStarted),
    /// M9: renderer changed (e.g. fullscreen → plain fallback).
    UiRendererChanged(UiRendererChanged),
    /// M9: status line template configured/updated at runtime.
    StatusLineConfigured(StatusLineConfigured),
    /// M9: an approval prompt was presented to the user.
    ApprovalPromptShown(ApprovalPromptShown),
    /// M9: an approval prompt was resolved (allow/deny/revision).
    ApprovalPromptResolved(ApprovalPromptResolved),
    /// M9: user requested a revision instead of approving or denying.
    ApprovalRevisionRequested(ApprovalRevisionRequested),
    /// M9: user inspected a checkpoint or workspace diff.
    DiffViewed(DiffViewed),
    /// M9: user listed session changes.
    ChangesViewed(ChangesViewed),
    /// M9: doctor diagnostic check started.
    DoctorCheckStarted(DoctorCheckStarted),
    /// M9: doctor diagnostic check completed.
    DoctorCheckCompleted(DoctorCheckCompleted),
    /// M9: a recoverable session was restored.
    SessionRecovered(SessionRecovered),
    /// M9: session recovery failed (corrupted JSONL, missing data, ...).
    SessionRecoveryFailed(SessionRecoveryFailed),
    /// M9: `code-agent init` started writing files.
    InitStarted(InitStarted),
    /// M9: `code-agent init` finished.
    InitCompleted(InitCompleted),
    /// M9: a local notification was sent.
    NotificationSent(NotificationSent),
    /// M9: a local notification failed to send.
    NotificationFailed(NotificationFailed),
    /// M10: provider auth registry summarized at startup.
    AuthProviderRegistryLoaded(AuthProviderRegistryLoaded),
    /// M10: a login flow began.
    LoginStarted(LoginStarted),
    /// M10: a login flow completed successfully.
    LoginCompleted(LoginCompleted),
    /// M10: a login flow failed.
    LoginFailed(LoginFailed),
    /// M10: logout completed.
    LogoutCompleted(LogoutCompleted),
    /// M10: a credential was stored.
    CredentialStored(CredentialStored),
    /// M10: a credential was refreshed.
    CredentialRefreshed(CredentialRefreshed),
    /// M10: a credential refresh attempt failed.
    CredentialRefreshFailed(CredentialRefreshFailed),
    /// M10: an auth provider was selected as active.
    AuthProviderSelected(AuthProviderSelected),
    /// M10: a model was selected.
    ModelSelected(ModelSelected),
    /// M10: a model list was loaded.
    ModelListLoaded(ModelListLoaded),
    /// M10: an auth status check was performed.
    AuthStatusChecked(AuthStatusChecked),
    Result(AgentResult),
}

/// M10 auth events. We define them here (rather than re-using the auth
/// crate's records via `#[serde(transparent)]`) so the central
/// `AgentEvent` enum stays the canonical wire format. The CLI/TUI
/// converts auth-crate records into these wrappers before publishing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderRegistryLoaded {
    pub providers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStarted {
    pub provider: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginCompleted {
    pub provider: String,
    pub method: String,
    pub account_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_account: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFailed {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub kind: String,
    pub message: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutCompleted {
    pub provider: String,
    pub account_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialStored {
    pub provider: String,
    pub account_hash: String,
    pub store: String,
    pub secret_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRefreshed {
    pub provider: String,
    pub account_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRefreshFailed {
    pub provider: String,
    pub account_hash: String,
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderSelected {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelected {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelListLoaded {
    pub provider: String,
    pub model_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatusChecked {
    pub provider: String,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiStarted {
    pub renderer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub width: u16,
    pub height: u16,
    pub is_tty: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiRendererChanged {
    pub from: String,
    pub to: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusLineConfigured {
    pub template: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPromptShown {
    pub tool_name: String,
    pub permission_kind: crate::permission::PermissionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalPromptResolved {
    pub tool_name: String,
    pub decision: crate::permission::PermissionDecision,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRevisionRequested {
    pub tool_name: String,
    pub message_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffViewed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    pub files_changed: u32,
    pub bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangesViewed {
    pub session_id: SessionId,
    pub files_changed: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoctorSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheckStarted {
    pub check_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheckCompleted {
    pub check_id: String,
    pub status: DoctorStatus,
    pub severity: DoctorSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionRuntimeState {
    Idle,
    Running,
    Interrupted,
    Crashed,
    Complete,
    NeedsRecovery,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecovered {
    pub session_id: SessionId,
    pub previous_state: SessionRuntimeState,
    pub records_recovered: u32,
    pub records_skipped: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecoveryFailed {
    pub session_id: SessionId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitStarted {
    pub mode: String,
    pub cwd: std::path::PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitCompleted {
    pub mode: String,
    pub cwd: std::path::PathBuf,
    pub created_settings: bool,
    pub created_agents_md: bool,
    pub created_agent_dir: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSent {
    pub channel: String,
    pub event: String,
    pub message_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationFailed {
    pub channel: String,
    pub event: String,
    pub reason: String,
}

/// Loosely-typed checkpoint event delivered through `AgentEvent`. The
/// `code-agent-checkpoint` crate produces these by serializing its
/// concrete `CheckpointEvent` enum into JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointEventPayload {
    pub event: String,
    #[serde(flatten)]
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStarted {
    pub call: ToolCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallCompleted {
    pub tool_use_id: String,
    pub tool_name: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDenied {
    pub tool_use_id: String,
    pub tool_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserToolResult {
    pub tool_use_id: String,
    pub tool_name: String,
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResult {
    pub subtype: ResultSubtype,
    pub result: Option<String>,
    pub session_id: SessionId,
    #[serde(default)]
    pub run_id: Option<RunId>,
    pub num_turns: u32,
    pub usage: Option<TokenUsage>,
    pub total_cost_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResultSubtype {
    Success,
    ErrorMaxTurns,
    ErrorDuringExecution,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEvaluated {
    pub event: HookEventName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    pub outcome: HookOutcomeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HookOutcomeKind {
    Allow,
    Deny,
    Ask,
    Continue,
    AddContext,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecisionEvent {
    pub tool_name: String,
    pub outcome: PolicyOutcome,
    pub source: PolicyDecisionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoUpdate {
    pub list: TodoList,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptedEvent {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub turn_index: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCatalogBuilt {
    pub total_tools: usize,
    pub eager_tools: usize,
    pub deferred_tools: usize,
    pub disabled_tools: usize,
    pub estimated_total_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSearchStarted {
    pub query: String,
    pub strategy: ToolSearchStrategy,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_filter: Option<ToolSourceFilter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSearchCompleted {
    pub query: String,
    pub matches: Vec<ToolSearchMatchSummary>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLoaded {
    pub tool_name: String,
    pub source: ToolSource,
    pub reason: ToolLoadReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeferred {
    pub tool_name: String,
    pub source: ToolSource,
    pub estimated_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDisabled {
    pub tool_name: String,
    pub source: ToolSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSearchFailed {
    pub query: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolContextBudgetUpdated {
    pub estimated_total_tool_tokens: usize,
    pub estimated_loaded_tool_tokens: usize,
    pub estimated_deferred_tool_tokens: usize,
    pub loaded_tool_count: usize,
    pub deferred_tool_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolLoadingModeResolved {
    pub configured_mode: ToolLoadingMode,
    pub resolved_mode: ResolvedToolLoadingMode,
    pub reason: String,
}

impl From<ToolContextBudget> for ToolContextBudgetUpdated {
    fn from(value: ToolContextBudget) -> Self {
        Self {
            estimated_total_tool_tokens: value.estimated_total_tool_tokens,
            estimated_loaded_tool_tokens: value.estimated_loaded_tool_tokens,
            estimated_deferred_tool_tokens: value.estimated_deferred_tool_tokens,
            loaded_tool_count: 0,
            deferred_tool_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRegistryLoaded {
    pub agents: Vec<crate::agents::AgentDefinitionSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStart {
    pub agent_name: String,
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: String,
    pub subagent_session_id: SessionId,
    pub task_preview: String,
    pub tools: Vec<String>,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentEvent {
    pub agent_name: String,
    pub subagent_session_id: SessionId,
    pub event: Box<AgentEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentStop {
    pub agent_name: String,
    pub subagent_session_id: SessionId,
    pub status: crate::agents::SubagentStatus,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    pub result_preview: String,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentFailed {
    pub agent_name: String,
    pub subagent_session_id: SessionId,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentCancelled {
    pub agent_name: String,
    pub subagent_session_id: SessionId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentTimedOut {
    pub agent_name: String,
    pub subagent_session_id: SessionId,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRegistryLoaded {
    pub skills: Vec<crate::skills::SkillSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillLoaded {
    pub name: String,
    pub source: crate::skills::SkillSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub loaded_resource_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResourceLoaded {
    pub skill_name: String,
    pub path: String,
    pub size_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInvocationFailed {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandRegistryLoaded {
    pub commands: Vec<crate::slash::SlashCommandSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandStarted {
    pub command: String,
    pub arguments: Vec<String>,
    pub source: crate::slash::SlashCommandSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandExpanded {
    pub command: String,
    pub expanded_prompt_preview: String,
    pub file_refs: Vec<String>,
    pub bash_commands: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandCompleted {
    pub command: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandFailed {
    pub command: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    ManualSlashCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRegistryLoaded {
    pub plugins: Vec<PluginSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstalled {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub scope: PluginScope,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUninstalled {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub scope: PluginScope,
    pub kept_data: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnabled {
    pub name: String,
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDisabled {
    pub name: String,
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginReloaded {
    pub plugin_count: usize,
    pub component_total: usize,
    pub plugins: Vec<PluginSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginComponentLoaded {
    pub plugin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub component_kind: PluginComponentKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PluginSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginComponentFailed {
    pub plugin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub component_kind: PluginComponentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginValidationFailed {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    pub source_path: std::path::PathBuf,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<PluginStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMarketplaceAdded {
    pub marketplace: String,
    pub source_path: std::path::PathBuf,
    pub plugin_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMarketplaceEntryInstalled {
    pub marketplace: String,
    pub entry: String,
    pub plugin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub scope: PluginScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactBoundary {
    pub session_id: SessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    pub pre_messages: usize,
    pub post_messages: usize,
    pub preserved_recent_turns: usize,
    pub trigger: CompactionTrigger,
    /// One-line summary of the older context (the body persisted to history).
    pub summary_preview: String,
}
