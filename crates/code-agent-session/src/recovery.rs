//! Run-lock and session-recovery helpers for M9.
//!
//! Lock files live next to session JSONL files at
//! `<cwd>/.agent/sessions/<session-id>.lock`. On normal termination the lock
//! is removed; if the process exits unexpectedly the next launch detects
//! the stale file and offers recovery.
//!
//! Recovery never mutates existing JSONL beyond appending a marker record.
//! Truncated/corrupted JSONL is handled by skipping malformed lines and
//! reporting the count.

use std::path::{Path, PathBuf};

use code_agent_core::{SessionId, SessionRuntimeState};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::AsyncBufReadExt;

use crate::{
    check_schema_version_line, FsSessionStore, SessionError, SessionPayload, SessionRecord,
};

/// Snapshot of an active run, persisted as a `*.lock` file alongside the
/// session JSONL. The file is short and human-readable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunLock {
    pub session_id: String,
    pub run_id: String,
    pub pid: u32,
    pub hostname: String,
    pub started_at: String,
}

impl RunLock {
    pub fn new(session_id: &SessionId, run_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            pid: std::process::id(),
            hostname: hostname(),
            started_at: now_iso(),
        }
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "localhost".to_string())
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Validate the session id then return its lock file path.
pub fn lock_path_for(cwd: &Path, session_id: &SessionId) -> Result<PathBuf, SessionError> {
    if !session_id.is_valid_format() {
        return Err(SessionError::InvalidId);
    }
    Ok(FsSessionStore::sessions_dir_for(cwd).join(format!("{}.lock", session_id.as_str())))
}

/// Acquire the run lock for a session. Atomic: uses `create_new` so two
/// processes that race to the same session id cannot both observe an empty
/// lock and both believe they own it. Returns `Ok` only if no lock was
/// previously held.
pub async fn acquire_lock(
    cwd: &Path,
    session_id: &SessionId,
    lock: &RunLock,
) -> Result<PathBuf, SessionError> {
    use tokio::io::AsyncWriteExt;
    let path = lock_path_for(cwd, session_id)?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let body = serde_json::to_string_pretty(lock)?;
    let mut file = match tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(SessionError::Other(format!(
                "lock already held at {}",
                path.display()
            )));
        }
        Err(e) => return Err(SessionError::Io(e)),
    };
    file.write_all(body.as_bytes()).await?;
    file.flush().await?;
    Ok(path)
}

/// Forcibly write the run lock, replacing any prior file. Used after a
/// recovery decision.
pub async fn write_lock(
    cwd: &Path,
    session_id: &SessionId,
    lock: &RunLock,
) -> Result<PathBuf, SessionError> {
    let path = lock_path_for(cwd, session_id)?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let body = serde_json::to_string_pretty(lock)?;
    tokio::fs::write(&path, body).await?;
    Ok(path)
}

/// Release the run lock, if present. Missing locks are not an error so the
/// shutdown path can be idempotent.
pub async fn release_lock(cwd: &Path, session_id: &SessionId) -> Result<(), SessionError> {
    let path = lock_path_for(cwd, session_id)?;
    if path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
    }
    Ok(())
}

/// Read the lock file, if present.
pub async fn read_lock(
    cwd: &Path,
    session_id: &SessionId,
) -> Result<Option<RunLock>, SessionError> {
    let path = lock_path_for(cwd, session_id)?;
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(&path).await?;
    let lock: RunLock = serde_json::from_slice(&bytes)?;
    Ok(Some(lock))
}

/// Result of [`detect_state`] for a single session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecoveryInfo {
    pub session_id: SessionId,
    pub state: SessionRuntimeState,
    pub lock: Option<RunLock>,
    pub last_modified_unix: Option<i64>,
}

