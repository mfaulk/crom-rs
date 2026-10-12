//! Public SDK event stream.
//!
//! The SDK exposes a stable, typed event stream. Internal `AgentEvent`
//! values (from `crom_core::events`) are mapped into `SdkEvent` for
//! delivery to applications. Verbosity controls how many internal events
//! flow through.

mod mapping;

use serde::{Deserialize, Serialize};

pub use mapping::{redact_event, SdkEventMapper};

use crom_auth::ProviderId;
use crom_core::plan::PlanDocument;
use crom_core::TokenUsage;

use crate::callback::ApprovalDecision;
use crate::error::Diagnostic;
use crate::patch::PatchSet;
use crate::request::RequestMetadata;
use crate::result::{AgentRunStatus, EventSummary, ValidationReport};
use crate::workspace::WorkspacePath;
use crate::{RunId, SessionId};

/// Coarse-grained verbosity of the event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventVerbosity {
    /// Start, progress, patch, result.
    Minimal,
    /// + assistant, tools, approvals, validations, diagnostics. Default.
    #[default]
    Standard,
    /// All internal events mapped or wrapped.
    Full,
}

/// Public SDK event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// `PlanCreated` boxes the `PlanDocument` payload because the doc is much
// larger than every other event variant; without it the whole enum would
// inherit ~1 KB of stack footprint per event.
#[allow(clippy::large_enum_variant)]
pub enum SdkEvent {
    RunStarted(RunStarted),
    PlanningStarted(PlanningStarted),
    PlanCreated(PlanCreated),
    PlanApproved(PlanApproved),
    ActStarted(ActStarted),
    AssistantMessage(AssistantMessageEvent),
    AssistantTextDelta(AssistantTextDelta),
    ToolCallStarted(ToolCallStartedEvent),
    ToolCallCompleted(ToolCallCompletedEvent),
    ApprovalRequired(ApprovalRequiredEvent),
    ApprovalResolved(ApprovalResolvedEvent),
    PatchProposed(PatchProposedEvent),
    FileGenerated(FileGeneratedEvent),
    ValidationStarted(ValidationStartedEvent),
    ValidationCompleted(ValidationCompletedEvent),
    Diagnostic(DiagnosticEvent),
    RunCompleted(RunCompletedEvent),
    RunFailed(RunFailedEvent),
    RunCancelled(RunCancelledEvent),
    /// Internal events wrapped opaquely under EventVerbosity::Full.
    Internal(InternalEvent),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStarted {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub provider: ProviderId,
    pub model: String,
    pub metadata: RequestMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningStarted {
    pub run_id: RunId,
    pub metadata: RequestMetadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCreated {
    pub run_id: RunId,
    pub plan: PlanDocument,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanApproved {
    pub run_id: RunId,
    pub plan_id: crom_core::plan::PlanId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActStarted {
    pub run_id: RunId,
    pub plan_id: crom_core::plan::PlanId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageEvent {
    pub run_id: RunId,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantTextDelta {
    pub run_id: RunId,
    pub delta: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStartedEvent {
    pub run_id: RunId,
    pub tool_name: String,
    pub tool_use_id: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallCompletedEvent {
    pub run_id: RunId,
    pub tool_name: String,
    pub tool_use_id: String,
    pub is_error: bool,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequiredEvent {
    pub run_id: RunId,
    pub tool_name: String,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResolvedEvent {
    pub run_id: RunId,
    pub tool_name: String,
    pub decision: ApprovalDecisionWire,
}

/// Wire-friendly enum mirror of `ApprovalDecision`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalDecisionWire {
    Allow,
    AllowForSession,
    Deny { reason: String },
}

impl From<ApprovalDecision> for ApprovalDecisionWire {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Allow => Self::Allow,
            ApprovalDecision::AllowForSession => Self::AllowForSession,
            ApprovalDecision::Deny { reason } => Self::Deny { reason },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchProposedEvent {
    pub run_id: RunId,
    pub patch: PatchSet,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileGeneratedEvent {
    pub run_id: RunId,
    pub path: WorkspacePath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationStartedEvent {
    pub run_id: RunId,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCompletedEvent {
    pub run_id: RunId,
    pub report: ValidationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub run_id: RunId,
    pub diagnostic: Diagnostic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCompletedEvent {
    pub run_id: RunId,
    pub status: AgentRunStatus,
    pub summary: String,
    pub usage: Option<TokenUsage>,
    pub events_summary: EventSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFailedEvent {
    pub run_id: RunId,
    pub status: AgentRunStatus,
    pub message: String,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCancelledEvent {
    pub run_id: RunId,
    pub message: String,
}

/// Opaque wrapper for internal events surfaced only under
/// `EventVerbosity::Full`. We keep the payload as JSON so the wire format
/// stays stable even as the internal `AgentEvent` set evolves.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalEvent {
    pub run_id: RunId,
    pub kind: String,
    pub payload: serde_json::Value,
}
