use serde::{Deserialize, Serialize};

use crate::conversation::{AssistantMessage, SystemInit, ToolCall};
use crate::hooks::HookEventName;
use crate::options::SessionId;
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
    Result(AgentResult),
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