/// Decide a session's runtime state given its JSONL records and (optional)
/// lock. The `now_unix` argument lets tests pin time deterministically.
pub fn classify_state(
    records: &[SessionRecord],
    lock: Option<&RunLock>,
    now_unix: i64,
    stale_after_seconds: u64,
) -> SessionRuntimeState {
    if records.iter().any(|r| {
        matches!(
            &r.payload,
            SessionPayload::Event(code_agent_core::AgentEvent::Result(_))
        )
    }) {
        // A Result event marks completion. If a lock survives that's just
        // staleness, not "running".
        return if lock.is_none() {
            SessionRuntimeState::Complete
        } else {
            SessionRuntimeState::Crashed
        };
    }
    match lock {
        None => SessionRuntimeState::Idle,
        Some(l) => {
            let started = parse_iso(&l.started_at).unwrap_or(now_unix);
            let elapsed = now_unix.saturating_sub(started);
            if elapsed >= 0 && (elapsed as u64) > stale_after_seconds {
                SessionRuntimeState::NeedsRecovery
            } else {
                SessionRuntimeState::Running
            }
        }
    }
}

fn parse_iso(s: &str) -> Option<i64> {
    // Full RFC 3339 parser. Returns the unix timestamp in seconds.
    // Anything we can't parse falls back to None so the caller treats
    // the lock as fresh — we'd rather under-recover than misjudge a
    // real run as stale.
    //
    // The previous hand-rolled implementation sliced the first 19
    // characters and called `assume_utc()` on the result, silently
    // dropping any timezone offset (`+HH:MM`, `-HH:MM`, or `Z`). A
    // RunLock written with `2026-05-17T13:00:00-08:00` was therefore
    // parsed as `13:00:00 UTC` — eight hours off, which flipped the
    // staleness verdict for any lock written outside UTC (#159).
    let s = s.trim();
    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|dt| dt.unix_timestamp())
}

/// Inspect every session under `cwd` and report runtime state.
///
/// Avoids loading full JSONL files when possible. Sessions without a lock
/// only need to be checked for a `Result` event by tail-scanning the file
/// (a Result is always near the end of a session's log); only sessions with
/// a held lock fall back to a full load to disambiguate Crashed vs Running.
pub async fn detect_state(
    cwd: &Path,
    stale_after_seconds: u64,
) -> Result<Vec<SessionRecoveryInfo>, SessionError> {
    let store = FsSessionStore::new(cwd.to_path_buf());
    let mut out = Vec::new();
    let now_unix = OffsetDateTime::now_utc().unix_timestamp();
    use crate::SessionStore;
    for id in store.list(cwd).await? {
        let lock = read_lock(cwd, &id).await?;
        let path = FsSessionStore::sessions_dir_for(cwd).join(format!("{}.jsonl", id.as_str()));
        let last_modified_unix = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64);
        // Bounded-I/O probe: read only the last `TAIL_BYTES` of the file
        // to decide whether this session has any records and whether it
        // ended in a `Result` event. This caps the scan at ~64KB even for
        // multi-megabyte session logs.
        //
        // A future-version record must propagate loudly so recovery does not
        // silently classify a session it cannot fully understand. Genuine I/O
        // errors (unreadable tail) stay best-effort: fall back to the default
        // probe so a transient read failure doesn't abort the whole scan.
        let probe = match tail_probe(&path).await {
            Ok(p) => p,
            Err(e @ SessionError::UnsupportedSchemaVersion { .. }) => return Err(e),
            Err(_) => TailProbe::default(),
        };
        let state = classify_state_lite(probe, lock.as_ref(), now_unix, stale_after_seconds);
        out.push(SessionRecoveryInfo {
            session_id: id,
            state,
            lock,
            last_modified_unix,
        });
    }
    out.sort_by(|a, b| {
        b.last_modified_unix
            .unwrap_or(0)
            .cmp(&a.last_modified_unix.unwrap_or(0))
    });
    Ok(out)
}

/// Lightweight summary of a session's JSONL file produced by `tail_probe`.
/// Used by `classify_state_lite` to distinguish "nothing happened yet" from
/// "session ran but never reached a Result event".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TailProbe {
    pub has_records: bool,
    pub has_result: bool,
}

