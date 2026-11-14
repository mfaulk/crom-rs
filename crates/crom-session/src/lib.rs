//! JSONL session persistence for crom.
//!
//! Sessions are stored under `<cwd>/.agent/sessions/<session-id>.jsonl`. Each
//! line is a `SessionRecord` JSON object. Records include event timestamps so
//! that resuming sessions and reconstructing context is deterministic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use crom_core::agent::SessionSink;
use crom_core::{AgentError, AgentEvent, Conversation, Message, MessageRole, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub mod migrations;
pub mod recovery;
pub use recovery::{
    acquire_lock, append_recovery_marker, classify_state, detect_state, load_records_resilient,
    lock_path_for, read_lock, release_lock, write_lock, RecoveryMarker, RunLock,
    SessionRecoveryInfo,
};

/// Current on-disk schema version stamped on every newly written
/// [`SessionRecord`]. Version `0` is reserved for pre-versioning JSONL that
/// predates this field (such records deserialize as v0 via
/// `default_schema_version`); version `1` is the first explicitly versioned
/// format.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// Default schema version applied when a JSONL record lacks the
/// `schema_version` field. Pre-versioning records (written before the field
/// existed) load as v0 so older sessions keep replaying.
fn default_schema_version() -> u32 {
    0
}

/// Lightweight view over a JSONL line that reads ONLY the `schema_version`
/// field, ignoring every other field. This lets the load/recovery paths
/// detect a future-version record *before* attempting the full
/// [`SessionRecord`] deserialize.
///
/// This matters because schema versioning exists to guard *future breaking
/// changes*. A future breaking change (a new [`SessionPayload`] variant, a
/// changed field type) makes `from_str::<SessionRecord>` FAIL — so without
/// this pre-parse probe a future-version line would fall into the "corrupt
/// line" path and be silently skipped instead of surfacing the loud
/// [`SessionError::UnsupportedSchemaVersion`].
#[derive(serde::Deserialize)]
struct SchemaVersionProbe {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
}

/// Pre-parse schema-version gate applied to every JSONL session line before
/// the full [`SessionRecord`] deserialize.
///
/// If the line parses as a [`SchemaVersionProbe`] and its `schema_version`
/// exceeds [`CURRENT_SCHEMA_VERSION`], this returns
/// [`SessionError::UnsupportedSchemaVersion`] so a future-version record is a
/// hard error rather than a skipped/corrupt line.
///
/// If the probe itself fails to parse (genuinely malformed JSON), this returns
/// `Ok(())`: the caller's existing handling decides whether to error or skip
/// the corrupt line. The version gate only ever *adds* a loud failure for
/// detected future versions; it never suppresses the resilient skip path for
/// same/old-version corruption.
pub(crate) fn check_schema_version_line(line: &str) -> Result<(), SessionError> {
    if let Ok(probe) = serde_json::from_str::<SchemaVersionProbe>(line) {
        if probe.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedSchemaVersion {
                found: probe.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("session not found: {0}")]
    NotFound(String),
    #[error("invalid session id")]
    InvalidId,
    /// A record was written by a newer build whose schema version is ahead of
    /// what this build understands. Loading is refused rather than silently
    /// dropping fields the newer format may carry.
    #[error(
        "session record schema version {found} is newer than this build supports \
         (max {supported}); upgrade the agent to load this session"
    )]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// On-disk schema version for this record. Absent in pre-versioning JSONL,
    /// which deserializes as v0 (see `default_schema_version`).
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub timestamp: String,
    pub cwd: PathBuf,
    pub session_id: String,
    pub payload: SessionPayload,
}

impl SessionRecord {
    /// Construct a record stamped with [`CURRENT_SCHEMA_VERSION`]. All write
    /// paths build records through this so new JSONL is always versioned.
    pub fn new(
        timestamp: String,
        cwd: PathBuf,
        session_id: String,
        payload: SessionPayload,
    ) -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            timestamp,
            cwd,
            session_id,
            payload,
        }
    }

    /// Validate a freshly deserialized record's schema version against this
    /// build. Records at or below [`CURRENT_SCHEMA_VERSION`] are accepted (and
    /// migrated forward through [`migrations::migrate_record`]); records from a
    /// newer build fail loudly with [`SessionError::UnsupportedSchemaVersion`].
    pub fn validate_schema_version(self) -> Result<Self, SessionError> {
        if self.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: CURRENT_SCHEMA_VERSION,
            });
        }
        let from = self.schema_version;
        migrations::migrate_record(self, from)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionPayload {
    UserPrompt {
        text: String,
    },
    Event(AgentEvent),
    /// Marker recorded once at session creation when forking.
    ForkMetadata {
        source_session_id: String,
    },
    /// Snapshot of the effective settings used to start the run.
    SettingsSnapshot {
        settings_summary: String,
    },
    /// M6: marker recorded once at the head of a child-session JSONL file
    /// so audits can recover the parent linkage from the child alone.
    SubagentMetadata {
        parent_session_id: String,
        parent_run_id: String,
        parent_tool_call_id: String,
        agent_name: String,
    },
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, cwd: &Path) -> Result<SessionId, SessionError>;
    async fn append(
        &self,
        session_id: &SessionId,
        event: SessionRecord,
    ) -> Result<(), SessionError>;
    async fn load(&self, session_id: &SessionId) -> Result<Vec<SessionRecord>, SessionError>;
    async fn latest_for_cwd(&self, cwd: &Path) -> Result<Option<SessionId>, SessionError>;
    async fn list(&self, cwd: &Path) -> Result<Vec<SessionId>, SessionError>;
}

pub struct FsSessionStore {
    cwd: PathBuf,
}

impl FsSessionStore {
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn sessions_dir_for(cwd: &Path) -> PathBuf {
        cwd.join(".agent").join("sessions")
    }

    /// Directory used for subagent (child) session JSONL files for a given
    /// parent session.
    pub fn child_sessions_dir_for(cwd: &Path, parent: &SessionId) -> Result<PathBuf, SessionError> {
        if !parent.is_valid_format() {
            return Err(SessionError::InvalidId);
        }
        Ok(Self::sessions_dir_for(cwd)
            .join("children")
            .join(parent.as_str()))
    }

    fn file_path(&self, session_id: &SessionId) -> Result<PathBuf, SessionError> {
        if !session_id.is_valid_format() {
            return Err(SessionError::InvalidId);
        }
        Ok(Self::sessions_dir_for(&self.cwd).join(format!("{}.jsonl", session_id.as_str())))
    }

