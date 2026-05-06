//! Structured telemetry events emitted by the agent runtime. These are
//! orthogonal to `AgentEvent` (which is the user-facing event stream) and
//! are intended for logging, observability, and debugging.

use serde::{Deserialize, Serialize};

use crate::hooks::HookEventName;
use crate::options::SessionId;
use crate::policy::{PolicyDecisionSource, PolicyOutcome};
use crate::run_id::RunId;
use crate::tool_catalog::{ResolvedToolLoadingMode, ToolLoadingMode, ToolSearchStrategy};
use crate::usage::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentTelemetryEvent {
    ModelRequestStarted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        model: String,
    },
    ModelRequestCompleted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        model: String,
        duration_ms: u64,
        usage: Option<TokenUsage>,
        cost_usd: Option<f64>,
        outcome: TelemetryOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_category: Option<String>,
    },
    ToolExecutionStarted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        tool_name: String,
    },
    ToolExecutionCompleted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        tool_name: String,
        duration_ms: u64,
        outcome: TelemetryOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        error_category: Option<String>,
    },
    PermissionEvaluated {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        tool_name: String,
        outcome: PolicyOutcome,
        source: PolicyDecisionSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    HookEvaluated {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        hook_event: HookEventName,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_name: Option<String>,
        outcome: TelemetryOutcome,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    SessionLoaded {
        session_id: SessionId,
        run_id: RunId,
        cwd: std::path::PathBuf,
        kind: SessionLoadKind,
    },
    SessionPersisted {
        session_id: SessionId,
        run_id: RunId,
    },
    AgentInterrupted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
    },
    McpServerConnecting {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        transport: code_agent_mcp::McpTransportKind,
    },
    McpServerConnected {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        transport: code_agent_mcp::McpTransportKind,
        tool_count: usize,
        outcome: TelemetryOutcome,
    },
    McpServerFailed {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        transport: code_agent_mcp::McpTransportKind,
        outcome: TelemetryOutcome,
        error_category: String,
    },
    McpToolsDiscovered {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        tool_count: usize,
    },
    McpToolCallStarted {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        tool_name: String,
    },
    McpToolCallCompleted {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        tool_name: String,
        duration_ms: u64,
        result_bytes: usize,
        outcome: TelemetryOutcome,
    },
    McpToolCallFailed {
        session_id: SessionId,
        run_id: RunId,
        server_name: String,
        tool_name: String,
        error_category: String,
    },
    McpConfigLoaded {
        session_id: SessionId,
        run_id: RunId,
        server_count: usize,
    },
    McpConfigError {
        session_id: SessionId,
        run_id: RunId,
        error_category: String,
    },
    ToolCatalogBuilt {
        session_id: SessionId,
        run_id: RunId,
        total_tool_count: usize,
        loaded_tool_count: usize,
        deferred_tool_count: usize,
        estimated_total_tool_tokens: usize,
        outcome: TelemetryOutcome,
    },
    ToolLoadingModeResolved {
        session_id: SessionId,
        run_id: RunId,
        configured_mode: ToolLoadingMode,
        resolved_mode: ResolvedToolLoadingMode,
        reason: String,
    },
    ToolContextBudgetUpdated {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        loaded_tool_count: usize,
        deferred_tool_count: usize,
        estimated_total_tool_tokens: usize,
        estimated_loaded_tool_tokens: usize,
        estimated_deferred_tool_tokens: usize,
    },
    ToolSearchStarted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        query: String,
        strategy: ToolSearchStrategy,
        limit: usize,
    },
    ToolSearchCompleted {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        query: String,
        strategy: ToolSearchStrategy,
        result_count: usize,
        loaded_tool_count: usize,
        duration_ms: u64,
        outcome: TelemetryOutcome,
    },
    ToolSearchFailed {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        query: String,
        strategy: ToolSearchStrategy,
        error_category: String,
    },
    ToolLoaded {
        session_id: SessionId,
        run_id: RunId,
        turn_index: Option<u32>,
        tool_name: String,
        reason: String,
    },
    ToolDeferred {
        session_id: SessionId,
        run_id: RunId,
        tool_name: String,
        estimated_tokens: usize,
    },
    ToolDisabled {
        session_id: SessionId,
        run_id: RunId,
        tool_name: String,
    },
    ToolProviderNativeSearchEnabled {
        session_id: SessionId,
        run_id: RunId,
        mode: String,
        outcome: TelemetryOutcome,
    },
    ToolProviderNativeSearchFallback {
        session_id: SessionId,
        run_id: RunId,
        mode: String,
        error_category: String,
    },
    // ----- M6 subagent orchestration -----
    AgentRegistryLoaded {
        session_id: SessionId,
        run_id: RunId,
        agent_count: usize,
    },
    AgentToolInvoked {
        session_id: SessionId,
        run_id: RunId,
        turn_index: u32,
        agent_name: String,
        parent_tool_call_id: String,
    },
    SubagentQueued {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        queue_duration_ms: u64,
    },
    SubagentStart {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        subagent_run_id: RunId,
        model: String,
    },
    SubagentStop {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        subagent_run_id: RunId,
        outcome: TelemetryOutcome,
        duration_ms: u64,
        tool_calls: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<TokenUsage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
    },
    SubagentFailed {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        error_category: String,
    },
    SubagentCancelled {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        reason: String,
    },
    SubagentTimedOut {
        session_id: SessionId,
        run_id: RunId,
        agent_name: String,
        subagent_session_id: SessionId,
        timeout_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryOutcome {
    Ok,
    Error,
    Allow,
    Deny,
    Ask,
    Continue,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionLoadKind {
    New,
    Resume,
    Continue,
    Fork,
}

/// Approximate model pricing in USD per 1M tokens. Set by callers (e.g. CLI)
/// when known; if absent, costs are not reported.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input_per_million: f64,
    pub output_per_million: f64,
    #[serde(default)]
    pub cache_creation_per_million: Option<f64>,
    #[serde(default)]
    pub cache_read_per_million: Option<f64>,
}

impl ModelPricing {
    /// Estimate USD cost of a single usage record.
    pub fn estimate(&self, u: &TokenUsage) -> f64 {
        let input = u.input_tokens as f64;
        let output = u.output_tokens as f64;
        let cache_create = u.cache_creation_input_tokens.unwrap_or(0) as f64;
        let cache_read = u.cache_read_input_tokens.unwrap_or(0) as f64;
        let mut cost = (input * self.input_per_million + output * self.output_per_million) / 1.0e6;
        if let Some(rate) = self.cache_creation_per_million {
            cost += (cache_create * rate) / 1.0e6;
        }
        if let Some(rate) = self.cache_read_per_million {
            cost += (cache_read * rate) / 1.0e6;
        }
        cost
    }
}
