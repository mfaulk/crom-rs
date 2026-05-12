//! Pluggable storage for session events and plans.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use code_agent_core::plan::{PlanDocument, PlanId};

use crate::event::SdkEvent;
use crate::SessionId;

/// Storage-level error returned by the [`AgentStorage`] trait.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageError {
    pub code: String,
    pub message: String,
}

impl StorageError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    pub fn io(reason: impl Into<String>) -> Self {
        Self::new("storage.io_error", reason.into())
    }
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new("storage.not_found", what.into())
    }
    pub fn serialization(reason: impl Into<String>) -> Self {
        Self::new("storage.serialization", reason.into())
    }
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StorageError {}

/// A loaded session snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub session_id: SessionId,
    pub events: Vec<SdkEvent>,
}

/// Pluggable storage abstraction. Used for session events and plan
/// persistence. Default SDK runtime uses [`NoopStorage`].
#[async_trait]
pub trait AgentStorage: Send + Sync {
    async fn append_event(
        &self,
        session_id: &SessionId,
        event: SdkEvent,
    ) -> Result<(), StorageError>;

    async fn load_session(&self, session_id: &SessionId) -> Result<SessionSnapshot, StorageError>;

    async fn save_plan(&self, plan: &PlanDocument) -> Result<(), StorageError>;

    async fn load_plan(&self, plan_id: &PlanId) -> Result<PlanDocument, StorageError>;
}

/// No-op storage. Drops everything.
pub struct NoopStorage;

#[async_trait]
impl AgentStorage for NoopStorage {
    async fn append_event(
        &self,
        _session_id: &SessionId,
        _event: SdkEvent,
    ) -> Result<(), StorageError> {
        Ok(())
    }

    async fn load_session(&self, session_id: &SessionId) -> Result<SessionSnapshot, StorageError> {
        Ok(SessionSnapshot {
            session_id: session_id.clone(),
            events: Vec::new(),
        })
    }

    async fn save_plan(&self, _plan: &PlanDocument) -> Result<(), StorageError> {
        Ok(())
    }

    async fn load_plan(&self, plan_id: &PlanId) -> Result<PlanDocument, StorageError> {
        Err(StorageError::not_found(format!("plan {plan_id}")))
    }
}

/// In-memory storage. Keeps events and plans in two `HashMap`s for the
/// lifetime of the value. `SessionId` and `PlanId` implement `Hash` but
/// not `Ord`, so we use `HashMap` rather than `BTreeMap`. Useful in tests
/// and for short-lived web requests.
#[derive(Default)]
pub struct InMemoryStorage {
    sessions: RwLock<HashMap<SessionId, Vec<SdkEvent>>>,
    plans: RwLock<HashMap<PlanId, PlanDocument>>,
}

impl InMemoryStorage {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

#[async_trait]
impl AgentStorage for InMemoryStorage {
    async fn append_event(
        &self,
        session_id: &SessionId,
        event: SdkEvent,
    ) -> Result<(), StorageError> {
        let mut g = self.sessions.write().await;
        g.entry(session_id.clone()).or_default().push(event);
        Ok(())
    }

    async fn load_session(&self, session_id: &SessionId) -> Result<SessionSnapshot, StorageError> {
        let g = self.sessions.read().await;
        let events = g.get(session_id).cloned().unwrap_or_default();
        Ok(SessionSnapshot {
            session_id: session_id.clone(),
            events,
        })
    }

    async fn save_plan(&self, plan: &PlanDocument) -> Result<(), StorageError> {
        let mut g = self.plans.write().await;
        g.insert(plan.id.clone(), plan.clone());
        Ok(())
    }

    async fn load_plan(&self, plan_id: &PlanId) -> Result<PlanDocument, StorageError> {
        let g = self.plans.read().await;
        g.get(plan_id)
            .cloned()
            .ok_or_else(|| StorageError::not_found(format!("plan {plan_id}")))
    }
}

/// JSONL-backed storage. Sessions live under `<root>/sessions/<id>.jsonl`;
/// plans live under `<root>/plans/<id>.json`. No locking beyond a per-call
/// tokio `RwLock` so two writes serialize.
pub struct FileStorage {
    root: PathBuf,
    write_lock: RwLock<()>,
}

impl FileStorage {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, StorageError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(root.join("sessions"))
            .map_err(|e| StorageError::io(e.to_string()))?;
        std::fs::create_dir_all(root.join("plans")).map_err(|e| StorageError::io(e.to_string()))?;
        Ok(Self {
            root,
            write_lock: RwLock::new(()),
        })
    }