    /// Resolve the path to a child session's JSONL file given its parent.
    fn child_file_path(
        &self,
        parent: &SessionId,
        child: &SessionId,
    ) -> Result<PathBuf, SessionError> {
        if !child.is_valid_format() {
            return Err(SessionError::InvalidId);
        }
        Ok(Self::child_sessions_dir_for(&self.cwd, parent)?
            .join(format!("{}.jsonl", child.as_str())))
    }

    /// Append a single record to a child session, creating the parent
    /// directory if needed.
    pub async fn append_child(
        &self,
        parent: &SessionId,
        child: &SessionId,
        event: SessionRecord,
    ) -> Result<(), SessionError> {
        let path = self.child_file_path(parent, child)?;
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// List child session ids for a given parent.
    pub async fn list_children(&self, parent: &SessionId) -> Result<Vec<SessionId>, SessionError> {
        let dir = match Self::child_sessions_dir_for(&self.cwd, parent) {
            Ok(p) => p,
            Err(e) => return Err(e),
        };
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push(SessionId::from(stem.to_string()));
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }

    /// Load records for a child session.
    pub async fn load_child(
        &self,
        parent: &SessionId,
        child: &SessionId,
    ) -> Result<Vec<SessionRecord>, SessionError> {
        let path = self.child_file_path(parent, child)?;
        if !path.exists() {
            return Err(SessionError::NotFound(child.to_string()));
        }
        let f = tokio::fs::File::open(&path).await?;
        let mut reader = BufReader::new(f).lines();
        let mut out = Vec::new();
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            // Pre-parse version gate: a future-version line is a hard error
            // even if it would fail the full SessionRecord parse below.
            check_schema_version_line(&line)?;
            let rec: SessionRecord = serde_json::from_str(&line)?;
            out.push(rec.validate_schema_version()?);
        }
        Ok(out)
    }

    /// Find the parent session id for a child by scanning the children/
    /// directory tree. Returns `None` if the child is unknown.
    pub async fn find_parent_for_child(
        &self,
        child: &SessionId,
    ) -> Result<Option<SessionId>, SessionError> {
        let root = Self::sessions_dir_for(&self.cwd).join("children");
        if !root.exists() {
            return Ok(None);
        }
        let mut rd = tokio::fs::read_dir(&root).await?;
        while let Some(ent) = rd.next_entry().await? {
            let parent_dir = ent.path();
            if !parent_dir.is_dir() {
                continue;
            }
            let parent_name = match parent_dir.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let parent_id = SessionId::from(parent_name);
            if !parent_id.is_valid_format() {
                continue;
            }
            let candidate = parent_dir.join(format!("{}.jsonl", child.as_str()));
            if candidate.exists() {
                return Ok(Some(parent_id));
            }
        }
        Ok(None)
    }
}

#[async_trait]
impl SessionStore for FsSessionStore {
    async fn create(&self, _cwd: &Path) -> Result<SessionId, SessionError> {
        let id = SessionId::new();
        let dir = Self::sessions_dir_for(&self.cwd);
        tokio::fs::create_dir_all(&dir).await?;
        // Touch the file so it shows up in `list` and `latest_for_cwd`.
        let path = self.file_path(&id)?;
        tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(id)
    }

    async fn append(
        &self,
        session_id: &SessionId,
        event: SessionRecord,
    ) -> Result<(), SessionError> {
        // Validate first so a bad id never reaches `create_dir_all` or the
        // filesystem at all.
        let path = self.file_path(session_id)?;
        let dir = Self::sessions_dir_for(&self.cwd);
        tokio::fs::create_dir_all(&dir).await?;
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    async fn load(&self, session_id: &SessionId) -> Result<Vec<SessionRecord>, SessionError> {
        let path = self.file_path(session_id)?;
        if !path.exists() {
            return Err(SessionError::NotFound(session_id.to_string()));
        }
        let f = tokio::fs::File::open(&path).await?;
        let mut reader = BufReader::new(f).lines();
        let mut out = Vec::new();
        while let Some(line) = reader.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            // Pre-parse version gate: a future-version line is a hard error
            // even if it would fail the full SessionRecord parse below.
            check_schema_version_line(&line)?;
            let rec: SessionRecord = serde_json::from_str(&line)?;
            out.push(rec.validate_schema_version()?);
        }
        Ok(out)
    }

    async fn latest_for_cwd(&self, _cwd: &Path) -> Result<Option<SessionId>, SessionError> {
        let dir = Self::sessions_dir_for(&self.cwd);
        if !dir.exists() {
            return Ok(None);
        }
        let mut newest: Option<(std::time::SystemTime, SessionId)> = None;
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = match p.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let mtime = ent
                .metadata()
                .await
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(std::time::UNIX_EPOCH);
            if newest.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                newest = Some((mtime, SessionId::from(stem)));
            }
        }
        Ok(newest.map(|(_, s)| s))
    }

    async fn list(&self, _cwd: &Path) -> Result<Vec<SessionId>, SessionError> {
        let dir = Self::sessions_dir_for(&self.cwd);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push(SessionId::from(stem.to_string()));
            }
        }
        out.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(out)
    }
}

/// Adapter that connects a `SessionStore` to the agent's `SessionSink` trait.
pub struct SessionSinkAdapter {
    store: Arc<dyn SessionStore>,
    cwd: PathBuf,
}

impl SessionSinkAdapter {
    pub fn new(store: Arc<dyn SessionStore>, cwd: PathBuf) -> Self {
        Self { store, cwd }
    }
}

#[async_trait]
impl SessionSink for SessionSinkAdapter {
    async fn record(&self, session_id: &SessionId, event: &AgentEvent) -> Result<(), AgentError> {
        let record = SessionRecord::new(
            now_iso(),
            self.cwd.clone(),
            session_id.to_string(),
            SessionPayload::Event(event.clone()),
        );
        self.store
            .append(session_id, record)
            .await
            .map_err(|e| AgentError::Session(format!("{e}")))
    }
}

/// SessionSink that persists subagent records under a child-session path
/// rooted at the parent's session id. Used as the inner sink when M6
/// orchestration runs.
pub struct ChildSessionSinkAdapter {
    store: Arc<FsSessionStore>,
    cwd: PathBuf,
    parent_session_id: SessionId,
}

