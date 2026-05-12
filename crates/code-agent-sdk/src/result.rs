//! Final result types.

use serde::{Deserialize, Serialize};

use code_agent_auth::ProviderId;
use code_agent_core::plan::PlanDocument;
use code_agent_core::TokenUsage;

use crate::error::Diagnostic;
use crate::patch::{CodeArtifacts, PatchSet};
use crate::request::RequestMetadata;
use crate::workspace::WorkspacePath;
use crate::{RunId, SessionId};

/// High-level outcome of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRunStatus {
    Completed,
    Failed,
    Cancelled,
    TimedOut,
    NeedsApproval,
    NeedsReplan,
    ValidationFailed,
}

/// Aggregated counts that describe the event stream of the run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventSummary {
    pub total_events: usize,
    pub tool_calls: usize,
    pub approvals_requested: usize,
    pub files_changed: usize,
    pub diagnostics_count: usize,
    pub duration_ms: u64,
}

/// Report of a single validation command run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

impl ValidationReport {
    pub fn skipped(command: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: reason.into(),
            duration_ms: 0,
            timed_out: false,
        }
    }
}

/// Final result returned by `run_once` (and matching `final_result()` on a
/// streamed run).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRunResult {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub status: AgentRunStatus,
    pub summary: String,
    pub artifacts: CodeArtifacts,
    pub patch: Option<PatchSet>,
    pub diagnostics: Vec<Diagnostic>,
    pub validations: Vec<ValidationReport>,
    pub plan: Option<PlanDocument>,
    pub usage: Option<TokenUsage>,
    pub provider: ProviderId,
    pub model: String,
    pub events_summary: EventSummary,
    pub metadata: RequestMetadata,
    /// Workspace files that were touched (created/modified) during the run.
    pub changed_files: Vec<WorkspacePath>,
}

impl AgentRunResult {
    /// Convenience: build an empty failed result.
    pub fn failed(
        run_id: RunId,
        provider: ProviderId,
        model: impl Into<String>,
        metadata: RequestMetadata,
        message: impl Into<String>,
    ) -> Self {
        Self {
            run_id,
            session_id: None,
            status: AgentRunStatus::Failed,
            summary: message.into(),
            artifacts: CodeArtifacts::empty(),
            patch: None,
            diagnostics: Vec::new(),
            validations: Vec::new(),
            plan: None,
            usage: None,
            provider,
            model: model.into(),
            events_summary: EventSummary::default(),
            metadata,
            changed_files: Vec::new(),
        }
    }
}
