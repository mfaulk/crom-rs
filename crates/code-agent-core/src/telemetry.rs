//! Structured telemetry events emitted by the agent runtime. These are
//! orthogonal to `AgentEvent` (which is the user-facing event stream) and
//! are intended for logging, observability, and debugging.

use serde::{Deserialize, Serialize};

use crate::hooks::HookEventName;
use crate::options::SessionId;
use crate::policy::{PolicyDecisionSource, PolicyOutcome};
use crate::run_id::RunId;
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