/// Cheap state classifier that takes a precomputed [`TailProbe`] rather
/// than the full record list. Sessions with records but no `Result` event
/// and no held lock are reported as `Interrupted` so recovery flows can
/// pick them up.
pub(crate) fn classify_state_lite(
    probe: TailProbe,
    lock: Option<&RunLock>,
    now_unix: i64,
    stale_after_seconds: u64,
) -> SessionRuntimeState {
    if probe.has_result {
        return if lock.is_none() {
            SessionRuntimeState::Complete
        } else {
            SessionRuntimeState::Crashed
        };
    }
    match lock {
        None => {
            // No `Result` event and no lock: either nothing ever happened
            // (Idle) or the prior process exited without recording a
            // terminal state (Interrupted).
            if probe.has_records {
                SessionRuntimeState::Interrupted
            } else {
                SessionRuntimeState::Idle
            }
        }
        Some(l) => {
            let started = parse_iso(&l.started_at).unwrap_or(now_unix);
            let elapsed = now_unix.saturating_sub(started);
            if elapsed >= 0 && (elapsed as u64) > stale_after_seconds {
                SessionRuntimeState::NeedsRecovery
            } else {
                SessionRuntimeState::Running
            }
        }
    }
}

/// Read the tail of a session JSONL file (last `TAIL_BYTES`) and report
/// whether it contains records and whether the most recent terminal record
/// is a `Result` event. Tail-reading bounds I/O at ~64KB regardless of
/// session size, so `code-agent doctor` and `sessions-ext recover` stay
/// fast even on multi-megabyte session logs.
const TAIL_BYTES: u64 = 64 * 1024;

async fn tail_probe(path: &Path) -> Result<TailProbe, SessionError> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    if !path.exists() {
        return Ok(TailProbe::default());
    }
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    if len == 0 {
        return Ok(TailProbe::default());
    }
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).await?;
    let mut buf = Vec::with_capacity(TAIL_BYTES.min(len) as usize);
    f.read_to_end(&mut buf).await?;
    let text = String::from_utf8_lossy(&buf);
    // Parse newline-terminated JSON records from the tail. The first line
    // may be partial when we did not start from offset zero — drop it then.
    let lines: Vec<&str> = text.lines().collect();
    let skip_first = start > 0;
    let mut has_records = false;
    let mut has_result = false;
    for (idx, line) in lines.iter().enumerate() {
        if skip_first && idx == 0 {
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        has_records = true;
        // Pre-parse version gate, so recovery state detection rejects a
        // future-version session loudly instead of silently classifying it.
        // A future breaking change can make the full `SessionRecord` parse
        // below fail; the probe still reads `schema_version` and hard-errors.
        check_schema_version_line(trimmed)?;
        if !trimmed.contains("\"result\"") {
            continue;
        }
        if let Ok(rec) = serde_json::from_str::<SessionRecord>(trimmed) {
            if matches!(
                &rec.payload,
                SessionPayload::Event(code_agent_core::AgentEvent::Result(_))
            ) {
                has_result = true;
            }
        }
    }
    Ok(TailProbe {
        has_records,
        has_result,
    })
}