impl ChildSessionSinkAdapter {
    pub fn new(store: Arc<FsSessionStore>, cwd: PathBuf, parent_session_id: SessionId) -> Self {
        Self {
            store,
            cwd,
            parent_session_id,
        }
    }
}

#[async_trait]
impl SessionSink for ChildSessionSinkAdapter {
    async fn record(&self, session_id: &SessionId, event: &AgentEvent) -> Result<(), AgentError> {
        let record = SessionRecord::new(
            now_iso(),
            self.cwd.clone(),
            session_id.to_string(),
            SessionPayload::Event(event.clone()),
        );
        self.store
            .append_child(&self.parent_session_id, session_id, record)
            .await
            .map_err(|e| AgentError::Session(format!("{e}")))
    }
}

/// Factory that hands out a fresh `ChildSessionSinkAdapter` per child run,
/// satisfying [`crom_core::ChildSessionSinkFactory`]. The factory
/// owns one `FsSessionStore` instance and the cwd, so the parent can
/// configure it once and the orchestrator can mint sinks per child.
pub struct FsChildSessionSinkFactory {
    store: Arc<FsSessionStore>,
    cwd: PathBuf,
}

impl FsChildSessionSinkFactory {
    pub fn new(store: Arc<FsSessionStore>, cwd: PathBuf) -> Self {
        Self { store, cwd }
    }
}

impl crom_core::ChildSessionSinkFactory for FsChildSessionSinkFactory {
    fn child_sink(
        &self,
        parent_session_id: &SessionId,
        _child_session_id: &SessionId,
    ) -> Arc<dyn SessionSink> {
        Arc::new(ChildSessionSinkAdapter::new(
            self.store.clone(),
            self.cwd.clone(),
            parent_session_id.clone(),
        ))
    }

    fn write_metadata(
        &self,
        parent_session_id: &SessionId,
        child_session_id: &SessionId,
        metadata: &crom_core::ChildSessionMetadata,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crom_core::AgentError>> + Send + '_>,
    > {
        let parent = parent_session_id.clone();
        let child = child_session_id.clone();
        let cwd = self.cwd.clone();
        let store = self.store.clone();
        let payload = SessionPayload::SubagentMetadata {
            parent_session_id: metadata.parent_session_id.to_string(),
            parent_run_id: metadata.parent_run_id.to_string(),
            parent_tool_call_id: metadata.parent_tool_call_id.clone(),
            agent_name: metadata.agent_name.clone(),
        };
        Box::pin(async move {
            let record = SessionRecord::new(now_iso(), cwd, child.to_string(), payload);
            store
                .append_child(&parent, &child, record)
                .await
                .map_err(|e| crom_core::AgentError::Session(format!("{e}")))
        })
    }
}

pub fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Append a freeform user prompt to the session log. Convenient for the CLI.
pub async fn append_user_prompt(
    store: &dyn SessionStore,
    cwd: &Path,
    session_id: &SessionId,
    prompt: &str,
) -> Result<(), SessionError> {
    let record = SessionRecord::new(
        now_iso(),
        cwd.to_path_buf(),
        session_id.to_string(),
        SessionPayload::UserPrompt {
            text: prompt.to_string(),
        },
    );
    store.append(session_id, record).await
}

/// Append a fork-metadata record so the new session knows where it came from.
pub async fn append_fork_metadata(
    store: &dyn SessionStore,
    cwd: &Path,
    new_session_id: &SessionId,
    source_session_id: &SessionId,
) -> Result<(), SessionError> {
    let record = SessionRecord::new(
        now_iso(),
        cwd.to_path_buf(),
        new_session_id.to_string(),
        SessionPayload::ForkMetadata {
            source_session_id: source_session_id.to_string(),
        },
    );
    store.append(new_session_id, record).await
}

/// Append a settings snapshot so audits can reproduce the run.
pub async fn append_settings_snapshot(
    store: &dyn SessionStore,
    cwd: &Path,
    session_id: &SessionId,
    settings_summary: &str,
) -> Result<(), SessionError> {
    let record = SessionRecord::new(
        now_iso(),
        cwd.to_path_buf(),
        session_id.to_string(),
        SessionPayload::SettingsSnapshot {
            settings_summary: settings_summary.to_string(),
        },
    );
    store.append(session_id, record).await
}

/// Fork an existing session into a new one. Copies every record from the
/// source session into a fresh session id and writes a fork-metadata marker.
pub async fn fork_session(
    store: &dyn SessionStore,
    cwd: &Path,
    source: &SessionId,
) -> Result<SessionId, SessionError> {
    let records = store.load(source).await?;
    let new_id = store.create(cwd).await?;
    append_fork_metadata(store, cwd, &new_id, source).await?;
    for r in records {
        // Skip the source's own lineage/provenance markers. We write a fresh
        // ForkMetadata above; copying the source's ForkMetadata/
        // SettingsSnapshot/SubagentMetadata would corrupt lineage, since
        // `summarize_session` resolves `source_session_id` from whichever
        // ForkMetadata it encounters. Normal message/event records are kept.
        if matches!(
            r.payload,
            SessionPayload::ForkMetadata { .. }
                | SessionPayload::SettingsSnapshot { .. }
                | SessionPayload::SubagentMetadata { .. }
        ) {
            continue;
        }
        // Keep the original timestamp but rewrite the session id so the
        // new file references itself, not the source. The copy is a freshly
        // written record, so it is stamped with the current schema version.
        let copied = SessionRecord::new(
            r.timestamp,
            cwd.to_path_buf(),
            new_id.to_string(),
            r.payload,
        );
        store.append(&new_id, copied).await?;
    }
    Ok(new_id)
}

/// Summary of a session (computed by walking its records).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionSummary {
    pub session_id: String,
    pub cwd: Option<PathBuf>,
    pub source_session_id: Option<String>,
    pub user_prompts: u32,
    pub assistant_messages: u32,
    pub tool_calls_started: u32,
    pub tool_calls_completed: u32,
    pub tool_calls_denied: u32,
    pub policy_decisions: u32,
    pub hook_evaluations: u32,
    pub todo_updates: u32,
    pub interruptions: u32,
    pub final_result: Option<String>,
    pub final_subtype: Option<String>,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: Option<f64>,
}

