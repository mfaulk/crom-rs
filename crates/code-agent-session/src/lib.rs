//! JSONL session persistence for code-agent.
//!
//! Sessions are stored under `<cwd>/.agent/sessions/<session-id>.jsonl`. Each
//! line is a `SessionRecord` JSON object. Records include event timestamps so
//! that resuming sessions and reconstructing context is deterministic.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use code_agent_core::agent::SessionSink;
use code_agent_core::{AgentError, AgentEvent, Conversation, Message, MessageRole, SessionId};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub timestamp: String,
    pub cwd: PathBuf,
    pub session_id: String,
    pub payload: SessionPayload,
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

    fn file_path(&self, session_id: &SessionId) -> Result<PathBuf, SessionError> {
        if !session_id.is_valid_format() {
            return Err(SessionError::InvalidId);
        }
        Ok(Self::sessions_dir_for(&self.cwd).join(format!("{}.jsonl", session_id.as_str())))
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
            let rec: SessionRecord = serde_json::from_str(&line)?;
            out.push(rec);
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
        let record = SessionRecord {
            timestamp: now_iso(),
            cwd: self.cwd.clone(),
            session_id: session_id.to_string(),
            payload: SessionPayload::Event(event.clone()),
        };
        self.store
            .append(session_id, record)
            .await
            .map_err(|e| AgentError::Session(format!("{e}")))
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
    let record = SessionRecord {
        timestamp: now_iso(),
        cwd: cwd.to_path_buf(),
        session_id: session_id.to_string(),
        payload: SessionPayload::UserPrompt {
            text: prompt.to_string(),
        },
    };
    store.append(session_id, record).await
}

/// Append a fork-metadata record so the new session knows where it came from.
pub async fn append_fork_metadata(
    store: &dyn SessionStore,
    cwd: &Path,
    new_session_id: &SessionId,
    source_session_id: &SessionId,
) -> Result<(), SessionError> {
    let record = SessionRecord {
        timestamp: now_iso(),
        cwd: cwd.to_path_buf(),
        session_id: new_session_id.to_string(),
        payload: SessionPayload::ForkMetadata {
            source_session_id: source_session_id.to_string(),
        },
    };
    store.append(new_session_id, record).await
}

/// Append a settings snapshot so audits can reproduce the run.
pub async fn append_settings_snapshot(
    store: &dyn SessionStore,
    cwd: &Path,
    session_id: &SessionId,
    settings_summary: &str,
) -> Result<(), SessionError> {
    let record = SessionRecord {
        timestamp: now_iso(),
        cwd: cwd.to_path_buf(),
        session_id: session_id.to_string(),
        payload: SessionPayload::SettingsSnapshot {
            settings_summary: settings_summary.to_string(),
        },
    };
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
        // Keep the original timestamp but rewrite the session id so the
        // new file references itself, not the source.
        let copied = SessionRecord {
            timestamp: r.timestamp,
            cwd: cwd.to_path_buf(),
            session_id: new_id.to_string(),
            payload: r.payload,
        };
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
                        s.total_input_tokens += u.input_tokens;
                        s.total_output_tokens += u.output_tokens;
                    }
                    // Each Result reports the cumulative cost for its run.
                    // Summing across runs yields the session-wide total;
                    // overwriting (the old behavior) under-reported cost
                    // for any session with more than one run (resumed,
                    // continued, forked, or chat sessions).
                    if let Some(c) = res.total_cost_usd {
                        s.total_cost_usd = Some(s.total_cost_usd.unwrap_or(0.0) + c);
                    }
                }
                _ => {}
            },
        }
    }
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
    use code_agent_core::{AssistantContent, ContentBlock, ToolResultContent};

    let mut conv = Conversation::new();
    let mut pending_tool_results: Vec<ContentBlock> = Vec::new();

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
                flush_results(&mut conv, &mut pending_tool_results);
                conv.push(Message::user_text(text.clone()));
            }
            SessionPayload::ForkMetadata { .. } | SessionPayload::SettingsSnapshot { .. } => {}
            SessionPayload::Event(ev) => match ev {
                AgentEvent::AssistantMessage(am) => {
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
                        code_agent_core::ConvToolResult {
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
    flush_results(&mut conv, &mut pending_tool_results);
    conv
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::{AssistantMessage, ResultSubtype};
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
            content: vec![code_agent_core::AssistantContent::Text { text: "hi".into() }],
            stop_reason: Some("end_turn".into()),
        });
        let rec = SessionRecord {
            timestamp: now_iso(),
            cwd: dir.path().to_path_buf(),
            session_id: id.to_string(),
            payload: SessionPayload::Event(event),
        };
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
        let rec = SessionRecord {
            timestamp: now_iso(),
            cwd: dir.path().to_path_buf(),
            session_id: bad.to_string(),
            payload: SessionPayload::UserPrompt {
                text: "x".to_string(),
            },
        };
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
        SessionRecord {
            timestamp: now_iso(),
            cwd: dir.path().to_path_buf(),
            session_id: id.to_string(),
            payload: SessionPayload::UserPrompt {
                text: "x".to_string(),
            },
        }
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
            content: vec![code_agent_core::AssistantContent::Text { text: "ok".into() }],
            stop_reason: None,
        });
        store
            .append(
                &id,
                SessionRecord {
                    timestamp: now_iso(),
                    cwd: dir.path().to_path_buf(),
                    session_id: id.to_string(),
                    payload: SessionPayload::Event(am),
                },
            )
            .await
            .unwrap();

        let recs = store.load(&id).await.unwrap();
        let conv = replay_conversation(&recs);
        assert_eq!(conv.messages.len(), 2);
    }
}