/// Resilient JSONL load: returns parsed records and the count of skipped
/// (corrupted/truncated) lines. Never fails on bad lines, since the recovery
/// flow needs partial reads of crashed sessions.
pub async fn load_records_resilient(
    cwd: &Path,
    session_id: &SessionId,
) -> Result<(Vec<SessionRecord>, u32), SessionError> {
    if !session_id.is_valid_format() {
        return Err(SessionError::InvalidId);
    }
    let path = FsSessionStore::sessions_dir_for(cwd).join(format!("{}.jsonl", session_id.as_str()));
    if !path.exists() {
        return Err(SessionError::NotFound(session_id.to_string()));
    }
    let f = tokio::fs::File::open(&path).await?;
    let mut reader = tokio::io::BufReader::new(f).lines();
    let mut out = Vec::new();
    let mut skipped: u32 = 0;
    while let Some(line) = reader.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        // Pre-parse version gate FIRST. A future-version line must be a hard
        // error, NOT a skipped "corrupt" line — even when a future breaking
        // change makes the full `SessionRecord` parse below fail. Without this
        // the resilient loader would silently drop future-version records into
        // `skipped`, defeating the whole point of versioning. The gate only
        // fires for lines whose `schema_version` is detectably > current; a
        // genuinely malformed line at a known version still parses as a probe
        // failure here (Ok) and falls through to the skip path below.
        check_schema_version_line(&line)?;
        match serde_json::from_str::<SessionRecord>(&line) {
            // A well-formed record from a newer build is not "corrupt" — it is
            // a record we must not silently drop. Surface it as a hard error
            // rather than counting it as a skipped/truncated line.
            Ok(rec) => out.push(rec.validate_schema_version()?),
            Err(_) => {
                skipped = skipped.saturating_add(1);
            }
        }
    }
    Ok((out, skipped))
}

/// Append a recovery marker so audits can see when this happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryMarker {
    pub kind: &'static str,
    pub previous_state: SessionRuntimeState,
    pub records_recovered: u32,
    pub records_skipped: u32,
}

