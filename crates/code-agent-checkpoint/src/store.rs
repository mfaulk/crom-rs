use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use code_agent_core::{is_valid_session_id_str, RunId, SessionId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;

use crate::diff::{CheckpointDiff, DiffCheckpointRequest};
use crate::types::{
    CheckpointId, CheckpointLimits, CheckpointMetadata, CheckpointSummary, FileSnapshotKind,
    GitWorkspaceState, RewindReport, SkippedRewind, UserMessageId, WorkspaceRelativePath,
};

#[derive(Debug, Error)]
pub enum CheckpointError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid session id")]
    InvalidSessionId,

    #[error("invalid checkpoint id")]
    InvalidCheckpointId,

    #[error("checkpoint not found: {0}")]
    NotFound(String),

    #[error("path outside workspace: {0}")]
    PathEscape(String),

    #[error("file too large: {path} is {size} bytes, exceeds limit {limit}")]
    FileTooLarge { path: String, size: u64, limit: u64 },

    #[error("session checkpoint storage exceeds limit: {used} > {limit} bytes")]
    SessionStorageLimit { used: u64, limit: u64 },

    #[error("blob missing for hash {0}")]
    BlobMissing(String),

    #[error("hash mismatch for blob {0}")]
    HashMismatch(String),

    #[error("malformed checkpoint manifest at {0}")]
    MalformedManifest(String),

    #[error("cwd mismatch: checkpoint cwd {checkpoint_cwd:?}, current cwd {current_cwd:?}")]
    CwdMismatch {
        checkpoint_cwd: PathBuf,
        current_cwd: PathBuf,
    },

    #[error("invalid checkpoint configuration: {0}")]
    InvalidConfig(String),

    #[error("{0}")]
    Other(String),
}

/// Request to create a fresh checkpoint covering the current state.
#[derive(Debug, Clone)]
pub struct CreateCheckpointRequest {
    pub session_id: SessionId,
    pub run_id: RunId,
    pub cwd: PathBuf,
    pub description: Option<String>,
    pub user_message_id: Option<UserMessageId>,
    pub git_state: Option<GitWorkspaceState>,
}

/// Capture pre-mutation snapshots for the given paths into an existing
/// checkpoint.
#[derive(Debug, Clone)]
pub struct CaptureBeforeToolRequest {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub run_id: RunId,
    pub cwd: PathBuf,
    pub tool_name: String,
    pub tool_input: serde_json::Value,
    pub paths: Vec<WorkspaceRelativePath>,
}

/// Request a rewind to the given checkpoint.
#[derive(Debug, Clone)]
pub struct RewindRequest {
    pub checkpoint_id: CheckpointId,
    pub cwd: PathBuf,
    pub force_cwd: bool,
    pub dry_run: bool,
}

#[async_trait]
pub trait CheckpointStore: Send + Sync {
    async fn create_checkpoint(
        &self,
        request: CreateCheckpointRequest,
    ) -> Result<CheckpointId, CheckpointError>;

    async fn capture_before_tool(
        &self,
        request: CaptureBeforeToolRequest,
    ) -> Result<Vec<FileSnapshotKind>, CheckpointError>;

    async fn rewind(&self, request: RewindRequest) -> Result<RewindReport, CheckpointError>;

    async fn list_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<CheckpointSummary>, CheckpointError>;

    async fn get_checkpoint(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<CheckpointMetadata, CheckpointError>;

    async fn diff_checkpoint(
        &self,
        request: DiffCheckpointRequest,
    ) -> Result<CheckpointDiff, CheckpointError>;
}

/// On-disk index entry for one checkpoint within a session. Stored as one
/// JSON object per line in `checkpoints.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    checkpoint_id: CheckpointId,
    session_id: SessionId,
    run_id: RunId,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(default)]
    files_tracked_count: usize,
}

