//! `CheckpointApi` impl backed by `LocalCheckpointStore`. Used by
//! `AgentClient::{create_checkpoint, list_checkpoints, diff_checkpoint,
//! rewind_files}` and by the CLI commands.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use crom_core::{AgentError, CheckpointApi, RunId, SessionId};

use crate::diff::DiffCheckpointRequest;
use crate::store::{
    CheckpointError, CheckpointStore, CreateCheckpointRequest, LocalCheckpointStore, RewindRequest,
};
use crate::types::CheckpointId;

/// Concrete implementation of [`crom_core::CheckpointApi`].
pub struct LocalCheckpointApi {
    pub store: Arc<LocalCheckpointStore>,
    pub session_id: SessionId,
    pub cwd: PathBuf,
}

impl LocalCheckpointApi {
    pub fn new(store: Arc<LocalCheckpointStore>, session_id: SessionId, cwd: PathBuf) -> Self {
        Self {
            store,
            session_id,
            cwd,
        }
    }
}

fn map_err(e: CheckpointError) -> AgentError {
    AgentError::other(format!("checkpoint: {e}"))
}

#[async_trait]
impl CheckpointApi for LocalCheckpointApi {
    async fn create_checkpoint(&self, description: Option<String>) -> Result<String, AgentError> {
        let id = self
            .store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: self.session_id.clone(),
                run_id: RunId::new(),
                cwd: self.cwd.clone(),
                description,
                user_message_id: None,
                git_state: crate::probe_git_state(&self.cwd).await,
            })
            .await
            .map_err(map_err)?;
        Ok(id.to_string())
    }

    async fn list_checkpoints(&self) -> Result<serde_json::Value, AgentError> {
        let list = self
            .store
            .list_checkpoints(&self.session_id)
            .await
            .map_err(map_err)?;
        serde_json::to_value(list).map_err(|e| AgentError::other(format!("checkpoint list: {e}")))
    }

    async fn diff_checkpoint(&self, checkpoint_id: &str) -> Result<serde_json::Value, AgentError> {
        let diff = self
            .store
            .diff_checkpoint(DiffCheckpointRequest {
                checkpoint_id: CheckpointId::from(checkpoint_id.to_string()),
                session_id: self.session_id.clone(),
                cwd: self.cwd.clone(),
                max_bytes: None,
            })
            .await
            .map_err(map_err)?;
        serde_json::to_value(diff).map_err(|e| AgentError::other(format!("checkpoint diff: {e}")))
    }

    async fn rewind(
        &self,
        checkpoint_id: &str,
        force_cwd: bool,
        dry_run: bool,
    ) -> Result<serde_json::Value, AgentError> {
        let report = self
            .store
            .rewind(RewindRequest {
                checkpoint_id: CheckpointId::from(checkpoint_id.to_string()),
                cwd: self.cwd.clone(),
                force_cwd,
                dry_run,
            })
            .await
            .map_err(map_err)?;
        serde_json::to_value(report)
            .map_err(|e| AgentError::other(format!("checkpoint rewind: {e}")))
    }
}
