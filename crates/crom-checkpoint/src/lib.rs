//! Workspace checkpointing and rewind (M3).
//!
//! Provides a content-addressed local checkpoint store, before-mutation
//! snapshot capture for `Write`/`Edit`, and atomic rewind. Checkpoints are
//! stored under `<cwd>/.agent/checkpoints/<session-id>/`. Blobs are
//! deduplicated by SHA-256 hash.

mod api;
mod bash_policy;
mod diff;
mod events;
mod handle;
mod options;
mod store;
mod types;
pub mod validation;

pub use api::LocalCheckpointApi;
pub use bash_policy::{classify_bash_command, BashCheckpointPolicy, BashCommandClass};
pub use diff::{
    redact_path_for_diff, BinaryChangeKind, CheckpointDiff, DiffCheckpointRequest, FileDiff,
    DEFAULT_MAX_DIFF_BYTES,
};
pub use events::{
    CheckpointCaptureFailed, CheckpointCreated, CheckpointEvent, FileRewound,
    FileSnapshotCaptureKind, FileSnapshotCaptured, RewindCompleted, RewindFailure,
    RewindFileAction, RewindStarted, ValidationCommandResult, ValidationCompleted,
    ValidationReport, ValidationStarted,
};
pub use handle::{paths_for_tool, AgentCheckpointHandle, RotatingCheckpointHandle};
pub use options::{CheckpointFailureMode, CheckpointOptions, ValidationOptions};
pub use store::{
    CaptureBeforeToolRequest, CheckpointError, CheckpointStore, CreateCheckpointRequest,
    LocalCheckpointStore, RewindRequest,
};

/// Convenience: serialize a typed checkpoint event to the
/// `crom-core::CheckpointEventPayload` envelope used by the agent
/// event stream.
pub fn to_payload<T: serde::Serialize>(name: &str, value: &T) -> crom_core::CheckpointEventPayload {
    crom_core::CheckpointEventPayload {
        event: name.to_string(),
        data: serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
    }
}

/// Optional Git workspace probe. Best-effort: returns `None` if `git` is
/// not on the PATH or the directory is not a repo.
pub async fn probe_git_state(cwd: &std::path::Path) -> Option<types::GitWorkspaceState> {
    let out = tokio::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });
    let head = tokio::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .await
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });
    let dirty = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(cwd)
        .output()
        .await
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    Some(types::GitWorkspaceState {
        is_git_repo: true,
        branch,
        head,
        dirty,
    })
}
pub use types::{
    CheckpointId, CheckpointLimits, CheckpointMetadata, CheckpointSummary, FileSnapshotKind,
    GitWorkspaceState, RewindReport, SkippedRewind, UserMessageId, WorkspaceRelativePath,
};