    fn session_path(&self, session_id: &SessionId) -> PathBuf {
        self.root
            .join("sessions")
            .join(format!("{}.jsonl", session_id))
    }

    fn plan_path(&self, plan_id: &PlanId) -> PathBuf {
        self.root.join("plans").join(format!("{}.json", plan_id))
    }
}

#[async_trait]
impl AgentStorage for FileStorage {
    async fn append_event(
        &self,
        session_id: &SessionId,
        event: SdkEvent,
    ) -> Result<(), StorageError> {
        let _g = self.write_lock.write().await;
        let line = serde_json::to_string(&event)
            .map_err(|e| StorageError::serialization(e.to_string()))?;
        let path = self.session_path(session_id);
        let mut content = String::new();
        content.push_str(&line);
        content.push('\n');
        // Async-friendly append.
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await
            .map_err(|e| StorageError::io(e.to_string()))?;
        file.write_all(content.as_bytes())
            .await
            .map_err(|e| StorageError::io(e.to_string()))?;
        file.flush()
            .await
            .map_err(|e| StorageError::io(e.to_string()))?;
        Ok(())
    }

    async fn load_session(&self, session_id: &SessionId) -> Result<SessionSnapshot, StorageError> {
        let path = self.session_path(session_id);
        if !path.exists() {
            return Ok(SessionSnapshot {
                session_id: session_id.clone(),
                events: Vec::new(),
            });
        }
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| StorageError::io(e.to_string()))?;
        let mut events = Vec::new();
        for line in content.lines() {
            if line.is_empty() {
                continue;
            }
            let event: SdkEvent = serde_json::from_str(line)
                .map_err(|e| StorageError::serialization(e.to_string()))?;
            events.push(event);
        }
        Ok(SessionSnapshot {
            session_id: session_id.clone(),
            events,
        })
    }

    async fn save_plan(&self, plan: &PlanDocument) -> Result<(), StorageError> {
        let _g = self.write_lock.write().await;
        let path = self.plan_path(&plan.id);
        let body = serde_json::to_string_pretty(plan)
            .map_err(|e| StorageError::serialization(e.to_string()))?;
        tokio::fs::write(&path, body)
            .await
            .map_err(|e| StorageError::io(e.to_string()))
    }

    async fn load_plan(&self, plan_id: &PlanId) -> Result<PlanDocument, StorageError> {
        let path = self.plan_path(plan_id);
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|_| StorageError::not_found(format!("plan {plan_id}")))?;
        serde_json::from_str(&body).map_err(|e| StorageError::serialization(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RunStarted;
    use code_agent_auth::ProviderId;

    fn ev(rid: &crate::RunId) -> SdkEvent {
        SdkEvent::RunStarted(RunStarted {
            run_id: rid.clone(),
            session_id: None,
            provider: ProviderId::Fake,
            model: "fake".into(),
            metadata: crate::request::RequestMetadata::default(),
        })
    }

    /// NoopStorage drops everything but returns success.
    #[tokio::test]
    async fn noop_returns_ok() {
        let s = NoopStorage;
        let sid = SessionId::default();
        let rid = crate::RunId::new();
        s.append_event(&sid, ev(&rid)).await.unwrap();
        let snap = s.load_session(&sid).await.unwrap();
        assert!(snap.events.is_empty());
    }

    /// InMemoryStorage round-trips events for a session.
    #[tokio::test]
    async fn in_memory_round_trip() {
        let s = InMemoryStorage::new();
        let sid = SessionId::default();
        let rid = crate::RunId::new();
        s.append_event(&sid, ev(&rid)).await.unwrap();
        let snap = s.load_session(&sid).await.unwrap();
        assert_eq!(snap.events.len(), 1);
    }

    /// FileStorage round-trips events to JSONL on disk.
    #[tokio::test]
    async fn file_storage_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let s = FileStorage::new(dir.path()).unwrap();
        let sid = SessionId::default();
        let rid = crate::RunId::new();
        s.append_event(&sid, ev(&rid)).await.unwrap();
        let snap = s.load_session(&sid).await.unwrap();
        assert_eq!(snap.events.len(), 1);
    }
}
