//! Trait the agent loop uses to interact with an optional checkpointing
//! subsystem (M3). The concrete implementation lives in
//! `crom-checkpoint`; this module only exposes the integration
//! surface so `crom-core` does not depend on the checkpoint crate.

use async_trait::async_trait;

use crate::events::CheckpointEventPayload;
use crate::options::SessionId;
use crate::run_id::RunId;

/// Result of a pre-mutation capture call.
#[derive(Debug, Clone, Default)]
pub struct CheckpointCaptureResult {
    /// Events to be forwarded to the agent's session sink and event stream.
    pub events: Vec<CheckpointEventPayload>,
    /// When `Some(reason)`, the calling tool MUST be denied.
    pub deny: Option<String>,
}

/// Bash-specific advisory result. Returns events (e.g. warnings) and an
/// optional deny reason driven by the configured `BashCheckpointPolicy`.
#[derive(Debug, Clone, Default)]
pub struct CheckpointBashAction {
    pub events: Vec<CheckpointEventPayload>,
    pub deny: Option<String>,
}

/// Per-run checkpoint handle the agent loop calls into between approval and
/// mutation. Implementations should be cheap to clone (typically wrapping
/// an `Arc<LocalCheckpointStore>` plus a checkpoint id).
///
/// **Lifecycle contract**: a handle is bound to a single session and a
/// single per-run checkpoint at construction time. The agent loop also
/// passes `session_id` and `run_id` on every call so an implementation
/// can detect (and refuse) accidental reuse across sessions — a defense
/// against fork-lineage bleed where a forked AgentClient might inherit
/// the parent's handle.
#[async_trait]
pub trait CheckpointHandle: Send + Sync {
    /// Capture pre-mutation snapshots for the paths implied by the tool's
    /// input. The agent loop calls this AFTER policy approval and BEFORE
    /// the tool's `execute()` runs.
    async fn capture_for_tool(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        tool_name: &str,
        tool_input: &serde_json::Value,
    ) -> CheckpointCaptureResult;

    /// Optional Bash advisory hook. The agent loop calls this when Bash is
    /// requested and checkpointing is enabled. `explicitly_approved` is
    /// true when the user (or a specific rule) approved this exact
    /// command, as opposed to a broad allow.
    async fn before_bash(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        command: &str,
        explicitly_approved: bool,
    ) -> CheckpointBashAction;

    /// The active checkpoint id, if a checkpoint exists for this run.
    fn checkpoint_id(&self) -> Option<String>;
}

/// Convenience API the agent client exposes for create/list/diff/rewind.
/// Return values are JSON to keep `crom-core` independent of the
/// concrete checkpoint store crate.
#[async_trait]
pub trait CheckpointApi: Send + Sync {
    async fn create_checkpoint(
        &self,
        description: Option<String>,
    ) -> Result<String, crate::AgentError>;

    async fn list_checkpoints(&self) -> Result<serde_json::Value, crate::AgentError>;

    async fn diff_checkpoint(
        &self,
        checkpoint_id: &str,
    ) -> Result<serde_json::Value, crate::AgentError>;

    async fn rewind(
        &self,
        checkpoint_id: &str,
        force_cwd: bool,
        dry_run: bool,
    ) -> Result<serde_json::Value, crate::AgentError>;
}