/// Local filesystem-backed implementation of [`CheckpointStore`].
pub struct LocalCheckpointStore {
    /// Workspace root (cwd). Checkpoints are stored under
    /// `<root>/.agent/checkpoints/<session-id>/`.
    root: PathBuf,
    limits: CheckpointLimits,
    /// Per-session async mutexes serializing index reads-then-writes.
    /// The index is `checkpoints.jsonl`; it's updated by
    /// `create_checkpoint` (append + trim oldest), `refresh_index_count`
    /// (read-modify-write), and `delete_checkpoint` (read-modify-write).
    /// Without this lock, a refresh after capture could clobber a newly
    /// appended entry.
    index_locks: tokio::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
    /// Per-(session, checkpoint) async mutexes that serialize manifest
    /// read-modify-writes inside `capture_before_tool`. Without this,
    /// two concurrent captures targeting the same checkpoint can both
    /// read the manifest, each extend their own copy, and the second
    /// writer overwrites the first writer's `files_tracked` entries.
    /// Keyed by "<session_id>/<checkpoint_id>".
    manifest_locks: tokio::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
    /// Cached per-session total blob bytes, populated lazily so
    /// `capture_one_path` doesn't have to re-scan the blobs directory on
    /// every snapshot. We update incrementally when a new blob is
    /// written.
    storage_cache: tokio::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl LocalCheckpointStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            limits: CheckpointLimits::default(),
            index_locks: tokio::sync::Mutex::new(Default::default()),
            manifest_locks: tokio::sync::Mutex::new(Default::default()),
            storage_cache: tokio::sync::Mutex::new(Default::default()),
        }
    }

    pub fn with_limits(root: PathBuf, limits: CheckpointLimits) -> Self {
        Self {
            root,
            limits,
            index_locks: tokio::sync::Mutex::new(Default::default()),
            manifest_locks: tokio::sync::Mutex::new(Default::default()),
            storage_cache: tokio::sync::Mutex::new(Default::default()),
        }
    }

    /// Returns (and lazily creates) the per-session index lock.
    async fn index_lock_for(
        &self,
        session_id: &SessionId,
    ) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let mut g = self.index_locks.lock().await;
        g.entry(session_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Returns (and lazily creates) the per-(session, checkpoint) manifest
    /// lock. `capture_before_tool` holds this for the read-modify-write
    /// window so concurrent captures on the same checkpoint don't clobber
    /// each other's `files_tracked` entries.
    async fn manifest_lock_for(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        let key = format!("{}/{}", session_id, checkpoint_id);
        let mut g = self.manifest_locks.lock().await;
        g.entry(key)
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> &CheckpointLimits {
        &self.limits
    }

    /// Returns the directory holding all checkpoints for `session_id`.
    pub fn session_dir_for(&self, session_id: &SessionId) -> Result<PathBuf, CheckpointError> {
        validate_session_id(session_id)?;
        Ok(self
            .root
            .join(".agent")
            .join("checkpoints")
            .join(session_id.as_str()))
    }

    /// Returns the index file (`checkpoints.jsonl`) for a session.
    fn index_path_for(&self, session_id: &SessionId) -> Result<PathBuf, CheckpointError> {
        Ok(self.session_dir_for(session_id)?.join("checkpoints.jsonl"))
    }

    fn manifests_dir_for(&self, session_id: &SessionId) -> Result<PathBuf, CheckpointError> {
        Ok(self.session_dir_for(session_id)?.join("manifests"))
    }

    fn blobs_dir_for(&self, session_id: &SessionId) -> Result<PathBuf, CheckpointError> {
        Ok(self.session_dir_for(session_id)?.join("blobs"))
    }

    fn manifest_path(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<PathBuf, CheckpointError> {
        validate_checkpoint_id(checkpoint_id)?;
        Ok(self
            .manifests_dir_for(session_id)?
            .join(format!("{}.json", checkpoint_id.as_str())))
    }

    fn blob_path(&self, session_id: &SessionId, hash: &str) -> Result<PathBuf, CheckpointError> {
        validate_blob_hash(hash)?;
        Ok(self.blobs_dir_for(session_id)?.join(hash))
    }

    async fn ensure_dirs(&self, session_id: &SessionId) -> Result<(), CheckpointError> {
        let manifests = self.manifests_dir_for(session_id)?;
        tokio::fs::create_dir_all(&manifests).await?;
        let blobs = self.blobs_dir_for(session_id)?;
        tokio::fs::create_dir_all(&blobs).await?;
        Ok(())
    }

    async fn append_index(
        &self,
        session_id: &SessionId,
        entry: &IndexEntry,
    ) -> Result<(), CheckpointError> {
        let path = self.index_path_for(session_id)?;
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        file.write_all(line.as_bytes()).await?;
        file.flush().await?;
        Ok(())
    }

    /// Load the index for a session. Missing index file yields an empty Vec.
    async fn load_index(&self, session_id: &SessionId) -> Result<Vec<IndexEntry>, CheckpointError> {
        let path = self.index_path_for(session_id)?;
        if !path.exists() {
            return Ok(Vec::new());
        }
        let bytes = tokio::fs::read(&path).await?;
        let text = String::from_utf8_lossy(&bytes);
        let mut out = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let entry: IndexEntry = serde_json::from_str(line).map_err(|_| {
                CheckpointError::MalformedManifest(format!("{}:{}", path.display(), i + 1))
            })?;
            out.push(entry);
        }
        Ok(out)
    }

    async fn read_manifest(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<CheckpointMetadata, CheckpointError> {
        let path = self.manifest_path(session_id, checkpoint_id)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CheckpointError::NotFound(checkpoint_id.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        let meta: CheckpointMetadata = serde_json::from_slice(&bytes)
            .map_err(|_| CheckpointError::MalformedManifest(path.display().to_string()))?;
        Ok(meta)
    }

    async fn write_manifest(
        &self,
        session_id: &SessionId,
        meta: &CheckpointMetadata,
    ) -> Result<(), CheckpointError> {
        let path = self.manifest_path(session_id, &meta.checkpoint_id)?;
        let bytes = serde_json::to_vec_pretty(meta)?;
        atomic_write(&path, &bytes).await?;
        Ok(())
    }

    /// Returns the number of bytes used by the session's blobs directory.
    /// Cached: the first call rescans the directory; subsequent capture
    /// operations bump the cached total without rescanning.
    async fn session_storage_bytes(&self, session_id: &SessionId) -> Result<u64, CheckpointError> {
        {
            let g = self.storage_cache.lock().await;
            if let Some(v) = g.get(session_id.as_str()) {
                return Ok(*v);
            }
        }
        let blobs = self.blobs_dir_for(session_id)?;
        let mut total: u64 = 0;
        if blobs.exists() {
            let mut rd = tokio::fs::read_dir(&blobs).await?;
            while let Some(ent) = rd.next_entry().await? {
                if let Ok(meta) = ent.metadata().await {
                    if meta.is_file() {
                        total = total.saturating_add(meta.len());
                    }
                }
            }
        }
        let mut g = self.storage_cache.lock().await;
        g.insert(session_id.to_string(), total);
        Ok(total)
    }

    /// Bump the cached session storage total after writing a new blob.
    async fn note_blob_written(&self, session_id: &SessionId, bytes_added: u64) {
        let mut g = self.storage_cache.lock().await;
        let entry = g.entry(session_id.to_string()).or_insert(0);
        *entry = entry.saturating_add(bytes_added);
    }

    /// Capture a single file snapshot. Computes the SHA-256 hash and writes
    /// the blob if it isn't already present. Returns `MissingBeforeChange`
    /// when the file does not exist.
    async fn capture_one_path(
        &self,
        session_id: &SessionId,
        cwd: &Path,
        path: &WorkspaceRelativePath,
    ) -> Result<FileSnapshotKind, CheckpointError> {
        // Resolve and reject anything outside cwd or via symlink.
        let absolute = resolve_within_cwd(cwd, &path.to_path_buf())?;

        match tokio::fs::symlink_metadata(&absolute).await {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(FileSnapshotKind::MissingBeforeChange { path: path.clone() })
            }
            Err(e) => Err(e.into()),
            Ok(meta) if meta.file_type().is_symlink() => {
                Err(CheckpointError::PathEscape(path.to_string()))
            }
            Ok(meta) if meta.is_dir() => {
                // Tracked tools shouldn't hand us directories. Treat as
                // non-snapshottable so the caller surfaces the limitation
                // without crashing the run.
                Err(CheckpointError::Other(format!(
                    "refusing to snapshot directory: {}",
                    path
                )))
            }
            Ok(meta) => {
                let size = meta.len();
                if size > self.limits.max_checkpoint_bytes_per_file {
                    return Err(CheckpointError::FileTooLarge {
                        path: path.to_string(),
                        size,
                        limit: self.limits.max_checkpoint_bytes_per_file,
                    });
                }

                // Read the file, compute SHA-256, write blob if needed.
                let bytes = tokio::fs::read(&absolute).await?;
                let hash = sha256_hex(&bytes);
                let blob_path = self.blob_path(session_id, &hash)?;
                if !blob_path.exists() {
                    // Enforce session storage cap before writing.
                    let used = self.session_storage_bytes(session_id).await?;
                    let total_after = used.saturating_add(bytes.len() as u64);
                    if total_after > self.limits.max_total_checkpoint_bytes_per_session {
                        return Err(CheckpointError::SessionStorageLimit {
                            used: total_after,
                            limit: self.limits.max_total_checkpoint_bytes_per_session,
                        });
                    }
                    if let Some(parent) = blob_path.parent() {
                        tokio::fs::create_dir_all(parent).await?;
                    }
                    atomic_write(&blob_path, &bytes).await?;
                    self.note_blob_written(session_id, bytes.len() as u64).await;
                }

                let modified_time = meta
                    .modified()
                    .ok()
                    .and_then(|st| OffsetDateTime::from(st).into());
                let executable = is_executable(&meta);

                Ok(FileSnapshotKind::ExistingFile {
                    path: path.clone(),
                    content_hash: hash,
                    size_bytes: size,
                    modified_time,
                    executable,
                })
            }
        }
    }

    /// Read blob bytes for a hash; verify hash before returning.
    pub(crate) async fn read_blob(
        &self,
        session_id: &SessionId,
        hash: &str,
    ) -> Result<Vec<u8>, CheckpointError> {
        let path = self.blob_path(session_id, hash)?;
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CheckpointError::BlobMissing(hash.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        let actual = sha256_hex(&bytes);
        if actual != hash {
            return Err(CheckpointError::HashMismatch(hash.to_string()));
        }
        Ok(bytes)
    }

    /// Delete a checkpoint's manifest and its index entry. Blobs are kept
    /// (they are content-addressed and may be referenced by other
    /// checkpoints). Caller MUST hold the per-session index lock.
    async fn delete_checkpoint_locked(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<(), CheckpointError> {
        let manifest = self.manifest_path(session_id, checkpoint_id)?;
        if manifest.exists() {
            // Surface unlink failures (e.g. permission denied on the
            // manifests dir, manifest file held open elsewhere): a silent
            // failure here would leave the index pointing at a manifest we
            // didn't actually remove, and the trim loop would keep
            // marching past it forever.
            tokio::fs::remove_file(&manifest).await?;
        }
        // Rewrite the index without the deleted entry. A malformed line
        // must propagate up: silently treating it as an empty Vec would
        // overwrite the file with empty contents and destroy retention
        // history. Recovery is opt-in via a dedicated path.
        let entries = self.load_index(session_id).await?;
        let kept: Vec<IndexEntry> = entries
            .into_iter()
            .filter(|e| e.checkpoint_id != *checkpoint_id)
            .collect();
        let path = self.index_path_for(session_id)?;
        if path.exists() {
            let mut buf = String::new();
            for e in &kept {
                buf.push_str(&serde_json::to_string(e)?);
                buf.push('\n');
            }
            atomic_write(&path, buf.as_bytes()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl CheckpointStore for LocalCheckpointStore {
    async fn create_checkpoint(
        &self,
        request: CreateCheckpointRequest,
    ) -> Result<CheckpointId, CheckpointError> {
        self.ensure_dirs(&request.session_id).await?;

        // Hold the per-session index lock for the entire trim+append
        // window so a concurrent `refresh_index_count` cannot clobber
        // the newly appended entry.
        let lock = self.index_lock_for(&request.session_id).await;
        let _g = lock.lock().await;

        // Enforce per-session checkpoint count by trimming the oldest
        // entries past the cap. Trimming happens *before* we write the
        // new checkpoint so we don't immediately exceed the cap. A
        // malformed index line propagates as an error rather than being
        // silently coerced to an empty Vec (which would clobber the on-
        // disk index on the next write).
        let mut entries = self.load_index(&request.session_id).await?;
        while entries.len() >= self.limits.max_checkpoints_per_session {
            let oldest = entries.remove(0);
            if let Err(err) = self
                .delete_checkpoint_locked(&request.session_id, &oldest.checkpoint_id)
                .await
            {
                // Don't silently drop the failure: a `.ok()` would lie to
                // ourselves about the trim having succeeded. Keep the
                // entry in the index so a later run can retry, and bail
                // out of the trim loop — if one delete failed the next
                // one likely will too (e.g. read-only manifests dir).
                tracing::warn!(
                    session_id = %request.session_id,
                    checkpoint_id = %oldest.checkpoint_id,
                    error = %err,
                    "checkpoint retention trim failed; keeping entry in index",
                );
                entries.insert(0, oldest);
                break;
            }
        }

        // Age-based retention: drop entries older than max_age_days. The
        // count-based trim above only protects against unbounded growth;
        // long-idle sessions could otherwise hoard expired checkpoints
        // forever. We honor a stable `now` so the cutoff is consistent
        // across this method.
        if let Some(days) = self.limits.max_age_days {
            let cutoff = OffsetDateTime::now_utc() - time::Duration::days(days as i64);
            let aged: Vec<CheckpointId> = entries
                .iter()
                .filter(|e| e.created_at < cutoff)
                .map(|e| e.checkpoint_id.clone())
                .collect();
            for cid in aged {
                if let Err(err) = self
                    .delete_checkpoint_locked(&request.session_id, &cid)
                    .await
                {
                    tracing::warn!(
                        session_id = %request.session_id,
                        checkpoint_id = %cid,
                        error = %err,
                        "checkpoint age-based trim failed; keeping entry in index",
                    );
                    break;
                }
                entries.retain(|e| e.checkpoint_id != cid);
            }
        }

        let checkpoint_id = CheckpointId::new();
        let now = OffsetDateTime::now_utc();
        let meta = CheckpointMetadata {
            checkpoint_id: checkpoint_id.clone(),
            session_id: request.session_id.clone(),
            run_id: request.run_id.clone(),
            user_message_id: request.user_message_id,
            cwd: canonicalize_cwd(&request.cwd),
            created_at: now,
            description: request.description.clone(),
            files_tracked: Vec::new(),
            git_state: request.git_state,
        };
        self.write_manifest(&request.session_id, &meta).await?;

        let entry = IndexEntry {
            checkpoint_id: checkpoint_id.clone(),
            session_id: request.session_id.clone(),
            run_id: request.run_id.clone(),
            created_at: now,
            description: request.description,
            files_tracked_count: 0,
        };
        self.append_index(&request.session_id, &entry).await?;

        Ok(checkpoint_id)
    }

    async fn capture_before_tool(
        &self,
        request: CaptureBeforeToolRequest,
    ) -> Result<Vec<FileSnapshotKind>, CheckpointError> {
        self.ensure_dirs(&request.session_id).await?;

        // Serialize the read-modify-write of this checkpoint's manifest
        // against any other concurrent capture targeting the same
        // checkpoint. Without this, two callers can both read the
        // manifest, each extend their own copy, and the second writer
        // overwrites the first writer's `files_tracked` entries. Blob
        // writes inside `capture_one_path` are content-addressed and
        // safe to interleave, but the duplicate-path check below reads
        // `meta.files_tracked` and so must also stay inside the lock.
        let lock = self
            .manifest_lock_for(&request.session_id, &request.checkpoint_id)
            .await;
        let _g = lock.lock().await;

        let mut meta = self
            .read_manifest(&request.session_id, &request.checkpoint_id)
            .await?;

        // Verify the checkpoint cwd matches the request cwd. Diverging cwd
        // implies the agent is running against a different workspace; a
        // capture there would silently snapshot the wrong tree.
        if !cwd_matches(&meta.cwd, &request.cwd) {
            return Err(CheckpointError::CwdMismatch {
                checkpoint_cwd: meta.cwd.clone(),
                current_cwd: request.cwd.clone(),
            });
        }

        let mut new_snapshots = Vec::with_capacity(request.paths.len());
        for path in &request.paths {
            // Don't double-snapshot the same path. The first capture wins
            // because subsequent captures would see an already-mutated
            // state and break the rewind invariant.
            if meta.files_tracked.iter().any(|s| s.path() == path) {
                continue;
            }
            let snap = self
                .capture_one_path(&request.session_id, &request.cwd, path)
                .await?;
            new_snapshots.push(snap);
        }
        meta.files_tracked.extend(new_snapshots.clone());
        self.write_manifest(&request.session_id, &meta).await?;
        let _ = request.tool_name;
        let _ = request.tool_input;
        let _ = request.run_id;
        Ok(new_snapshots)
    }

    async fn rewind(&self, request: RewindRequest) -> Result<RewindReport, CheckpointError> {
        // Locate which session this checkpoint belongs to by scanning the
        // checkpoints directory. Manifests live under
        // `.agent/checkpoints/<sid>/manifests/<cid>.json`.
        let session_id = self
            .find_session_for_checkpoint(&request.checkpoint_id)
            .await?;
        let meta = self
            .read_manifest(&session_id, &request.checkpoint_id)
            .await?;

        if !request.force_cwd && !cwd_matches(&meta.cwd, &request.cwd) {
            return Err(CheckpointError::CwdMismatch {
                checkpoint_cwd: meta.cwd.clone(),
                current_cwd: request.cwd.clone(),
            });
        }

        let mut report = RewindReport {
            checkpoint_id: request.checkpoint_id.clone(),
            restored_files: Vec::new(),
            deleted_files: Vec::new(),
            recreated_files: Vec::new(),
            skipped_files: Vec::new(),
            dry_run: request.dry_run,
        };

        for snap in &meta.files_tracked {
            let path = snap.path();
            let absolute = match resolve_within_cwd(&request.cwd, &path.to_path_buf()) {
                Ok(p) => p,
                Err(e) => {
                    report.skipped_files.push(SkippedRewind {
                        path: path.clone(),
                        reason: format!("{e}"),
                    });
                    continue;
                }
            };

            // Sanity check: never traverse a symlink during rewind.
            if let Ok(meta_now) = tokio::fs::symlink_metadata(&absolute).await {
                if meta_now.file_type().is_symlink() {
                    report.skipped_files.push(SkippedRewind {
                        path: path.clone(),
                        reason: "current path is a symlink".into(),
                    });
                    continue;
                }
            }

            match snap {
                FileSnapshotKind::ExistingFile {
                    content_hash,
                    executable,
                    ..
                } => {
                    let blob = match self.read_blob(&session_id, content_hash).await {
                        Ok(b) => b,
                        Err(e) => {
                            report.skipped_files.push(SkippedRewind {
                                path: path.clone(),
                                reason: format!("{e}"),
                            });
                            continue;
                        }
                    };
                    let exists_now = absolute.exists();
                    if request.dry_run {
                        if exists_now {
                            report.restored_files.push(path.clone());
                        } else {
                            report.recreated_files.push(path.clone());
                        }
                        continue;
                    }
                    if let Some(parent) = absolute.parent() {
                        if !parent.exists() {
                            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                                report.skipped_files.push(SkippedRewind {
                                    path: path.clone(),
                                    reason: format!("create parent: {e}"),
                                });
                                continue;
                            }
                        }
                    }
                    if let Err(e) = atomic_write(&absolute, &blob).await {
                        report.skipped_files.push(SkippedRewind {
                            path: path.clone(),
                            reason: format!("{e}"),
                        });
                        continue;
                    }
                    if *executable {
                        let _ = set_executable(&absolute).await;
                    }
                    if exists_now {
                        report.restored_files.push(path.clone());
                    } else {
                        report.recreated_files.push(path.clone());
                    }
                }
                FileSnapshotKind::MissingBeforeChange { .. } => {
                    let exists_now = absolute.exists();
                    if !exists_now {
                        // No-op; nothing to delete.
                        continue;
                    }
                    if request.dry_run {
                        report.deleted_files.push(path.clone());
                        continue;
                    }
                    if let Err(e) = tokio::fs::remove_file(&absolute).await {
                        report.skipped_files.push(SkippedRewind {
                            path: path.clone(),
                            reason: format!("{e}"),
                        });
                        continue;
                    }
                    report.deleted_files.push(path.clone());
                }
            }
        }

        Ok(report)
    }

    async fn list_checkpoints(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<CheckpointSummary>, CheckpointError> {
        let entries = self.load_index(session_id).await?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            out.push(CheckpointSummary {
                checkpoint_id: e.checkpoint_id,
                session_id: e.session_id,
                run_id: e.run_id,
                created_at: e.created_at,
                description: e.description,
                files_tracked_count: e.files_tracked_count,
            });
        }
        Ok(out)
    }

    async fn get_checkpoint(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<CheckpointMetadata, CheckpointError> {
        self.read_manifest(session_id, checkpoint_id).await
    }

    async fn diff_checkpoint(
        &self,
        request: DiffCheckpointRequest,
    ) -> Result<CheckpointDiff, CheckpointError> {
        crate::diff::diff_checkpoint(self, request).await
    }
}

impl LocalCheckpointStore {
    /// Walk the checkpoints directory looking for the session that owns
    /// `checkpoint_id`. The caller can avoid this by passing the session id
    /// directly to [`Self::get_checkpoint`].
    pub async fn find_session_for_checkpoint(
        &self,
        checkpoint_id: &CheckpointId,
    ) -> Result<SessionId, CheckpointError> {
        validate_checkpoint_id(checkpoint_id)?;
        let dir = self.root.join(".agent").join("checkpoints");
        if !dir.exists() {
            return Err(CheckpointError::NotFound(checkpoint_id.to_string()));
        }
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let p = ent.path();
            if !p.is_dir() {
                continue;
            }
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !is_valid_session_id_str(&name) {
                continue;
            }
            let sid = SessionId::from(name);
            let manifest = self
                .manifest_path(&sid, checkpoint_id)
                .ok()
                .filter(|p| p.exists());
            if manifest.is_some() {
                return Ok(sid);
            }
        }
        Err(CheckpointError::NotFound(checkpoint_id.to_string()))
    }

    /// Persist updated tracking count in the index file. Used after
    /// snapshot capture so `list_checkpoints` reports a fresh count.
    /// Holds the per-session index lock so a concurrent
    /// `create_checkpoint` doesn't lose its append.
    pub async fn refresh_index_count(
        &self,
        session_id: &SessionId,
        checkpoint_id: &CheckpointId,
    ) -> Result<(), CheckpointError> {
        let lock = self.index_lock_for(session_id).await;
        let _g = lock.lock().await;
        let meta = self.read_manifest(session_id, checkpoint_id).await?;
        let mut entries = self.load_index(session_id).await?;
        let mut changed = false;
        for e in &mut entries {
            if e.checkpoint_id == *checkpoint_id {
                e.files_tracked_count = meta.files_tracked.len();
                changed = true;
                break;
            }
        }
        if changed {
            let path = self.index_path_for(session_id)?;
            let mut buf = String::new();
            for e in &entries {
                buf.push_str(&serde_json::to_string(e)?);
                buf.push('\n');
            }
            atomic_write(&path, buf.as_bytes()).await?;
        }
        Ok(())
    }
}

fn validate_session_id(session_id: &SessionId) -> Result<(), CheckpointError> {
    if session_id.is_valid_format() {
        Ok(())
    } else {
        Err(CheckpointError::InvalidSessionId)
    }
}

fn validate_checkpoint_id(checkpoint_id: &CheckpointId) -> Result<(), CheckpointError> {
    if checkpoint_id.is_valid_format() {
        Ok(())
    } else {
        Err(CheckpointError::InvalidCheckpointId)
    }
}

fn validate_blob_hash(hash: &str) -> Result<(), CheckpointError> {
    if hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(CheckpointError::Other("invalid blob hash".into()))
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Resolve `requested` (which is workspace-relative) inside `cwd`, rejecting
/// absolute paths, `..` escapes, and any path whose **parent** components
/// traverse a symlink that resolves outside `cwd`.
///
/// Lexical-only sandboxing is unsafe: a workspace path like `dir/link/file`
/// where `link -> /etc` would lexically pass `starts_with(canon_root)` and
/// then escape on the next filesystem op. We canonicalize each existing
/// ancestor and check that the canonical path stays inside the canonical
/// root.
pub(crate) fn resolve_within_cwd(cwd: &Path, requested: &Path) -> Result<PathBuf, CheckpointError> {
    let canon_root = cwd
        .canonicalize()
        .map_err(|e| CheckpointError::Other(format!("cwd canonicalize failed: {e}")))?;
    let mut joined = canon_root.clone();
    for c in requested.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                return Err(CheckpointError::PathEscape(requested.display().to_string()));
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !joined.pop() {
                    return Err(CheckpointError::PathEscape(requested.display().to_string()));
                }
                if !joined.starts_with(&canon_root) {
                    return Err(CheckpointError::PathEscape(requested.display().to_string()));
                }
            }
            Component::Normal(seg) => {
                joined.push(seg);
            }
        }
    }
    if !joined.starts_with(&canon_root) {
        return Err(CheckpointError::PathEscape(requested.display().to_string()));
    }

    // Symlink check on the parent. We canonicalize each ancestor that
    // actually exists; if it canonicalizes outside the workspace root we
    // reject. The terminal component is allowed to be a symlink only if
    // its target stays inside the workspace — this is enforced at
    // capture/restore time via `symlink_metadata`, which never follows.
    if let Some(parent) = joined.parent() {
        match parent.canonicalize() {
            Ok(canon_parent) => {
                if !canon_parent.starts_with(&canon_root) {
                    return Err(CheckpointError::PathEscape(requested.display().to_string()));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Parent doesn't exist yet (e.g. capturing into a path
                // whose parent dir hasn't been created). Walk up to the
                // closest existing ancestor and check that.
                let mut probe: PathBuf = parent.to_path_buf();
                while !probe.exists() {
                    if !probe.pop() {
                        break;
                    }
                }
                if let Ok(canon_existing) = probe.canonicalize() {
                    if !canon_existing.starts_with(&canon_root) {
                        return Err(CheckpointError::PathEscape(requested.display().to_string()));
                    }
                }
            }
            Err(e) => {
                return Err(CheckpointError::Other(format!(
                    "canonicalize parent failed: {e}"
                )));
            }
        }
    }

    Ok(joined)
}

fn canonicalize_cwd(cwd: &Path) -> PathBuf {
    cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf())
}

fn cwd_matches(checkpoint_cwd: &Path, current_cwd: &Path) -> bool {
    let a = checkpoint_cwd
        .canonicalize()
        .unwrap_or_else(|_| checkpoint_cwd.to_path_buf());
    let b = current_cwd
        .canonicalize()
        .unwrap_or_else(|_| current_cwd.to_path_buf());
    a == b
}

/// Atomic write: write to a unique sibling temp file, fsync, rename into
/// place. The temp filename appends a `.ckpt-tmp.<rand>` suffix to the
/// full filename (NOT `with_extension`, which would replace the existing
/// `.rs`/`.txt` and collide for sibling files like `main.rs` + `main.txt`).
pub(crate) async fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CheckpointError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let original_name = path
        .file_name()
        .ok_or_else(|| CheckpointError::Other("atomic_write: path has no file name".into()))?
        .to_string_lossy()
        .into_owned();
    // Random suffix from the same randomness source the checkpoint id
    // uses; cheap, unique-enough that two writers in the same dir won't
    // collide. We don't need cryptographic uniqueness here.
    let suffix = uuid::Uuid::new_v4().simple();
    let tmp_name = format!("{original_name}.ckpt-tmp.{suffix}");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.flush().await?;
        // sync_all is best-effort; some filesystems don't support it.
        let _ = f.sync_all().await;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e.into());
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
async fn set_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    let mode = perms.mode();
    perms.set_mode(mode | 0o111);
    tokio::fs::set_permissions(path, perms).await
}

#[cfg(not(unix))]
async fn set_executable(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::SessionId;
    use tempfile::TempDir;

    fn fresh_store() -> (TempDir, LocalCheckpointStore, SessionId) {
        let dir = TempDir::new().unwrap();
        let store = LocalCheckpointStore::new(dir.path().to_path_buf());
        let sid = SessionId::new();
        (dir, store, sid)
    }

    #[tokio::test]
    // Creating a checkpoint persists a manifest and an index entry.
    async fn create_persists_manifest_and_index() {
        let (_dir, store, sid) = fresh_store();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: Some("t".into()),
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        let list = store.list_checkpoints(&sid).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].checkpoint_id, id);
        let meta = store.get_checkpoint(&sid, &id).await.unwrap();
        assert_eq!(meta.description.as_deref(), Some("t"));
    }

    #[tokio::test]
    // Path traversal in capture is rejected.
    async fn capture_rejects_traversal() {
        let (_dir, store, sid) = fresh_store();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        let err = store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: id,
                session_id: sid,
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                tool_name: "Edit".into(),
                tool_input: serde_json::json!({}),
                paths: vec![WorkspaceRelativePath("../etc/passwd".into())],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CheckpointError::PathEscape(_)), "got {err:?}");
    }

    #[tokio::test]
    // Per-file size limit is enforced.
    async fn enforces_per_file_size_limit() {
        let dir = TempDir::new().unwrap();
        let limits = CheckpointLimits {
            max_checkpoint_bytes_per_file: 8,
            ..Default::default()
        };
        let store = LocalCheckpointStore::with_limits(dir.path().to_path_buf(), limits);
        let sid = SessionId::new();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        std::fs::write(dir.path().join("big.txt"), b"way too many bytes").unwrap();
        let err = store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: id,
                session_id: sid,
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                tool_name: "Write".into(),
                tool_input: serde_json::json!({}),
                paths: vec![WorkspaceRelativePath("big.txt".into())],
            })
            .await
            .unwrap_err();
        assert!(matches!(err, CheckpointError::FileTooLarge { .. }));
    }

    #[tokio::test]
    // Identical content is deduplicated to a single blob.
    async fn dedupes_identical_blobs() {
        let dir = TempDir::new().unwrap();
        let store = LocalCheckpointStore::new(dir.path().to_path_buf());
        let sid = SessionId::new();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"hello").unwrap();
        store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: id.clone(),
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                tool_name: "Edit".into(),
                tool_input: serde_json::json!({}),
                paths: vec![
                    WorkspaceRelativePath("a.txt".into()),
                    WorkspaceRelativePath("b.txt".into()),
                ],
            })
            .await
            .unwrap();
        let blobs_dir = store.blobs_dir_for(&sid).unwrap();
        let count = std::fs::read_dir(&blobs_dir).unwrap().count();
        assert_eq!(count, 1, "identical content should produce one blob");
    }

    #[tokio::test]
    // A malformed line in the index must NOT be silently overwritten with an
    // empty vec; callers that touch the index must propagate the parse error
    // and leave the on-disk index untouched.
    async fn malformed_index_is_not_silently_overwritten() {
        let (_dir, store, sid) = fresh_store();
        store.ensure_dirs(&sid).await.unwrap();
        let valid = IndexEntry {
            checkpoint_id: CheckpointId::new(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            created_at: OffsetDateTime::now_utc(),
            description: None,
            files_tracked_count: 0,
        };
        let mut contents = serde_json::to_string(&valid).unwrap();
        contents.push('\n');
        contents.push_str("{ not json }\n");
        let path = store.index_path_for(&sid).unwrap();
        tokio::fs::write(&path, &contents).await.unwrap();
        let before = tokio::fs::read_to_string(&path).await.unwrap();
        // Use delete_checkpoint_locked (the entry point that previously did
        // unwrap_or_default() and clobbered the index).
        let err = store
            .delete_checkpoint_locked(&sid, &valid.checkpoint_id)
            .await
            .unwrap_err();
        assert!(
            matches!(err, CheckpointError::MalformedManifest(_)),
            "expected MalformedManifest, got {err:?}",
        );
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(
            before, after,
            "malformed index file must not be overwritten on load failure"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    // delete_checkpoint_locked must surface a manifest unlink error rather
    // than silently discarding it; the index entry is left in place so a
    // later run can retry.
    async fn delete_surfaces_manifest_remove_error() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, store, sid) = fresh_store();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        let manifests = store.manifests_dir_for(&sid).unwrap();
        let original = std::fs::metadata(&manifests).unwrap().permissions();
        // Make the manifests dir read+exec only: unlink inside is denied.
        std::fs::set_permissions(&manifests, std::fs::Permissions::from_mode(0o555)).unwrap();

        let result = store.delete_checkpoint_locked(&sid, &id).await;

        // Always restore perms so the tempdir can be cleaned up.
        std::fs::set_permissions(&manifests, original).unwrap();

        let err = result.expect_err("delete should surface unlink failure");
        assert!(
            matches!(err, CheckpointError::Io(_)),
            "expected Io error, got {err:?}",
        );
        // Index entry must still be present so a later retry can pick it up.
        let list = store.list_checkpoints(&sid).await.unwrap();
        assert!(
            list.iter().any(|e| e.checkpoint_id == id),
            "index entry must be kept after a failed unlink",
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    // Two parallel capture_before_tool calls against the same checkpoint
    // with different paths must both end up in files_tracked on disk; the
    // read-modify-write of the manifest is serialized per checkpoint.
    async fn concurrent_captures_preserve_both_entries() {
        let dir = TempDir::new().unwrap();
        let store = std::sync::Arc::new(LocalCheckpointStore::new(dir.path().to_path_buf()));
        let sid = SessionId::new();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();

        // Pre-create both files so the captures emit real ExistingFile
        // snapshots rather than MissingBeforeChange placeholders.
        std::fs::write(dir.path().join("a.txt"), b"AAAA").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"BBBB").unwrap();

        let mk_req = |path: &str| CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: store.root().to_path_buf(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({}),
            paths: vec![WorkspaceRelativePath(path.into())],
        };

        let s1 = store.clone();
        let s2 = store.clone();
        let r1 = mk_req("a.txt");
        let r2 = mk_req("b.txt");
        let (res1, res2) = tokio::join!(
            tokio::spawn(async move { s1.capture_before_tool(r1).await }),
            tokio::spawn(async move { s2.capture_before_tool(r2).await }),
        );
        res1.unwrap().unwrap();
        res2.unwrap().unwrap();

        let meta = store.get_checkpoint(&sid, &id).await.unwrap();
        let paths: std::collections::HashSet<String> = meta
            .files_tracked
            .iter()
            .map(|s| s.path().as_str().to_string())
            .collect();
        assert!(paths.contains("a.txt"), "missing a.txt: {paths:?}");
        assert!(paths.contains("b.txt"), "missing b.txt: {paths:?}");
    }

    #[test]
    // CheckpointLimits validator rejects zero values that would crash the
    // retention trim or instantly expire every checkpoint.
    fn limits_validate_rejects_zero_count_and_zero_age() {
        let bad = CheckpointLimits {
            max_checkpoints_per_session: 0,
            ..Default::default()
        };
        let err = bad.validate().unwrap_err();
        assert!(
            matches!(err, CheckpointError::InvalidConfig(_)),
            "got {err:?}",
        );

        let bad_age = CheckpointLimits {
            max_age_days: Some(0),
            ..Default::default()
        };
        let err = bad_age.validate().unwrap_err();
        assert!(
            matches!(err, CheckpointError::InvalidConfig(_)),
            "got {err:?}",
        );

        CheckpointLimits::default()
            .validate()
            .expect("defaults must validate");
    }

    #[tokio::test]
    // An index entry older than max_age_days is pruned when the next
    // checkpoint is created.
    async fn create_checkpoint_prunes_entries_past_max_age() {
        let dir = TempDir::new().unwrap();
        let limits = CheckpointLimits {
            max_age_days: Some(7),
            ..Default::default()
        };
        let store = LocalCheckpointStore::with_limits(dir.path().to_path_buf(), limits);
        let sid = SessionId::new();
        store.ensure_dirs(&sid).await.unwrap();

        // Hand-write an index entry whose `created_at` is 30 days in the
        // past plus a matching manifest. This avoids depending on system
        // time / a real clock injection.
        let old_id = CheckpointId::new();
        let old_run = RunId::new();
        let old_created = OffsetDateTime::now_utc() - time::Duration::days(30);
        let old_entry = IndexEntry {
            checkpoint_id: old_id.clone(),
            session_id: sid.clone(),
            run_id: old_run.clone(),
            created_at: old_created,
            description: None,
            files_tracked_count: 0,
        };
        let old_meta = CheckpointMetadata {
            checkpoint_id: old_id.clone(),
            session_id: sid.clone(),
            run_id: old_run,
            user_message_id: None,
            cwd: dir.path().to_path_buf(),
            created_at: old_created,
            description: None,
            files_tracked: Vec::new(),
            git_state: None,
        };
        store.write_manifest(&sid, &old_meta).await.unwrap();
        store.append_index(&sid, &old_entry).await.unwrap();

        let list = store.list_checkpoints(&sid).await.unwrap();
        assert_eq!(list.len(), 1, "precondition: aged entry present");

        // Creating a new checkpoint should prune the 30-day-old entry.
        let new_id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: dir.path().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();

        let list = store.list_checkpoints(&sid).await.unwrap();
        let ids: Vec<_> = list.iter().map(|c| c.checkpoint_id.clone()).collect();
        assert!(
            !ids.contains(&old_id),
            "aged entry should be pruned, got {ids:?}"
        );
        assert!(ids.contains(&new_id), "new entry should be present");
    }

    #[tokio::test]
    // Missing blob produces a structured error.
    async fn missing_blob_returns_structured_error() {
        let (dir, store, sid) = fresh_store();
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        std::fs::write(dir.path().join("x.txt"), b"abc").unwrap();
        store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: id.clone(),
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: store.root().to_path_buf(),
                tool_name: "Edit".into(),
                tool_input: serde_json::json!({}),
                paths: vec![WorkspaceRelativePath("x.txt".into())],
            })
            .await
            .unwrap();
        // Wipe the blobs directory to simulate corruption.
        let blobs = store.blobs_dir_for(&sid).unwrap();
        for ent in std::fs::read_dir(&blobs).unwrap() {
            std::fs::remove_file(ent.unwrap().path()).unwrap();
        }
        let err = store
            .read_blob(&sid, "0".repeat(64).as_str())
            .await
            .unwrap_err();
        assert!(matches!(err, CheckpointError::BlobMissing(_)));
    }
}
