//! Final result types.

use serde::{Deserialize, Serialize};

use crom_auth::ProviderId;
use crom_core::plan::PlanDocument;
use crom_core::TokenUsage;

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

#[cfg(test)]
mod tests {
    //! Pure conversion/constructor logic on the result types: the
    //! `ValidationReport::skipped` and `AgentRunResult::failed` shortcuts and
    //! the snake_case wire form of `AgentRunStatus`.

    use super::*;

    // skipped() builds a non-successful, non-timed-out report whose reason is
    // carried on stderr (where consumers look for skip context) and leaves
    // exit_code/duration empty.
    #[test]
    fn validation_report_skipped_shape() {
        let r = ValidationReport::skipped("cargo test", "command_execution disabled");
        assert_eq!(r.command, "cargo test");
        assert!(!r.success);
        assert!(!r.timed_out);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.duration_ms, 0);
        assert_eq!(r.stderr, "command_execution disabled");
        assert!(r.stdout.is_empty());
    }

    // failed() yields a Failed result with the message as the summary, empty
    // artifacts/patch/diagnostics, and no session id.
    #[test]
    fn agent_run_result_failed_shape() {
        let r = AgentRunResult::failed(
            RunId::new(),
            ProviderId::Fake,
            "fake-model",
            RequestMetadata::default(),
            "boom",
        );
        assert_eq!(r.status, AgentRunStatus::Failed);
        assert_eq!(r.summary, "boom");
        assert_eq!(r.model, "fake-model");
        assert!(r.session_id.is_none());
        assert!(r.patch.is_none());
        assert!(r.diagnostics.is_empty());
        assert!(r.changed_files.is_empty());
    }

    // AgentRunStatus serializes to its snake_case wire form (the public
    // contract for stored/streamed results). Spot-check the compound names.
    #[test]
    fn agent_run_status_serializes_snake_case() {
        let cases = [
            (AgentRunStatus::Completed, "\"completed\""),
            (AgentRunStatus::TimedOut, "\"timed_out\""),
            (AgentRunStatus::NeedsApproval, "\"needs_approval\""),
            (AgentRunStatus::ValidationFailed, "\"validation_failed\""),
        ];
        for (status, wire) in cases {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(json, wire);
            // Round-trips back to the same variant.
            let back: AgentRunStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status);
        }
    }
}