/// Compute a summary for a session by reading its JSONL.
pub async fn summarize_session(
    store: &dyn SessionStore,
    session_id: &SessionId,
) -> Result<SessionSummary, SessionError> {
    let records = store.load(session_id).await?;
    let mut s = SessionSummary {
        session_id: session_id.to_string(),
        ..Default::default()
    };
    // Each AgentResult event with a run_id reports cumulative cost for its
    // run, so summing every event would double-count whenever a run emits
    // more than one Result. Keep the last reported cost per run id and sum
    // those at the end.
    //
    // Legacy logs (and any Result lacking a run_id) cannot be bucketed by
    // run: collapsing every `None` into a single map entry would silently
    // drop all but the last value. Instead, treat each None-tagged result
    // as an independent contribution and add it to `uncounted_sum`. This
    // can over-count if multiple None-tagged events came from the same run
    // (each reporting cumulative cost), but the alternative — dropping
    // values — silently undercounts, which is worse for a billing-adjacent
    // summary. Sessions written by this codebase always include a run_id,
    // so this fallback only affects legacy fixtures.
    let mut last_cost_per_run: std::collections::HashMap<crom_core::RunId, f64> =
        std::collections::HashMap::new();
    let mut uncounted_sum: f64 = 0.0;
    let mut saw_any_cost = false;
    // Token usage is cumulative per run just like cost, so it must be
    // bucketed the same way: keep the last `TokenUsage` per run id and add
    // up the run-less ones, then sum the buckets at the end. Summing on
    // every Result double-counts whenever a run emits more than one Result.
    let mut last_usage_per_run: std::collections::HashMap<crom_core::RunId, crom_core::TokenUsage> =
        std::collections::HashMap::new();
    let mut uncounted_usage = crom_core::TokenUsage::default();
    for r in &records {
        if s.cwd.is_none() {
            s.cwd = Some(r.cwd.clone());
        }
        match &r.payload {
            SessionPayload::UserPrompt { .. } => s.user_prompts += 1,
            SessionPayload::ForkMetadata { source_session_id } => {
                s.source_session_id = Some(source_session_id.clone());
            }
            SessionPayload::SettingsSnapshot { .. } => {}
            SessionPayload::SubagentMetadata { .. } => {}
            SessionPayload::Event(ev) => match ev {
                AgentEvent::AssistantMessage(_) => s.assistant_messages += 1,
                AgentEvent::ToolCallStarted(_) => s.tool_calls_started += 1,
                AgentEvent::ToolCallCompleted(_) => s.tool_calls_completed += 1,
                AgentEvent::ToolCallDenied(_) => s.tool_calls_denied += 1,
                AgentEvent::PolicyDecision(_) => s.policy_decisions += 1,
                AgentEvent::HookEvaluated(_) => s.hook_evaluations += 1,
                AgentEvent::TodoUpdate(_) => s.todo_updates += 1,
                AgentEvent::Interrupted(_) => s.interruptions += 1,
                AgentEvent::Result(res) => {
                    s.final_result = res.result.clone();
                    s.final_subtype = Some(format!("{:?}", res.subtype));
                    if let Some(u) = &res.usage {
                        match &res.run_id {
                            // Overwrite, not add: cumulative usage per run,
                            // mirroring `last_cost_per_run`.
                            Some(run_id) => {
                                last_usage_per_run.insert(run_id.clone(), u.clone());
                            }
                            // No run id to bucket by — accumulate directly,
                            // mirroring `uncounted_sum`.
                            None => uncounted_usage.add(u),
                        }
                    }
                    if let Some(c) = res.total_cost_usd {
                        saw_any_cost = true;
                        match &res.run_id {
                            Some(run_id) => {
                                // Overwrite, not add: each Result is
                                // cumulative for its run. Records appear in
                                // append order, so the final write per run
                                // id wins.
                                last_cost_per_run.insert(run_id.clone(), c);
                            }
                            None => {
                                // No run id to bucket by — accumulate
                                // directly. See note on `uncounted_sum`
                                // above.
                                uncounted_sum += c;
                            }
                        }
                    }
                }
                _ => {}
            },
        }
    }
    if saw_any_cost {
        let bucketed: f64 = last_cost_per_run.values().sum();
        s.total_cost_usd = Some(bucketed + uncounted_sum);
    }
    // Sum the per-run usage buckets plus the run-less accumulator. The summary
    // only tracks input/output totals (no cache fields), so collapse to those.
    let mut total_usage = uncounted_usage;
    for u in last_usage_per_run.values() {
        total_usage.add(u);
    }
    s.total_input_tokens = total_usage.input_tokens;
    s.total_output_tokens = total_usage.output_tokens;
    Ok(s)
}

/// Export format for `sessions export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Jsonl,
    Json,
}