/// Append a `SessionRecovered` event to a session's JSONL file. This is
/// the audit trail referenced in the recovery flow's documentation:
/// recovery never mutates existing JSONL beyond appending a marker
/// record. The append is idempotent in spirit — a repeated recover just
/// adds another marker.
pub async fn append_recovery_marker(
    cwd: &Path,
    session_id: &SessionId,
    previous_state: SessionRuntimeState,
    records_recovered: u32,
    records_skipped: u32,
) -> Result<(), SessionError> {
    use crate::SessionStore;
    let store = FsSessionStore::new(cwd.to_path_buf());
    let event = code_agent_core::AgentEvent::SessionRecovered(code_agent_core::SessionRecovered {
        session_id: session_id.clone(),
        previous_state,
        records_recovered,
        records_skipped,
    });
    let record = SessionRecord::new(
        now_iso(),
        cwd.to_path_buf(),
        session_id.to_string(),
        SessionPayload::Event(event),
    );
    store.append(session_id, record).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{append_user_prompt, FsSessionStore, SessionStore};
    use code_agent_core::{
        AgentEvent, AgentResult, ResultSubtype, RunId, SessionRuntimeState as S, TokenUsage,
    };
    use tempfile::TempDir;

    #[tokio::test]
    // Acquiring a lock writes a JSON file; releasing removes it.
    async fn lock_round_trip() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        let lock = RunLock::new(&id, "run-1");
        let path = acquire_lock(dir.path(), &id, &lock).await.unwrap();
        assert!(path.exists());
        let read = read_lock(dir.path(), &id).await.unwrap().unwrap();
        assert_eq!(read.run_id, "run-1");
        release_lock(dir.path(), &id).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    // The lock is acquired atomically: when many tasks race, exactly one wins.
    async fn lock_acquire_is_atomic() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        let path = dir.path().to_path_buf();
        let mut handles = Vec::new();
        for n in 0..16 {
            let id = id.clone();
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                let lock = RunLock::new(&id, &format!("run-{n}"));
                acquire_lock(&path, &id, &lock).await
            }));
        }
        let mut successes = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                successes += 1;
            }
        }
        assert_eq!(successes, 1, "exactly one acquire should win");
    }

    #[tokio::test]
    // A second acquire fails while the lock is held; write_lock force-replaces.
    async fn lock_held_blocks_acquire() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        let lock = RunLock::new(&id, "run-1");
        acquire_lock(dir.path(), &id, &lock).await.unwrap();
        let again = acquire_lock(dir.path(), &id, &lock).await;
        assert!(again.is_err(), "second acquire must fail");
        let lock2 = RunLock::new(&id, "run-2");
        write_lock(dir.path(), &id, &lock2).await.unwrap();
        let read = read_lock(dir.path(), &id).await.unwrap().unwrap();
        assert_eq!(read.run_id, "run-2");
    }

    #[tokio::test]
    // Idle sessions with no lock and no Result are classified Idle.
    async fn classify_idle() {
        let state = classify_state(&[], None, 1_000_000, 60);
        assert_eq!(state, S::Idle);
    }

    #[tokio::test]
    // Sessions with a Result but no lock are Complete.
    async fn classify_complete() {
        let id = SessionId::new();
        let rec = SessionRecord::new(
            now_iso(),
            PathBuf::from("/tmp"),
            id.to_string(),
            SessionPayload::Event(AgentEvent::Result(AgentResult {
                subtype: ResultSubtype::Success,
                result: None,
                session_id: id.clone(),
                run_id: Some(RunId::new()),
                num_turns: 0,
                usage: Some(TokenUsage::default()),
                total_cost_usd: None,
            })),
        );
        let state = classify_state(&[rec], None, 1_000_000, 60);
        assert_eq!(state, S::Complete);
    }

    #[tokio::test]
    // Stale lock past threshold marks the session for recovery.
    async fn classify_needs_recovery_when_lock_stale() {
        let id = SessionId::new();
        let mut lock = RunLock::new(&id, "r");
        // Choose a fixed past timestamp the parser handles deterministically.
        lock.started_at = "2020-01-01T00:00:00Z".to_string();
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let state = classify_state(&[], Some(&lock), now, 60);
        assert_eq!(state, S::NeedsRecovery);
    }

    #[tokio::test]
    // Lock + Result in the JSONL means the process crashed *after* completion
    // recording but before clean shutdown.
    async fn classify_crashed_when_result_and_lock() {
        let id = SessionId::new();
        let lock = RunLock::new(&id, "r");
        let rec = SessionRecord::new(
            now_iso(),
            PathBuf::from("/tmp"),
            id.to_string(),
            SessionPayload::Event(AgentEvent::Result(AgentResult {
                subtype: ResultSubtype::Success,
                result: None,
                session_id: id.clone(),
                run_id: None,
                num_turns: 0,
                usage: None,
                total_cost_usd: None,
            })),
        );
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let state = classify_state(&[rec], Some(&lock), now, 60);
        assert_eq!(state, S::Crashed);
    }

    #[tokio::test]
    // A session with records but no Result event and no held lock is
    // classified as Interrupted. Regression test for the classifier
    // hole where these sessions were previously reported as Idle and
    // would never surface to recovery flows.
    async fn classify_interrupted_when_records_no_result_no_lock() {
        let probe = TailProbe {
            has_records: true,
            has_result: false,
        };
        let state = classify_state_lite(probe, None, 1_000_000, 60);
        assert_eq!(state, S::Interrupted);
    }

    #[tokio::test]
    // tail_probe correctly reports Result presence even when the file is
    // larger than the configured tail window.
    async fn tail_probe_finds_result_in_large_file() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        // Pad the file with > TAIL_BYTES of unrelated records.
        for i in 0..2000 {
            append_user_prompt(&store, dir.path(), &id, &format!("padding line number {i}"))
                .await
                .unwrap();
        }
        // Then write a Result event at the very end.
        let rec = SessionRecord::new(
            now_iso(),
            dir.path().to_path_buf(),
            id.to_string(),
            SessionPayload::Event(AgentEvent::Result(AgentResult {
                subtype: ResultSubtype::Success,
                result: None,
                session_id: id.clone(),
                run_id: None,
                num_turns: 0,
                usage: None,
                total_cost_usd: None,
            })),
        );
        store.append(&id, rec).await.unwrap();
        let path =
            FsSessionStore::sessions_dir_for(dir.path()).join(format!("{}.jsonl", id.as_str()));
        let probe = tail_probe(&path).await.unwrap();
        assert!(probe.has_records);
        assert!(probe.has_result);
    }

    #[tokio::test]
    // Truncated JSONL still loads as many records as possible and reports
    // the skipped-line count.
    async fn resilient_load_handles_truncation() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id = store.create(dir.path()).await.unwrap();
        append_user_prompt(&store, dir.path(), &id, "first")
            .await
            .unwrap();
        append_user_prompt(&store, dir.path(), &id, "second")
            .await
            .unwrap();
        // Append a truncated line by hand.
        let p = FsSessionStore::sessions_dir_for(dir.path()).join(format!("{}.jsonl", id.as_str()));
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.extend_from_slice(b"{\"timestamp\":\"truncated...");
        std::fs::write(&p, bytes).unwrap();

        let (recs, skipped) = load_records_resilient(dir.path(), &id).await.unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(skipped, 1);
    }

    #[tokio::test]
    // detect_state returns one entry per session and reports state correctly.
    // (We don't depend on fs-mtime resolution for ordering — the sort is
    // best-effort for newest-first display.)
    async fn detect_state_lists_sessions() {
        let dir = TempDir::new().unwrap();
        let store = FsSessionStore::new(dir.path());
        let id1 = store.create(dir.path()).await.unwrap();
        let id2 = store.create(dir.path()).await.unwrap();
        let lock = RunLock::new(&id1, "r");
        write_lock(dir.path(), &id1, &lock).await.unwrap();
        let infos = detect_state(dir.path(), 3600).await.unwrap();
        assert_eq!(infos.len(), 2);
        let id1_info = infos
            .iter()
            .find(|i| i.session_id.as_str() == id1.as_str())
            .unwrap();
        assert!(id1_info.lock.is_some());
        assert_eq!(id1_info.state, S::Running);
        let id2_info = infos
            .iter()
            .find(|i| i.session_id.as_str() == id2.as_str())
            .unwrap();
        assert!(id2_info.lock.is_none());
        assert_eq!(id2_info.state, S::Idle);
    }

    // Regression for #159: a non-UTC RFC 3339 timestamp must be
    // converted to its UTC unix timestamp, not interpreted as UTC.
    // The previous implementation sliced the leading 19 chars and
    // called `assume_utc()`, so `2026-05-17T13:00:00-08:00` was
    // misread as `2026-05-17T13:00:00Z` — eight hours off.
    #[test]
    fn parse_iso_honors_timezone_offset() {
        let pst = "2026-05-17T13:00:00-08:00";
        let utc = "2026-05-17T21:00:00Z";
        let pst_ts = super::parse_iso(pst).expect("pst parses");
        let utc_ts = super::parse_iso(utc).expect("utc parses");
        assert_eq!(
            pst_ts, utc_ts,
            "RFC 3339 offset must be applied: PST 13:00 == UTC 21:00"
        );
    }

    // Subsecond precision (`...000000Z`) and the `+00:00` synonym for
    // UTC must also parse.
    #[test]
    fn parse_iso_handles_subsecond_and_plus_zero_offsets() {
        let with_subsec = "2026-05-17T13:00:00.123456789Z";
        let with_plus_zero = "2026-05-17T13:00:00+00:00";
        let plain_z = "2026-05-17T13:00:00Z";
        let a = super::parse_iso(with_subsec).expect("subsec parses");
        let b = super::parse_iso(with_plus_zero).expect("plus zero parses");
        let c = super::parse_iso(plain_z).expect("plain z parses");
        assert_eq!(a, b);
        assert_eq!(b, c);
    }

    // Garbage / truncated input still falls back to None.
    #[test]
    fn parse_iso_rejects_invalid_input() {
        assert!(super::parse_iso("not a date").is_none());
        assert!(super::parse_iso("2026-05-17").is_none());
        assert!(super::parse_iso("").is_none());
    }
}
