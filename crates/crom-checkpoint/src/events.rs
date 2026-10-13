use crom_core::{RunId, SessionId};
use serde::{Deserialize, Serialize};

use crate::types::{CheckpointId, RewindReport, WorkspaceRelativePath};

/// Checkpoint subsystem events. These flow through the agent event stream
/// and the session JSONL alongside `AgentEvent` values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CheckpointEvent {
    Created(CheckpointCreated),
    SnapshotCaptured(FileSnapshotCaptured),
    CaptureFailed(CheckpointCaptureFailed),
    BashWarning(BashCheckpointWarning),
    RewindStarted(RewindStarted),
    FileRewound(FileRewound),
    RewindCompleted(RewindCompleted),
    RewindFailed(RewindFailure),
    ValidationStarted(ValidationStarted),
    ValidationCompleted(ValidationCompleted),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointCreated {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshotCaptured {
    pub checkpoint_id: CheckpointId,
    pub path: WorkspaceRelativePath,
    pub capture: FileSnapshotCaptureKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSnapshotCaptureKind {
    ExistingFile {
        size_bytes: u64,
        content_hash: String,
    },
    MissingBeforeChange,
    Skipped {
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointCaptureFailed {
    pub checkpoint_id: Option<CheckpointId>,
    pub path: Option<WorkspaceRelativePath>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BashCheckpointWarning {
    pub command: String,
    pub class: crate::bash_policy::BashCommandClass,
    pub policy: crate::bash_policy::BashCheckpointPolicy,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindStarted {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRewound {
    pub checkpoint_id: CheckpointId,
    pub path: WorkspaceRelativePath,
    pub action: RewindFileAction,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RewindFileAction {
    Restored,
    Recreated,
    Deleted,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindCompleted {
    #[serde(flatten)]
    pub report: RewindReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindFailure {
    pub checkpoint_id: CheckpointId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationStarted {
    pub commands: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCompleted {
    #[serde(flatten)]
    pub report: ValidationReport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub passed: bool,
    pub results: Vec<ValidationCommandResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationCommandResult {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub stdout_preview: String,
    pub stderr_preview: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}