impl ExportFormat {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "jsonl" => Some(Self::Jsonl),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Export a session as a single string in the requested format.
pub async fn export_session(
    store: &dyn SessionStore,
    session_id: &SessionId,
    format: ExportFormat,
) -> Result<String, SessionError> {
    let records = store.load(session_id).await?;
    let s = match format {
        ExportFormat::Jsonl => {
            let mut out = String::new();
            for r in records {
                out.push_str(&serde_json::to_string(&r)?);
                out.push('\n');
            }
            out
        }
        ExportFormat::Json => serde_json::to_string_pretty(&records)?,
    };
    Ok(s)
}

/// Reconstruct prior conversation messages from a session log so the agent can
/// resume with full context.
///
/// The reconstruction takes the perspective of the model's input messages: for
/// each prior turn we include the user prompt and the assistant message that
/// followed (text + tool calls), then a synthetic user message with the tool
/// results. This is intentionally simple and matches what the agent loop
/// produces during a fresh run.
pub fn replay_conversation(records: &[SessionRecord]) -> Conversation {
    use crom_core::{AssistantContent, ContentBlock, ToolResultContent};

    let mut conv = Conversation::new();
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

    // Before transitioning away from the last assistant turn (either to the
    // next assistant turn or to a new user prompt or to the end of the
    // transcript), synthesize cancelled tool_results for any tool_use blocks
    // in the most recent assistant message that have no matching entry in
    // `pending_tool_results`. Without this repair the Anthropic API would
    // reject the resumed conversation because every tool_use must be
    // answered. We do this for *every* assistant turn, not just the tail,
    // so that interior dangling tool_use blocks (e.g. two assistant turns
    // recorded back-to-back without intervening tool results) are also
    // repaired. Tool_use blocks are preserved — we never drop them — so the
    // model can see what was attempted.
    let repair_dangling = |conv: &Conversation, pending: &mut Vec<ContentBlock>| {
        let Some(last) = conv.messages.last() else {
            return;
        };
        if last.role != MessageRole::Assistant {
            return;
        }
        let already_answered: std::collections::HashSet<String> = pending
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult(tr) => Some(tr.tool_use_id.clone()),
                _ => None,
            })
            .collect();
        for b in &last.content {
            if let ContentBlock::ToolUse(tc) = b {
                if !already_answered.contains(&tc.id) {
                    pending.push(ContentBlock::ToolResult(crom_core::ConvToolResult {
                        tool_use_id: tc.id.clone(),
                        content: vec![ToolResultContent::Text {
                            text: "cancelled - agent interrupted".to_string(),
                        }],
                        is_error: true,
                    }));
                }
            }
        }
    };

    let flush_results = |conv: &mut Conversation, pending: &mut Vec<ContentBlock>| {
        if !pending.is_empty() {
            conv.push(Message {
                role: MessageRole::User,
                content: std::mem::take(pending),
            });
        }
    };

    for r in records {
        match &r.payload {
            SessionPayload::UserPrompt { text } => {
                repair_dangling(&conv, &mut pending_tool_results);
                flush_results(&mut conv, &mut pending_tool_results);
                conv.push(Message::user_text(text.clone()));
            }
            SessionPayload::ForkMetadata { .. }
            | SessionPayload::SettingsSnapshot { .. }
            | SessionPayload::SubagentMetadata { .. } => {}
            SessionPayload::Event(ev) => match ev {
                AgentEvent::AssistantMessage(am) => {
                    repair_dangling(&conv, &mut pending_tool_results);
                    flush_results(&mut conv, &mut pending_tool_results);
                    let mut blocks: Vec<ContentBlock> = Vec::new();
                    for b in &am.content {
                        match b {
                            AssistantContent::Text { text } => {
                                blocks.push(ContentBlock::Text { text: text.clone() });
                            }
                            AssistantContent::ToolUse(c) => {
                                blocks.push(ContentBlock::ToolUse(c.clone()));
                            }
                        }
                    }
                    conv.push(Message {
                        role: MessageRole::Assistant,
                        content: blocks,
                    });
                }
                AgentEvent::UserToolResult(r) => {
                    pending_tool_results.push(ContentBlock::ToolResult(
                        crom_core::ConvToolResult {
                            tool_use_id: r.tool_use_id.clone(),
                            content: vec![ToolResultContent::Text {
                                text: r.content.clone(),
                            }],
                            is_error: r.is_error,
                        },
                    ));
                }
                _ => {}
            },
        }
    }
    // Tail repair: if the transcript ended right after an AssistantMessage
    // with unmatched tool_use blocks, synthesize cancelled results for them
    // before flushing the final pending user message.
    repair_dangling(&conv, &mut pending_tool_results);
    flush_results(&mut conv, &mut pending_tool_results);
    conv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::{AgentResult, AssistantMessage, ResultSubtype, RunId, SessionId};
    use tempfile::TempDir;

    #[tokio::test]
    // FsSessionStore can create, append to, and load a JSONL session.
    async fn round_trip_session() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        append_user_prompt(&store, dir.path(), &id, "hello")
            .await
            .unwrap();
        let event = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![crom_core::AssistantContent::Text { text: "hi".into() }],
            stop_reason: Some("end_turn".into()),
        });
        let rec = SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            id.to_string(),
            SessionPayload::Event(event),
        );
        store.append(&id, rec).await.unwrap();

        let records = store.load(&id).await.unwrap();
        assert_eq!(records.len(), 2);
        match &records[0].payload {
            SessionPayload::UserPrompt { text } => assert_eq!(text, "hello"),
            _ => panic!("first record should be user prompt"),
        }
    }

    #[tokio::test]
    // latest_for_cwd returns the most recently modified session id.
    async fn latest_for_cwd_returns_most_recent() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id1 = store.create(dir.path()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let id2 = store.create(dir.path()).await.unwrap();
        let latest = store.latest_for_cwd(dir.path()).await.unwrap().unwrap();
        assert_ne!(id1.as_str(), id2.as_str());
        assert_eq!(latest.as_str(), id2.as_str());
    }

    #[tokio::test]
    // load with `..`-style id returns InvalidId rather than escaping the
    // sessions directory.
    async fn load_rejects_traversal_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let bad = SessionId::from("../etc/passwd".to_string());
        let err = store
            .load(&bad)
            .await
            .expect_err("traversal id must be rejected");
        assert!(
            matches!(err, SessionError::InvalidId),
            "expected InvalidId, got {err:?}"
        );
    }

    #[tokio::test]
    // append with a slash-containing id errors out instead of writing
    // outside the sessions directory.
    async fn append_rejects_slash_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let bad = SessionId::from("foo/bar".to_string());
        let rec = SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            bad.to_string(),
            SessionPayload::UserPrompt {
                text: "x".to_string(),
            },
        );
        let err = store
            .append(&bad, rec)
            .await
            .expect_err("slash id must be rejected");
        assert!(
            matches!(err, SessionError::InvalidId),
            "expected InvalidId, got {err:?}"
        );
        // Confirm no stray file/directory was created above the sessions dir.
        let stray = dir.path().join("foo");
        assert!(
            !stray.exists(),
            "append must not create paths outside the sessions dir"
        );
    }

    // Helper: build a single SessionRecord with the given id.
    fn rec_for(dir: &TempDir, id: &SessionId) -> SessionRecord {
        SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            id.to_string(),
            SessionPayload::UserPrompt {
                text: "x".to_string(),
            },
        )
    }

    #[tokio::test]
    // The original CVE: `../foo` would have written `.agent/foo.jsonl`,
    // one level above the sessions dir. Verify the would-be target file
    // is not created.
    async fn append_rejects_dotdot_id_no_filesystem_side_effects() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let bad = SessionId::from("../foo".to_string());
        let err = store
            .append(&bad, rec_for(&dir, &bad))
            .await
            .expect_err("traversal id must be rejected");
        assert!(matches!(err, SessionError::InvalidId), "got {err:?}");
        // The vulnerable path would have been `.agent/foo.jsonl`.
        let escaped = dir.path().join(".agent").join("foo.jsonl");
        assert!(!escaped.exists(), "must not create file above sessions dir");
    }

    #[cfg(unix)]
    #[tokio::test]
    // An absolute-path id used to replace the joined prefix entirely on
    // Unix. Must error and must not write to the absolute target.
    async fn append_rejects_absolute_id() {
        let outer = TempDir::new().unwrap();
        let target = outer.path().join("escape.jsonl");
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        // The ID encodes an absolute path with a separator the join would have
        // honored. Validation must reject it before reaching the filesystem.
        let bad_str = format!("/{}", target.strip_prefix("/").unwrap().display());
        let bad = SessionId::from(bad_str);
        let err = store
            .append(&bad, rec_for(&dir, &bad))
            .await
            .expect_err("absolute-path id must be rejected");
        assert!(matches!(err, SessionError::InvalidId), "got {err:?}");
        assert!(
            !target.exists(),
            "must not write to absolute path provided as session id"
        );
    }

    #[tokio::test]
    // Backslash in id (Windows-style separator) is also rejected.
    async fn append_rejects_backslash_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let bad = SessionId::from("foo\\bar".to_string());
        let err = store
            .append(&bad, rec_for(&dir, &bad))
            .await
            .expect_err("backslash id must be rejected");
        assert!(matches!(err, SessionError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    // NUL byte in id is rejected (would corrupt path syscalls).
    async fn append_rejects_nul_byte_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let bad = SessionId::from("foo\0bar".to_string());
        let err = store
            .append(&bad, rec_for(&dir, &bad))
            .await
            .expect_err("NUL byte id must be rejected");
        assert!(matches!(err, SessionError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    // create() with an externally-provided invalid id must never produce
    // a file outside the sessions dir. (FsSessionStore::create currently
    // generates a fresh uuid internally, so this is a smoke test that the
    // store can never persist an invalid id even if a buggy caller tries.)
    async fn append_rejects_unicode_fullwidth_slash_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        // U+FF0F FULLWIDTH SOLIDUS is visually similar to `/` but isn't ASCII;
        // the alphanumeric allowlist rejects it.
        let bad = SessionId::from("foo\u{FF0F}bar".to_string());
        let err = store
            .append(&bad, rec_for(&dir, &bad))
            .await
            .expect_err("unicode-slash id must be rejected");
        assert!(matches!(err, SessionError::InvalidId), "got {err:?}");
    }

    #[tokio::test]
    // replay_conversation reconstructs user/assistant/tool-result messages.
    async fn replay_includes_prior_context() {
        let _ = ResultSubtype::Success;
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        append_user_prompt(&store, dir.path(), &id, "do thing")
            .await
            .unwrap();
        let am = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![crom_core::AssistantContent::Text { text: "ok".into() }],
            stop_reason: None,
        });
        store
            .append(
                &id,
                SessionRecord::new(
                    now_iso(),
                    dir.path().to_path_buf(),
                    id.to_string(),
                    SessionPayload::Event(am),
                ),
            )
            .await
            .unwrap();

        let recs = store.load(&id).await.unwrap();
        let conv = replay_conversation(&recs);
        assert_eq!(conv.messages.len(), 2);
    }

    // Build a SessionRecord wrapping an AgentResult event with the given run id
    // and cumulative cost. Used by the cost-accounting tests.
    fn result_record(
        dir: &TempDir,
        session_id: &SessionId,
        run_id: Option<&str>,
        cost: f64,
    ) -> SessionRecord {
        let res = AgentResult {
            subtype: ResultSubtype::Success,
            result: None,
            session_id: session_id.clone(),
            run_id: run_id.map(|s| RunId(s.to_string())),
            num_turns: 1,
            usage: None,
            total_cost_usd: Some(cost),
        };
        SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            session_id.to_string(),
            SessionPayload::Event(AgentEvent::Result(res)),
        )
    }

    #[tokio::test]
    // summarize_session aggregates cost as (last per run_id), summed across runs.
    // Two distinct runs are recorded; run A reports cumulative 0.10 then 0.20,
    // run B reports 0.05. The session total must be 0.25 (0.20 + 0.05), not
    // 0.35 (sum of all events) or 0.30 (sum-of-deltas miscomputed).
    async fn summary_cost_takes_last_per_run() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(&id, result_record(&dir, &id, Some("run-a"), 0.10))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, Some("run-a"), 0.20))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, Some("run-b"), 0.05))
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        let total = summary.total_cost_usd.expect("cost should be reported");
        // Floating-point compare with epsilon to avoid representation noise.
        assert!(
            (total - 0.25).abs() < 1e-9,
            "expected 0.25 (last of run-a + last of run-b), got {total}"
        );
    }

    #[tokio::test]
    // Legacy logs may emit AgentResult events without a run_id. Each such event
    // must contribute its full cost to the total — collapsing them into a
    // single Option<None> bucket would silently drop all but the last value.
    async fn summary_cost_sums_results_without_run_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(&id, result_record(&dir, &id, None, 0.10))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, None, 0.20))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, None, 0.05))
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        let total = summary.total_cost_usd.expect("cost should be reported");
        // None-tagged results cannot be bucketed by run, so each contributes
        // its reported value. Expected: 0.10 + 0.20 + 0.05 = 0.35.
        assert!(
            (total - 0.35).abs() < 1e-9,
            "expected 0.35 (sum of all None-tagged results), got {total}"
        );
    }

    #[tokio::test]
    // Mixed log: a real run_id reports cumulative 0.10 then 0.20 (last wins,
    // contributing 0.20), and a legacy None-tagged result contributes 0.05.
    // The total must be 0.25 — bucket Some(run_id), sum None.
    async fn summary_cost_mixed_run_id_and_none() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(&id, result_record(&dir, &id, Some("run-a"), 0.10))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, Some("run-a"), 0.20))
            .await
            .unwrap();
        store
            .append(&id, result_record(&dir, &id, None, 0.05))
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        let total = summary.total_cost_usd.expect("cost should be reported");
        assert!(
            (total - 0.25).abs() < 1e-9,
            "expected 0.25 (last of run-a + sum of None), got {total}"
        );
    }

    // Build a SessionRecord wrapping an AgentResult event carrying cumulative
    // token usage for the given run id. Used by the token-accounting tests.
    fn result_record_with_usage(
        dir: &TempDir,
        session_id: &SessionId,
        run_id: Option<&str>,
        input_tokens: u64,
        output_tokens: u64,
    ) -> SessionRecord {
        let res = AgentResult {
            subtype: ResultSubtype::Success,
            result: None,
            session_id: session_id.clone(),
            run_id: run_id.map(|s| RunId(s.to_string())),
            num_turns: 1,
            usage: Some(crom_core::TokenUsage {
                input_tokens,
                output_tokens,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            total_cost_usd: None,
        };
        SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            session_id.to_string(),
            SessionPayload::Event(AgentEvent::Result(res)),
        )
    }

    #[tokio::test]
    // Token usage is cumulative per run, so two Results sharing one run_id must
    // collapse to the last reported value, not the sum. Run A reports 100/10
    // then a later cumulative 250/30; the session total must equal the last
    // (250/30), not 350/40.
    async fn summary_tokens_take_last_per_run() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(
                &id,
                result_record_with_usage(&dir, &id, Some("run-a"), 100, 10),
            )
            .await
            .unwrap();
        store
            .append(
                &id,
                result_record_with_usage(&dir, &id, Some("run-a"), 250, 30),
            )
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        assert_eq!(summary.total_input_tokens, 250);
        assert_eq!(summary.total_output_tokens, 30);
    }

    #[tokio::test]
    // Distinct run_ids contribute independently; their last-per-run usages sum.
    // Run A's last is 250/30, run B's last is 40/5, so the total is 290/35.
    async fn summary_tokens_sum_across_runs() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(
                &id,
                result_record_with_usage(&dir, &id, Some("run-a"), 100, 10),
            )
            .await
            .unwrap();
        store
            .append(
                &id,
                result_record_with_usage(&dir, &id, Some("run-a"), 250, 30),
            )
            .await
            .unwrap();
        store
            .append(
                &id,
                result_record_with_usage(&dir, &id, Some("run-b"), 40, 5),
            )
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        assert_eq!(summary.total_input_tokens, 290);
        assert_eq!(summary.total_output_tokens, 35);
    }

    #[tokio::test]
    // Results without a run_id cannot be bucketed, so each contributes its
    // full usage: 100/10 + 40/5 = 140/15.
    async fn summary_tokens_sum_results_without_run_id() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        store
            .append(&id, result_record_with_usage(&dir, &id, None, 100, 10))
            .await
            .unwrap();
        store
            .append(&id, result_record_with_usage(&dir, &id, None, 40, 5))
            .await
            .unwrap();

        let summary = summarize_session(&store, &id).await.unwrap();
        assert_eq!(summary.total_input_tokens, 140);
        assert_eq!(summary.total_output_tokens, 15);
    }

    #[tokio::test]
    // replay_conversation must not leave a tail tool_use block without a
    // matching tool_result; a session killed mid-flight would otherwise be
    // rejected by the Anthropic API on resume. The reconstructed conversation
    // must either drop the dangling tool_use blocks from the tail assistant
    // message OR end with a synthetic tool_result block for each tool_use_id.
    async fn replay_synthesizes_cancelled_tool_results_for_dangling_tool_uses() {
        use crom_core::{AssistantContent, ContentBlock, ToolCall};

        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        // A user prompt followed by an assistant message with two tool_use
        // blocks, then session end (no UserToolResult events).
        append_user_prompt(&store, dir.path(), &id, "do two things")
            .await
            .unwrap();
        let am = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![
                AssistantContent::Text {
                    text: "okay".into(),
                },
                AssistantContent::ToolUse(ToolCall {
                    id: "call_a".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"cmd": "ls"}),
                }),
                AssistantContent::ToolUse(ToolCall {
                    id: "call_b".into(),
                    name: "shell".into(),
                    input: serde_json::json!({"cmd": "pwd"}),
                }),
            ],
            stop_reason: Some("tool_use".into()),
        });
        store
            .append(
                &id,
                SessionRecord::new(
                    now_iso(),
                    dir.path().to_path_buf(),
                    id.to_string(),
                    SessionPayload::Event(am),
                ),
            )
            .await
            .unwrap();

        let recs = store.load(&id).await.unwrap();
        let conv = replay_conversation(&recs);

        // The conversation must be valid for an Anthropic resume: every
        // tool_use in the final assistant message has a matching tool_result
        // in the following user message, OR the tool_use blocks are absent.
        // Find the last assistant message.
        let last_assistant_idx = conv
            .messages
            .iter()
            .rposition(|m| m.role == MessageRole::Assistant)
            .expect("expected at least one assistant message");

        let unresolved_tool_use_ids: Vec<String> = conv.messages[last_assistant_idx]
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse(tc) => Some(tc.id.clone()),
                _ => None,
            })
            .collect();

        if unresolved_tool_use_ids.is_empty() {
            // Acceptable outcome: the tail tool_use blocks were dropped.
            return;
        }

        // Otherwise every tool_use must be answered by a tool_result in a
        // following user message.
        let mut answered: std::collections::HashSet<String> = std::collections::HashSet::new();
        for m in &conv.messages[last_assistant_idx + 1..] {
            if m.role != MessageRole::User {
                continue;
            }
            for b in &m.content {
                if let ContentBlock::ToolResult(tr) = b {
                    answered.insert(tr.tool_use_id.clone());
                }
            }
        }
        for id in &unresolved_tool_use_ids {
            assert!(
                answered.contains(id),
                "tool_use_id {id} has no matching tool_result; \
                 conversation: {conv:#?}"
            );
        }
    }

    #[tokio::test]
    // An interior assistant message can have unmatched tool_use blocks when
    // two assistant turns appear back-to-back with no UserToolResult events
    // recorded between them (e.g. orchestrator interrupted the first turn
    // and the model re-prompted). The replayed transcript must still be
    // valid for an Anthropic resume: every tool_use block in *any*
    // assistant message must be answered by a tool_result block in the
    // immediately following user message.
    async fn replay_synthesizes_results_for_interior_dangling_tool_use() {
        use crom_core::{AssistantContent, ContentBlock, ToolCall};

        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();

        append_user_prompt(&store, dir.path(), &id, "do stuff")
            .await
            .unwrap();
        // First assistant turn with a tool_use, but no following tool_result.
        let am1 = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::ToolUse(ToolCall {
                id: "call_a".into(),
                name: "shell".into(),
                input: serde_json::json!({"cmd": "ls"}),
            })],
            stop_reason: Some("tool_use".into()),
        });
        // Second assistant turn appears immediately after, also with a
        // tool_use, again with no tool_result. Then a final user message
        // (e.g. user follow-up) closes the session.
        let am2 = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::ToolUse(ToolCall {
                id: "call_b".into(),
                name: "shell".into(),
                input: serde_json::json!({"cmd": "pwd"}),
            })],
            stop_reason: Some("tool_use".into()),
        });
        for ev in [am1, am2] {
            store
                .append(
                    &id,
                    SessionRecord::new(
                        now_iso(),
                        dir.path().to_path_buf(),
                        id.to_string(),
                        SessionPayload::Event(ev),
                    ),
                )
                .await
                .unwrap();
        }
        append_user_prompt(&store, dir.path(), &id, "actually scratch that")
            .await
            .unwrap();

        let recs = store.load(&id).await.unwrap();
        let conv = replay_conversation(&recs);

        // For every assistant message, collect tool_use ids and assert they
        // are answered by tool_result blocks in the immediately following
        // user message.
        for (i, m) in conv.messages.iter().enumerate() {
            if m.role != MessageRole::Assistant {
                continue;
            }
            let tool_use_ids: Vec<String> = m
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse(tc) => Some(tc.id.clone()),
                    _ => None,
                })
                .collect();
            if tool_use_ids.is_empty() {
                continue;
            }
            let next = conv.messages.get(i + 1).unwrap_or_else(|| {
                panic!(
                    "assistant turn at index {i} has tool_use blocks but \
                     no following message; conversation: {conv:#?}"
                )
            });
            assert_eq!(
                next.role,
                MessageRole::User,
                "assistant turn at index {i} must be followed by a User \
                 message with tool_results; got {next:#?}"
            );
            let answered: std::collections::HashSet<String> = next
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult(tr) => Some(tr.tool_use_id.clone()),
                    _ => None,
                })
                .collect();
            for tu_id in &tool_use_ids {
                assert!(
                    answered.contains(tu_id),
                    "tool_use_id {tu_id} on assistant turn {i} has no \
                     matching tool_result in the next message; \
                     conversation: {conv:#?}"
                );
            }
        }
    }

    #[tokio::test]
    // Forking a session that already carries its own lineage/provenance
    // markers must not copy them into the fork: the fork gets exactly one
    // ForkMetadata (the fresh one) pointing at the immediate source, and the
    // source's SettingsSnapshot/SubagentMetadata are dropped.
    async fn fork_does_not_duplicate_metadata() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());

        // Build a source that is itself a fork of `grandparent` and carries a
        // settings snapshot plus a normal user prompt.
        let grandparent = store.create(dir.path()).await.unwrap();
        let source = store.create(dir.path()).await.unwrap();
        append_fork_metadata(&store, dir.path(), &source, &grandparent)
            .await
            .unwrap();
        append_settings_snapshot(&store, dir.path(), &source, "summary")
            .await
            .unwrap();
        append_user_prompt(&store, dir.path(), &source, "hello")
            .await
            .unwrap();

        let fork = fork_session(&store, dir.path(), &source).await.unwrap();
        let records = store.load(&fork).await.unwrap();

        let fork_metas: Vec<&str> = records
            .iter()
            .filter_map(|r| match &r.payload {
                SessionPayload::ForkMetadata { source_session_id } => {
                    Some(source_session_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            fork_metas,
            vec![source.as_str()],
            "fork must contain exactly one ForkMetadata pointing at the immediate source"
        );

        assert!(
            !records
                .iter()
                .any(|r| matches!(r.payload, SessionPayload::SettingsSnapshot { .. })),
            "source SettingsSnapshot must not be copied into the fork"
        );

        // The normal user prompt is still copied.
        assert!(
            records.iter().any(
                |r| matches!(&r.payload, SessionPayload::UserPrompt { text } if text == "hello")
            ),
            "normal records must still be copied into the fork"
        );

        // summarize_session resolves the correct immediate source.
        let summary = summarize_session(&store, &fork).await.unwrap();
        assert_eq!(summary.source_session_id.as_deref(), Some(source.as_str()));
    }

    // ------------------------------------------------------------------
    // Snapshot tests (insta): lock the FULL serialized on-disk JSONL
    // record shape so an unintended field rename/addition/removal in
    // SessionRecord / SessionPayload is caught.
    //
    // A fixed timestamp / cwd / session_id keep the snapshots
    // deterministic (the production `now_iso()` clock would otherwise
    // change every run).
    // ------------------------------------------------------------------

    /// Build a record with stable, non-clock-derived identity fields.
    fn fixed_record(payload: SessionPayload) -> SessionRecord {
        SessionRecord::new(
            "2026-06-04T00:00:00Z".to_string(),
            PathBuf::from("/work/project"),
            "sess-fixed-0001".to_string(),
            payload,
        )
    }

    #[test]
    // Snapshot a representative assistant-message Event record: the most
    // common payload, carrying text + a tool_use block.
    fn snapshot_assistant_message_record() {
        use crom_core::{AssistantContent, AssistantMessage, ToolCall};
        let rec = fixed_record(SessionPayload::Event(AgentEvent::AssistantMessage(
            AssistantMessage {
                content: vec![
                    AssistantContent::Text {
                        text: "Reading the file now.".into(),
                    },
                    AssistantContent::ToolUse(ToolCall {
                        id: "toolu_1".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "/etc/hosts"}),
                    }),
                ],
                stop_reason: Some("tool_use".into()),
            },
        )));
        insta::assert_json_snapshot!(rec);
    }

    #[test]
    // Snapshot the non-Event payload variants (the lineage/provenance
    // markers) so their wire tags and fields are locked too.
    fn snapshot_marker_records() {
        let user_prompt = fixed_record(SessionPayload::UserPrompt {
            text: "do the thing".into(),
        });
        let fork = fixed_record(SessionPayload::ForkMetadata {
            source_session_id: "sess-source-0000".into(),
        });
        let settings = fixed_record(SessionPayload::SettingsSnapshot {
            settings_summary: "model=claude-sonnet-4-6 permission=ask".into(),
        });
        let subagent = fixed_record(SessionPayload::SubagentMetadata {
            parent_session_id: "sess-parent-0000".into(),
            parent_run_id: "run-0000".into(),
            parent_tool_call_id: "toolu_parent".into(),
            agent_name: "code-reviewer".into(),
        });
        insta::assert_json_snapshot!("user_prompt_record", user_prompt);
        insta::assert_json_snapshot!("fork_metadata_record", fork);
        insta::assert_json_snapshot!("settings_snapshot_record", settings);
        insta::assert_json_snapshot!("subagent_metadata_record", subagent);
    }
}
