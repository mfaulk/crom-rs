//! Concrete `CheckpointHandle` implementation that ties a
//! `LocalCheckpointStore` and a checkpoint id into the agent loop.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use crom_core::{
    CheckpointBashAction, CheckpointCaptureResult, CheckpointEventPayload, CheckpointHandle, RunId,
    SessionId,
};

use crate::bash_policy::{classify_bash_command, BashCheckpointPolicy, BashCommandClass};
use crate::events::{
    BashCheckpointWarning, CheckpointCaptureFailed, FileSnapshotCaptureKind, FileSnapshotCaptured,
};
use crate::options::{CheckpointFailureMode, CheckpointOptions};
use crate::store::{
    CaptureBeforeToolRequest, CheckpointError, CheckpointStore, LocalCheckpointStore,
};
use crate::types::{CheckpointId, FileSnapshotKind, WorkspaceRelativePath};

/// Per-run checkpoint handle the agent uses to capture pre-mutation state.
pub struct AgentCheckpointHandle {
    pub store: Arc<LocalCheckpointStore>,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub cwd: PathBuf,
    pub checkpoint_id: CheckpointId,
    pub options: CheckpointOptions,
}

/// Chat-friendly checkpoint handle whose active checkpoint id can be
/// updated between user prompts. Internally consults the same
/// `LocalCheckpointStore`, but the `checkpoint_id` is held under a mutex
/// so the chat loop can rotate it.
pub struct RotatingCheckpointHandle {
    inner: tokio::sync::Mutex<RotatingState>,
    pub store: Arc<LocalCheckpointStore>,
    pub session_id: SessionId,
    pub cwd: PathBuf,
    pub options: CheckpointOptions,
}

struct RotatingState {
    checkpoint_id: CheckpointId,
    run_id: RunId,
}

impl RotatingCheckpointHandle {
    pub fn new(
        store: Arc<LocalCheckpointStore>,
        session_id: SessionId,
        cwd: PathBuf,
        checkpoint_id: CheckpointId,
        run_id: RunId,
        options: CheckpointOptions,
    ) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(RotatingState {
                checkpoint_id,
                run_id,
            }),
            store,
            session_id,
            cwd,
            options,
        }
    }

    /// Replace the active checkpoint id (and run id) used for subsequent
    /// `capture_for_tool` calls.
    pub async fn set_active(&self, checkpoint_id: CheckpointId, run_id: RunId) {
        let mut g = self.inner.lock().await;
        g.checkpoint_id = checkpoint_id;
        g.run_id = run_id;
    }
}

