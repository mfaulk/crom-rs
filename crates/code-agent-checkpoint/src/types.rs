use std::path::PathBuf;

use code_agent_core::{RunId, SessionId};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Stable identifier for a checkpoint. The string form is `ckpt_<uuid>` so
/// it can be parsed out of CLI output and used as a filesystem path
/// component without ambiguity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct CheckpointId(pub String);

impl CheckpointId {
    pub fn new() -> Self {
        Self(format!("ckpt_{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns true iff the id is a syntactically valid filesystem-safe
    /// identifier we minted: `ckpt_` prefix followed by `[A-Za-z0-9_-]+`.
    pub fn is_valid_format(&self) -> bool {
        let s = &self.0;
        if !s.starts_with("ckpt_") {
            return false;
        }
        let rest = &s["ckpt_".len()..];
        !rest.is_empty()
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
}

impl std::fmt::Display for CheckpointId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for CheckpointId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Default for CheckpointId {
    fn default() -> Self {
        Self::new()
    }
}

/// Optional opaque user-message identifier. Lets callers tie a checkpoint
/// to the specific top-level user prompt that triggered it. Format is
/// validated when used as a filesystem name elsewhere; this type is just
/// a thin wrapper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct UserMessageId(pub String);

impl From<String> for UserMessageId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for UserMessageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Workspace-relative path. We store paths in manifests as forward-slash
/// strings so manifests are portable across platforms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct WorkspaceRelativePath(pub String);

impl WorkspaceRelativePath {
    pub fn from_relative(p: &std::path::Path) -> Self {
        let s = p
            .to_string_lossy()
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

impl std::fmt::Display for WorkspaceRelativePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Snapshot of a single file as it existed before a tracked mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileSnapshotKind {
    ExistingFile {
        path: WorkspaceRelativePath,
        content_hash: String,
        size_bytes: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        modified_time: Option<OffsetDateTime>,
        #[serde(default)]
        executable: bool,
    },
    MissingBeforeChange {
        path: WorkspaceRelativePath,
    },
}

impl FileSnapshotKind {
    pub fn path(&self) -> &WorkspaceRelativePath {
        match self {
            Self::ExistingFile { path, .. } => path,
            Self::MissingBeforeChange { path } => path,
        }
    }
}

/// Metadata persisted for a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMetadata {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub run_id: RunId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_message_id: Option<UserMessageId>,
    pub cwd: PathBuf,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub files_tracked: Vec<FileSnapshotKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_state: Option<GitWorkspaceState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointSummary {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub run_id: RunId,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub files_tracked_count: usize,
}

/// Rewind summary returned by [`crate::CheckpointStore::rewind`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindReport {
    pub checkpoint_id: CheckpointId,
    #[serde(default)]
    pub restored_files: Vec<WorkspaceRelativePath>,
    #[serde(default)]
    pub deleted_files: Vec<WorkspaceRelativePath>,
    #[serde(default)]
    pub recreated_files: Vec<WorkspaceRelativePath>,
    #[serde(default)]
    pub skipped_files: Vec<SkippedRewind>,
    /// True when the report describes a `dry_run` invocation; no files
    /// were actually mutated.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedRewind {
    pub path: WorkspaceRelativePath,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitWorkspaceState {
    pub is_git_repo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    #[serde(default)]
    pub dirty: bool,
}

/// Storage limits applied to checkpoint capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointLimits {
    pub max_checkpoint_bytes_per_file: u64,
    pub max_total_checkpoint_bytes_per_session: u64,
    pub max_checkpoints_per_session: usize,
    pub max_age_days: Option<u32>,
    pub max_diff_bytes: usize,
}

impl Default for CheckpointLimits {
    fn default() -> Self {
        Self {
            max_checkpoint_bytes_per_file: 5 * 1024 * 1024,
            max_total_checkpoint_bytes_per_session: 100 * 1024 * 1024,
            max_checkpoints_per_session: 50,
            max_age_days: Some(14),
            max_diff_bytes: 256 * 1024,
        }
    }
}

impl CheckpointLimits {
    /// Validate that no field has a value that would render checkpoint
    /// creation impossible or that would crash the retention trim loop.
    /// Specifically: `max_checkpoints_per_session == 0` makes the trim
    /// loop `while entries.len() >= 0` an infinite loop / panic on
    /// `entries.remove(0)`, and `max_age_days == Some(0)` would expire
    /// every checkpoint the instant it's written.
    pub fn validate(&self) -> Result<(), crate::store::CheckpointError> {
        if self.max_checkpoints_per_session == 0 {
            return Err(crate::store::CheckpointError::InvalidConfig(
                "max_checkpoints_per_session must be > 0".into(),
            ));
        }
        if let Some(0) = self.max_age_days {
            return Err(crate::store::CheckpointError::InvalidConfig(
                "max_age_days must be > 0 when set".into(),
            ));
        }
        Ok(())
    }
}