#[async_trait]
impl CheckpointHandle for RotatingCheckpointHandle {
    async fn capture_for_tool(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> CheckpointCaptureResult {
        let (id, _stored_run_id) = {
            let g = self.inner.lock().await;
            (g.checkpoint_id.clone(), g.run_id.clone())
        };
        let inner = AgentCheckpointHandle {
            store: self.store.clone(),
            session_id: self.session_id.clone(),
            run_id: run_id.clone(),
            cwd: self.cwd.clone(),
            checkpoint_id: id,
            options: self.options.clone(),
        };
        inner
            .capture_for_tool(session_id, run_id, tool_name, tool_input)
            .await
    }

    async fn before_bash(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        command: &str,
        explicitly_approved: bool,
    ) -> CheckpointBashAction {
        let (id, _stored_run_id) = {
            let g = self.inner.lock().await;
            (g.checkpoint_id.clone(), g.run_id.clone())
        };
        let inner = AgentCheckpointHandle {
            store: self.store.clone(),
            session_id: self.session_id.clone(),
            run_id: run_id.clone(),
            cwd: self.cwd.clone(),
            checkpoint_id: id,
            options: self.options.clone(),
        };
        inner
            .before_bash(session_id, run_id, command, explicitly_approved)
            .await
    }

    fn checkpoint_id(&self) -> Option<String> {
        // Best-effort: try_lock so we don't block. If contended just return None.
        self.inner
            .try_lock()
            .ok()
            .map(|g| g.checkpoint_id.to_string())
    }
}

impl AgentCheckpointHandle {
    pub fn new(
        store: Arc<LocalCheckpointStore>,
        session_id: SessionId,
        run_id: RunId,
        cwd: PathBuf,
        checkpoint_id: CheckpointId,
        options: CheckpointOptions,
    ) -> Self {
        Self {
            store,
            session_id,
            run_id,
            cwd,
            checkpoint_id,
            options,
        }
    }
}

fn payload(name: &str, value: serde_json::Value) -> CheckpointEventPayload {
    CheckpointEventPayload {
        event: name.to_string(),
        data: value,
    }
}

/// Extract workspace-relative paths from a tool call's input. Returns None
/// when the tool isn't tracked by checkpointing.
pub fn paths_for_tool(
    tool_name: &str,
    input: &serde_json::Value,
) -> Option<Vec<WorkspaceRelativePath>> {
    match tool_name {
        "Write" | "Edit" => {
            let p = input.get("file_path").and_then(|v| v.as_str())?;
            Some(vec![WorkspaceRelativePath::from_relative(
                std::path::Path::new(p),
            )])
        }
        "MultiEdit" => {
            let p = input.get("file_path").and_then(|v| v.as_str())?;
            Some(vec![WorkspaceRelativePath::from_relative(
                std::path::Path::new(p),
            )])
        }
        _ => None,
    }
}

#[async_trait]
impl CheckpointHandle for AgentCheckpointHandle {
    async fn capture_for_tool(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> CheckpointCaptureResult {
        // Refuse to write to a different session than the handle was
        // built for. Defense against fork-lineage bleed: if a forked
        // AgentClient inherited the parent's handle, the loop's
        // session_id wouldn't match the handle's and we'd otherwise
        // silently corrupt the parent's manifest.
        if *session_id != self.session_id {
            return CheckpointCaptureResult {
                events: vec![payload(
                    "capture_failed",
                    serde_json::to_value(CheckpointCaptureFailed {
                        checkpoint_id: Some(self.checkpoint_id.clone()),
                        path: None,
                        reason: format!(
                            "checkpoint handle bound to session {} but call was for {}",
                            self.session_id, session_id
                        ),
                    })
                    .unwrap_or(serde_json::Value::Null),
                )],
                deny: Some("checkpoint handle / session mismatch (fork lineage bleed)".into()),
            };
        }
        let paths = match paths_for_tool(tool_name, tool_input) {
            Some(p) => p,
            None => return CheckpointCaptureResult::default(),
        };
        if paths.is_empty() {
            return CheckpointCaptureResult::default();
        }
        let req = CaptureBeforeToolRequest {
            checkpoint_id: self.checkpoint_id.clone(),
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            cwd: self.cwd.clone(),
            tool_name: tool_name.to_string(),
            tool_input: tool_input.clone(),
            paths,
        };
        match self.store.capture_before_tool(req).await {
            Ok(snapshots) => {
                let mut events = Vec::with_capacity(snapshots.len());
                for snap in snapshots {
                    let (path, capture) = describe_snapshot(&snap);
                    events.push(payload(
                        "snapshot_captured",
                        serde_json::to_value(FileSnapshotCaptured {
                            checkpoint_id: self.checkpoint_id.clone(),
                            path,
                            capture,
                        })
                        .unwrap_or(serde_json::Value::Null),
                    ));
                }
                let _ = self
                    .store
                    .refresh_index_count(session_id, &self.checkpoint_id)
                    .await;
                CheckpointCaptureResult { events, deny: None }
            }
            Err(err) => {
                let reason = checkpoint_error_to_reason(&err);
                let events = vec![payload(
                    "capture_failed",
                    serde_json::to_value(CheckpointCaptureFailed {
                        checkpoint_id: Some(self.checkpoint_id.clone()),
                        path: None,
                        reason: reason.clone(),
                    })
                    .unwrap_or(serde_json::Value::Null),
                )];
                let deny = match self.options.failure_mode {
                    CheckpointFailureMode::Strict => Some(reason),
                    CheckpointFailureMode::BestEffort => None,
                };
                CheckpointCaptureResult { events, deny }
            }
        }
    }

    async fn before_bash(
        &self,
        _session_id: &SessionId,
        _run_id: &RunId,
        command: &str,
        explicitly_approved: bool,
    ) -> CheckpointBashAction {
        let class = classify_bash_command(command);
        let policy = self.options.bash_policy;
        let mut events = Vec::new();
        let mut deny: Option<String> = None;

        match policy {
            BashCheckpointPolicy::AllowWithWarning => {
                events.push(bash_warning_event(command, class, policy));
            }
            BashCheckpointPolicy::RequireExplicitApproval => {
                events.push(bash_warning_event(command, class, policy));
                if !explicitly_approved {
                    deny = Some(
                        "checkpointing requires explicit approval for Bash; \
                         re-issue this exact command with --yes-to=Bash:'^…$' \
                         or accept it interactively"
                            .to_string(),
                    );
                }
            }
            BashCheckpointPolicy::DenyLikelyMutatingCommands => {
                events.push(bash_warning_event(command, class, policy));
                if matches!(class, BashCommandClass::LikelyMutating) {
                    deny = Some(
                        "checkpointing policy denies likely-mutating Bash; \
                         use Write/Edit so the change can be checkpointed, \
                         or override the policy"
                            .to_string(),
                    );
                }
            }
        }

        CheckpointBashAction { events, deny }
    }

    fn checkpoint_id(&self) -> Option<String> {
        Some(self.checkpoint_id.to_string())
    }
}

fn describe_snapshot(snap: &FileSnapshotKind) -> (WorkspaceRelativePath, FileSnapshotCaptureKind) {
    match snap {
        FileSnapshotKind::ExistingFile {
            path,
            content_hash,
            size_bytes,
            ..
        } => (
            path.clone(),
            FileSnapshotCaptureKind::ExistingFile {
                size_bytes: *size_bytes,
                content_hash: content_hash.clone(),
            },
        ),
        FileSnapshotKind::MissingBeforeChange { path } => {
            (path.clone(), FileSnapshotCaptureKind::MissingBeforeChange)
        }
    }
}

fn checkpoint_error_to_reason(err: &CheckpointError) -> String {
    err.to_string()
}

fn bash_warning_event(
    command: &str,
    class: BashCommandClass,
    policy: BashCheckpointPolicy,
) -> CheckpointEventPayload {
    let message = match class {
        BashCommandClass::Inert => "Bash changes are not captured by checkpointing".to_string(),
        BashCommandClass::LikelyMutating => {
            "This command may modify files outside checkpoint tracking.".to_string()
        }
        BashCommandClass::Unknown => {
            "Bash file changes are not captured by checkpointing.".to_string()
        }
    };
    payload(
        "bash_warning",
        serde_json::to_value(BashCheckpointWarning {
            command: command.to_string(),
            class,
            policy,
            message,
        })
        .unwrap_or(serde_json::Value::Null),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Write/Edit input shapes resolve to a single tracked path; other tools
    // are not tracked and return None.
    fn paths_for_tool_picks_file_path() {
        let v = serde_json::json!({"file_path": "src/main.rs"});
        let paths = paths_for_tool("Write", &v).unwrap();
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0].as_str(), "src/main.rs");
        assert!(paths_for_tool("Read", &v).is_none());
        assert!(paths_for_tool("Bash", &serde_json::json!({"command": "ls"})).is_none());
    }
}
