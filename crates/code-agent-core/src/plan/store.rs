//! Persistent plan store under `.agent/plans/`.
//!
//! On-disk layout:
//!
//! ```text
//! .agent/
//!   plans/
//!     <plan-id>.json                 # latest version, atomically written
//!     <plan-id>.v<n>.json            # per-version snapshot for diffs/history
//!     <plan-id>.v<n>.approved.json   # write-once snapshot captured at approval time
//!     <plan-id>.lock                 # advisory cross-process write lock
//!     index.jsonl                    # one line per write (never rewritten)
//! ```
//!
//! Writes that read-modify-write a plan (`create`, `save_revision`,
//! `approve`, `reject`, `archive`) take an exclusive advisory lock
//! (`flock`, via `fs4`) on the sibling `<plan-id>.lock` file for the full
//! read-decide-write block, so two separate processes cannot interleave
//! and clobber each other's writes. Within a single process a per-plan
//! async mutex provides the same serialization without blocking the
//! runtime on the `flock` syscall for sibling tasks.
//!
//! All writes go through `tempfile::NamedTempFile::persist` so a partial
//! write never replaces a valid file. A corrupted index never makes other
//! plans unreadable: listings re-walk the directory and skip files they
//! can't parse.
//!
//! Versioned snapshots `<plan-id>.v<n>.json` are overwritten with the
//! latest state for that version (status transitions through Executing /
//! Completed / Failed still update the same file). The `.approved.json`
//! snapshot is taken once when a plan first transitions into the
//! `Approved` state and is then write-once — it preserves exactly what
//! the reviewer approved, even after the driver mutates the same version
//! during execution. Diff / compare paths prefer `.approved.json` over
//! `.v<n>.json` for an approved version.
//!
//! Retention: `max_plan_revisions` (from `PlanningOptions`) is a soft cap
//! on `.v<n>.json` snapshots. After each successful revision write the
//! oldest versioned snapshots are GC'd until at most that many remain.
//! `.approved.json` snapshots are skipped by the GC sweep. The plan's
//! `version` counter itself is unbounded; only on-disk snapshot
//! retention is capped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fs4::FileExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::sync::Mutex as AsyncMutex;

use crate::options::SessionId;

use super::artifact::{
    PlanApproval, PlanApprovalScope, PlanApprover, PlanDocument, PlanId, PlanStatus, PlanSummary,
};
use super::deviation::redact_secrets_in_plan;
use super::errors::PlanError;
use super::options::PlanningOptions;

/// RAII guard for an exclusive cross-process advisory lock on a plan's
/// `<id>.lock` file. The lock is released when the guard is dropped:
/// `unlock` is best-effort and, even if it fails, closing the file
/// descriptor on drop releases the underlying `flock`.
struct PlanFileLock {
    file: Option<std::fs::File>,
}

impl Drop for PlanFileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlanListFilter {
    pub status: Option<PlanStatus>,
    pub session_id: Option<SessionId>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct PlanApprovalRequest {
    pub plan_id: PlanId,
    pub approved_by: PlanApprover,
    pub approved_version: u32,
    pub scope: PlanApprovalScope,
    pub notes: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlanRejectRequest {
    pub plan_id: PlanId,
    pub reason: Option<String>,
}

/// Persistent store for plan documents.
#[async_trait]
pub trait PlanStore: Send + Sync {
    async fn create(&self, plan: PlanDocument) -> Result<PlanId, PlanError>;
    async fn load(&self, plan_id: &PlanId) -> Result<PlanDocument, PlanError>;
    async fn save_revision(&self, plan: PlanDocument) -> Result<(), PlanError>;
    async fn list(&self, filter: PlanListFilter) -> Result<Vec<PlanSummary>, PlanError>;
    async fn approve(&self, request: PlanApprovalRequest) -> Result<PlanDocument, PlanError>;
    async fn reject(&self, request: PlanRejectRequest) -> Result<PlanDocument, PlanError>;
    async fn archive(&self, plan_id: &PlanId) -> Result<(), PlanError>;
    async fn latest_for_session(&self, session_id: &SessionId)
        -> Result<Option<PlanId>, PlanError>;
    /// True when the store is enabled and writable. Never panics.
    async fn writable(&self) -> bool;
}

/// On-disk implementation rooted at `<cwd>/.agent/plans/`.
///
/// Concurrency is serialized at two levels for the read-decide-write
/// block in `save_revision` / `save_revision_atomic`:
///
/// * In-process: a per-plan async mutex (`plan_locks`) so two concurrent
///   revisions of the same plan id within one process cannot both observe
///   the same prior state and race the approval-invalidation check.
/// * Cross-process: an exclusive advisory lock (`fs4` / `flock`) on a
///   sibling `<plan-id>.lock` file, held across the same block, so two
///   separate processes (e.g. the CLI and the TUI) cannot interleave
///   their read-modify-write cycles and clobber each other. The lock is
///   acquired after the in-process mutex and released on drop. Distinct
///   plan ids do not contend with each other at either level.
#[derive(Debug, Clone)]
pub struct FsPlanStore {
    root: PathBuf,
    options: PlanningOptions,
    plan_locks: Arc<AsyncMutex<HashMap<String, Arc<AsyncMutex<()>>>>>,
}

impl FsPlanStore {
    pub fn new(cwd: impl Into<PathBuf>, options: PlanningOptions) -> Self {
        let cwd = cwd.into();
        let root = cwd.join(".agent").join("plans");
        Self {
            root,
            options,
            plan_locks: Arc::new(AsyncMutex::new(HashMap::new())),
        }
    }

    /// Returns (and lazily creates) the per-plan async mutex that
    /// serializes `save_revision` for a single plan id. Multiple plan ids
    /// proceed in parallel. The map mutex is held only long enough to
    /// look up / insert the entry.
    ///
    /// This only guards callers that share this process. Cross-process
    /// serialization is handled separately by [`Self::lock_plan_file`].
    async fn plan_lock(&self, id: &PlanId) -> Arc<AsyncMutex<()>> {
        let mut map = self.plan_locks.lock().await;
        map.entry(id.0.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    /// Path of the sibling advisory-lock file for `id`. Separate from the
    /// plan's data files so the `flock` never sits on a file that gets
    /// renamed out from under it by the atomic temp-file writes.
    fn lock_path(&self, id: &PlanId) -> Result<PathBuf, PlanError> {
        Self::validate_id(id)?;
        Ok(self.root.join(format!("{}.lock", id.0)))
    }

    /// Acquire an exclusive cross-process advisory lock on `<id>.lock`,
    /// held until the returned guard is dropped. Serializes the
    /// read-decide-write block in `save_revision` against other processes
    /// (e.g. the CLI and the TUI) operating on the same plan id. The lock
    /// is advisory: it only serializes callers that also take it, which is
    /// every write path in this store.
    fn lock_plan_file(&self, id: &PlanId) -> Result<PlanFileLock, PlanError> {
        self.ensure_root()?;
        let path = self.lock_path(id)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .map_err(|e| PlanError::Io {
                path: path.clone(),
                source: e,
            })?;
        // UFCS through `fs4::FileExt` to disambiguate from the inherent
        // `std::fs::File::lock` stabilized in Rust 1.89: `unstable_name_collisions`
        // is denied under `-D warnings`, and the inherent method would otherwise
        // silently shadow fs4's on newer toolchains.
        FileExt::lock(&file).map_err(|e| PlanError::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(PlanFileLock { file: Some(file) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn plans_dir_for(cwd: &Path) -> PathBuf {
        cwd.join(".agent").join("plans")
    }

    fn validate_id(id: &PlanId) -> Result<(), PlanError> {
        if !id.is_valid_format() {
            return Err(PlanError::InvalidId(id.0.clone()));
        }
        Ok(())
    }

    fn current_path(&self, id: &PlanId) -> Result<PathBuf, PlanError> {
        Self::validate_id(id)?;
        Ok(self.root.join(format!("{}.json", id.0)))
    }

    fn version_path(&self, id: &PlanId, version: u32) -> Result<PathBuf, PlanError> {
        Self::validate_id(id)?;
        Ok(self.root.join(format!("{}.v{}.json", id.0, version)))
    }

    /// Write-once snapshot path captured at approval time. Distinct from
    /// `version_path` so post-approval status transitions (Executing /
    /// Completed / Failed) cannot overwrite the approved body — the
    /// approved-vs-ran audit and drift detection depend on it.
    fn approved_version_path(&self, id: &PlanId, version: u32) -> Result<PathBuf, PlanError> {
        Self::validate_id(id)?;
        Ok(self
            .root
            .join(format!("{}.v{}.approved.json", id.0, version)))
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.jsonl")
    }

    fn ensure_root(&self) -> Result<(), PlanError> {
        if !self.root.exists() {
            std::fs::create_dir_all(&self.root).map_err(|e| PlanError::Io {
                path: self.root.clone(),
                source: e,
            })?;
        }
        Ok(())
    }

    fn write_atomic(&self, target: &Path, bytes: &[u8]) -> Result<(), PlanError> {
        self.ensure_root()?;
        let dir = target.parent().unwrap_or(&self.root);
        let mut tf = tempfile::NamedTempFile::new_in(dir).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        use std::io::Write as _;
        tf.write_all(bytes).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        tf.as_file().sync_data().map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        tf.persist(target).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e.error,
        })?;
        Ok(())
    }

    /// Write-once, crash-safe create of an `<id>.v<n>.approved.json`
    /// snapshot. The body is staged to a temp file in the target's parent
    /// directory, `sync_data`'d, then renamed into place via
    /// `persist_noclobber` so a crash mid-write cannot leave a partial
    /// `.approved.json` at the final path. `AlreadyExists` from the
    /// rename is treated as `Ok(())` because the prior capture is the
    /// audit record we want to preserve.
    ///
    /// Note: if a partial `.approved.json` is already present from a
    /// pre-fix crash, this writer treats it as the existing capture and
    /// leaves it alone (same as the previous behavior). Going forward
    /// the temp-and-rename pattern prevents new partials from being
    /// created.
    fn write_approved_snapshot(&self, target: &Path, bytes: &[u8]) -> Result<(), PlanError> {
        self.ensure_root()?;
        let dir = target.parent().unwrap_or(&self.root);
        let mut tf = tempfile::NamedTempFile::new_in(dir).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        use std::io::Write as _;
        tf.write_all(bytes).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        tf.as_file().sync_data().map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        match tf.persist_noclobber(target) {
            Ok(_) => Ok(()),
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(e) => Err(PlanError::Io {
                path: target.to_path_buf(),
                source: e.error,
            }),
        }
    }

    /// Atomic `load → validate → save` for an edited plan, holding the
    /// per-plan lock across all three steps. Used by `plans edit` so a
    /// concurrent writer cannot mutate the on-disk plan between the
    /// validator reading the original and the save persisting the
    /// edited body. The validator is called with the live on-disk plan
    /// (`current`) and the caller's `edited` plan; returning an error
    /// aborts the save and surfaces the validator's error to the caller.
    pub async fn save_revision_atomic<F>(
        &self,
        edited: PlanDocument,
        validator: F,
    ) -> Result<(), PlanError>
    where
        F: FnOnce(&PlanDocument, &PlanDocument) -> Result<(), PlanError>,
    {
        Self::validate_id(&edited.id)?;
        let lock = self.plan_lock(&edited.id).await;
        let _guard = lock.lock().await;
        // Cross-process advisory lock, held for the whole load + validate
        // + save so a second process cannot interleave its own
        // read-modify-write for this plan id. Released on drop.
        let _file_lock = self.lock_plan_file(&edited.id)?;
        // Load the live on-disk plan inside the lock so a concurrent
        // `save_revision` for this id cannot slip between the load and
        // the validator. `load` itself does not take the per-plan lock,
        // so calling it here while we hold the guard is safe.
        let current_path = self.current_path(&edited.id)?;
        if !current_path.exists() {
            return Err(PlanError::NotFound(edited.id.0.clone()));
        }
        let bytes = std::fs::read(&current_path).map_err(|e| PlanError::Io {
            path: current_path.clone(),
            source: e,
        })?;
        let current: PlanDocument =
            serde_json::from_slice(&bytes).map_err(|e| PlanError::Parse {
                path: current_path,
                source: e,
            })?;
        validator(&current, &edited)?;
        self.save_revision_locked(edited)
    }

    /// Atomic read-modify-write for a single plan id. Loads the live
    /// on-disk plan, applies `transform` to it, and saves the result —
    /// all while holding both the in-process per-plan mutex and the
    /// cross-process `<id>.lock` advisory lock. Because the load happens
    /// under the lock, two concurrent callers (including separate
    /// processes) cannot both observe the same prior state, so no update
    /// is lost. `transform` returning an error aborts the save.
    pub async fn mutate_revision<F>(&self, id: &PlanId, transform: F) -> Result<(), PlanError>
    where
        F: FnOnce(&mut PlanDocument) -> Result<(), PlanError>,
    {
        Self::validate_id(id)?;
        let lock = self.plan_lock(id).await;
        let _guard = lock.lock().await;
        let _file_lock = self.lock_plan_file(id)?;
        // Load inside the lock so the modify happens against the freshest
        // committed state, not a snapshot read before the lock was held.
        let path = self.current_path(id)?;
        if !path.exists() {
            return Err(PlanError::NotFound(id.0.clone()));
        }
        let bytes = std::fs::read(&path).map_err(|e| PlanError::Io {
            path: path.clone(),
            source: e,
        })?;
        let mut plan: PlanDocument =
            serde_json::from_slice(&bytes).map_err(|e| PlanError::Parse {
                path: path.clone(),
                source: e,
            })?;
        transform(&mut plan)?;
        self.save_revision_locked(plan)
    }

    /// Locked body of `save_revision`: caller must hold the per-plan
    /// mutex. Performs the approval-invalidation check, atomic writes,
    /// `.approved.json` capture, GC, and index append.
    fn save_revision_locked(&self, mut plan: PlanDocument) -> Result<(), PlanError> {
        plan.updated_at = OffsetDateTime::now_utc();

        // Any approval is keyed to a specific plan version. The moment the
        // revision's version differs from the approval's recorded version
        // — or any body field of the on-disk current plan differs from the
        // incoming one while we're still calling ourselves Approved — we
        // must flip the plan back to PendingApproval and drop the prior
        // approval. This guards against three distinct mistakes:
        //
        //   1. A caller bumps `plan.version` but forgets to set
        //      `created_by = RevisedBy*`.
        //   2. A caller edits plan body fields in place at the same
        //      version (which would otherwise leave a stale approval
        //      attached to mutated content).
        //   3. The act driver itself transitioning the plan into
        //      `Executing` and back: that is intentional and we *do not*
        //      reset for status-only edits between Approved/Executing/
        //      Completed.
        // Load the prior on-disk state once: needed both for body-drift
        // detection and to decide whether this save is the first transition
        // into Approved (which captures the write-once `.approved.json`).
        let prev_on_disk = self.load_existing_for_compare(&plan.id);
        let prev_status = prev_on_disk.as_ref().map(|p| p.status);

        let approval_invalidated = match (&plan.approval, plan.status) {
            (Some(approval), PlanStatus::Approved) => {
                if approval.approved_version != plan.version {
                    true
                } else {
                    // Same version but content may have drifted. Compare
                    // against the on-disk current plan body.
                    match prev_on_disk.as_ref() {
                        Some(prev) => plan_body_changed(prev, &plan),
                        None => false,
                    }
                }
            }
            _ => false,
        };
        if approval_invalidated {
            plan.status = PlanStatus::PendingApproval;
            plan.approval = None;
        }
        redact_secrets_in_plan(&mut plan);
        let bytes = self.validate_plan_size(&plan)?;
        let cur = self.current_path(&plan.id)?;
        let ver = self.version_path(&plan.id, plan.version)?;
        self.write_atomic(&cur, &bytes)?;
        self.write_atomic(&ver, &bytes)?;

        // If this save is the first transition into Approved, capture a
        // write-once snapshot of the approved body. Subsequent saves at the
        // same version (status -> Executing / Completed / Failed) will
        // overwrite `<id>.v<n>.json` but must leave `.approved.json` alone.
        let is_first_approval_transition = matches!(plan.status, PlanStatus::Approved)
            && !matches!(prev_status, Some(PlanStatus::Approved));
        if is_first_approval_transition {
            let approved = self.approved_version_path(&plan.id, plan.version)?;
            // Use create_new so we never clobber an existing approved
            // snapshot. If it already exists we leave the older capture in
            // place — that's the audit record. The approved snapshot is
            // integrity-critical, so propagate any write failure.
            self.write_approved_snapshot(&approved, &bytes)?;
        }

        // Soft retention cap: trim the oldest `.v<n>.json` snapshots so
        // their count never exceeds `max_plan_revisions`. Approved
        // snapshots are skipped — they are write-once audit records.
        self.gc_old_version_snapshots(&plan.id);

        self.append_index(&IndexEntry {
            timestamp: now_iso(),
            plan_id: plan.id.0.clone(),
            version: plan.version,
            status: plan.status,
            event: IndexEvent::Revised,
            session_id: plan.session_id.as_ref().map(|s| s.as_str().to_string()),
        })?;
        Ok(())
    }

    fn append_index(&self, entry: &IndexEntry) -> Result<(), PlanError> {
        self.ensure_root()?;
        let line = serde_json::to_string(entry).map_err(|e| PlanError::Parse {
            path: self.index_path(),
            source: e,
        })?;
        let mut s = String::with_capacity(line.len() + 1);
        s.push_str(&line);
        s.push('\n');
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.index_path())
            .map_err(|e| PlanError::Io {
                path: self.index_path(),
                source: e,
            })?;
        f.write_all(s.as_bytes()).map_err(|e| PlanError::Io {
            path: self.index_path(),
            source: e,
        })?;
        Ok(())
    }

    fn validate_plan_size(&self, plan: &PlanDocument) -> Result<Vec<u8>, PlanError> {
        if plan.steps.len() > self.options.max_plan_steps {
            return Err(PlanError::TooManySteps {
                got: plan.steps.len(),
                limit: self.options.max_plan_steps,
            });
        }
        // `max_plan_revisions` is enforced as a soft cap via GC in
        // `gc_old_version_snapshots` after a successful write, not as a
        // hard error. The version counter is allowed to grow without
        // bound — only on-disk snapshot retention is capped.
        let bytes = serde_json::to_vec_pretty(plan).map_err(|e| PlanError::Parse {
            path: self.root.clone(),
            source: e,
        })?;
        if bytes.len() > self.options.max_plan_bytes {
            return Err(PlanError::TooLarge {
                bytes: bytes.len(),
                limit: self.options.max_plan_bytes,
            });
        }
        Ok(bytes)
    }

    /// Soft-cap retention for `<id>.v<n>.json` snapshots. Lists the
    /// versioned snapshots for `id`, sorts by version number, and deletes
    /// the oldest until at most `max_plan_revisions` remain.
    /// `<id>.v<n>.approved.json` snapshots are never touched — they are
    /// the write-once approval audit record. The snapshot whose version
    /// matches the current on-disk `<id>.json` is also protected so a
    /// non-monotonic write (e.g. a manual edit, or out-of-order writes
    /// that record a lower version after a higher one) cannot leave the
    /// live current plan without its matching versioned snapshot.
    fn gc_old_version_snapshots(&self, id: &PlanId) {
        let limit = self.options.max_plan_revisions;
        if limit == 0 {
            return;
        }
        // Determine the live current version (if any) so we can protect
        // its versioned snapshot from deletion regardless of where it
        // ranks numerically.
        let current_version: Option<u32> = self
            .current_path(id)
            .ok()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<PlanDocument>(&b).ok())
            .map(|p| p.version);

        let prefix = format!("{}.v", id.0);
        let suffix = ".json";
        let read = match std::fs::read_dir(&self.root) {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut versioned: Vec<(u32, PathBuf)> = Vec::new();
        for entry in read.flatten() {
            let name = match entry.file_name().into_string() {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !name.starts_with(&prefix) || !name.ends_with(suffix) {
                continue;
            }
            // Exclude `<id>.v<n>.approved.json` — preserve write-once snapshots.
            if name.ends_with(".approved.json") {
                continue;
            }
            let middle = &name[prefix.len()..name.len() - suffix.len()];
            // `middle` must be a bare version number for `<id>.v<n>.json`.
            // Anything else (future-suffixed variants) is skipped.
            let v: u32 = match middle.parse() {
                Ok(v) => v,
                Err(_) => continue,
            };
            versioned.push((v, entry.path()));
        }
        if versioned.len() <= limit {
            return;
        }
        versioned.sort_by_key(|(v, _)| *v);
        let to_remove = versioned.len() - limit;
        let mut removed = 0usize;
        for (v, path) in versioned.into_iter() {
            if removed >= to_remove {
                break;
            }
            // Never delete the snapshot that pairs with the live current
            // plan. If a non-monotonic save recorded a lower version after
            // a higher one, the numeric ordering alone would otherwise
            // pick the current version for deletion.
            if current_version == Some(v) {
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => {
                    removed += 1;
                    tracing::debug!(
                        plan_id = %id.0,
                        version = v,
                        "plan store GC: removed old version snapshot"
                    );
                }
                Err(e) => tracing::debug!(
                    plan_id = %id.0,
                    version = v,
                    error = %e,
                    "plan store GC: failed to remove old version snapshot"
                ),
            }
        }
    }
}

#[async_trait]
impl PlanStore for FsPlanStore {
    async fn create(&self, mut plan: PlanDocument) -> Result<PlanId, PlanError> {
        Self::validate_id(&plan.id)?;
        // Hold the cross-process lock across the exists-check + write so a
        // second process cannot create the same plan id concurrently.
        let _file_lock = self.lock_plan_file(&plan.id)?;
        if self.current_path(&plan.id)?.exists() {
            return Err(PlanError::Other(format!("plan {} already exists", plan.id)));
        }
        plan.created_at = OffsetDateTime::now_utc();
        plan.updated_at = plan.created_at;
        if matches!(plan.status, PlanStatus::Approved | PlanStatus::Executing) {
            return Err(PlanError::BadStatus(
                "cannot create a plan that is already approved/executing".into(),
            ));
        }
        redact_secrets_in_plan(&mut plan);
        let bytes = self.validate_plan_size(&plan)?;
        let cur = self.current_path(&plan.id)?;
        let ver = self.version_path(&plan.id, plan.version)?;
        self.write_atomic(&cur, &bytes)?;
        self.write_atomic(&ver, &bytes)?;
        self.append_index(&IndexEntry {
            timestamp: now_iso(),
            plan_id: plan.id.0.clone(),
            version: plan.version,
            status: plan.status,
            event: IndexEvent::Created,
            session_id: plan.session_id.as_ref().map(|s| s.as_str().to_string()),
        })?;
        Ok(plan.id)
    }

    async fn load(&self, plan_id: &PlanId) -> Result<PlanDocument, PlanError> {
        let path = self.current_path(plan_id)?;
        if !path.exists() {
            return Err(PlanError::NotFound(plan_id.0.clone()));
        }
        let bytes = std::fs::read(&path).map_err(|e| PlanError::Io {
            path: path.clone(),
            source: e,
        })?;
        let plan: PlanDocument = serde_json::from_slice(&bytes).map_err(|e| PlanError::Parse {
            path: path.clone(),
            source: e,
        })?;
        Ok(plan)
    }

    async fn save_revision(&self, plan: PlanDocument) -> Result<(), PlanError> {
        Self::validate_id(&plan.id)?;

        // Serialize the read-decide-write block for this plan id so two
        // concurrent revisions cannot both observe the same prior state
        // and race the approval-invalidation check. Different plan ids
        // do not block each other. The in-process mutex covers callers
        // sharing this process; the `<id>.lock` advisory lock extends the
        // same guarantee across separate processes.
        let lock = self.plan_lock(&plan.id).await;
        let _guard = lock.lock().await;
        let _file_lock = self.lock_plan_file(&plan.id)?;
        self.save_revision_locked(plan)
    }

    async fn list(&self, filter: PlanListFilter) -> Result<Vec<PlanSummary>, PlanError> {
        if !self.root.exists() {
            return Ok(Vec::new());
        }
        let mut out: Vec<PlanSummary> = Vec::new();
        let read = std::fs::read_dir(&self.root).map_err(|e| PlanError::Io {
            path: self.root.clone(),
            source: e,
        })?;
        for entry in read.flatten() {
            let p = entry.path();
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Only consider top-level current files: <plan-id>.json. Skip
            // versioned snapshots and the index file.
            if !name.ends_with(".json") || name.contains(".v") || name == "index.jsonl" {
                continue;
            }
            let bytes = match std::fs::read(&p) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let plan: PlanDocument = match serde_json::from_slice(&bytes) {
                Ok(p) => p,
                Err(_) => continue,
            };
            if let Some(status) = filter.status {
                if plan.status != status {
                    continue;
                }
            }
            if let Some(sid) = &filter.session_id {
                if plan.session_id.as_ref() != Some(sid) {
                    continue;
                }
            }
            out.push(plan.summary());
        }
        out.sort_by_key(|b| std::cmp::Reverse(b.updated_at));
        if let Some(lim) = filter.limit {
            out.truncate(lim);
        }
        Ok(out)
    }

    async fn approve(&self, req: PlanApprovalRequest) -> Result<PlanDocument, PlanError> {
        Self::validate_id(&req.plan_id)?;
        // Serialize the load + status transition + write against other
        // processes touching this plan id.
        let _file_lock = self.lock_plan_file(&req.plan_id)?;
        let mut plan = self.load(&req.plan_id).await?;
        if plan.version != req.approved_version {
            return Err(PlanError::VersionMismatch {
                expected: plan.version,
                got: req.approved_version,
            });
        }
        if matches!(plan.status, PlanStatus::Rejected | PlanStatus::Archived) {
            return Err(PlanError::BadStatus(format!(
                "cannot approve plan in status {:?}",
                plan.status
            )));
        }
        plan.status = PlanStatus::Approved;
        plan.updated_at = OffsetDateTime::now_utc();
        plan.approval = Some(PlanApproval {
            approved_at: plan.updated_at,
            approved_by: req.approved_by,
            approved_version: req.approved_version,
            notes: req.notes,
            scope: req.scope,
        });
        let bytes = self.validate_plan_size(&plan)?;
        let cur = self.current_path(&plan.id)?;
        self.write_atomic(&cur, &bytes)?;
        // Capture a write-once snapshot of the approved body. The
        // versioned snapshot `<id>.v<n>.json` is still updated by later
        // `save_revision` calls during execution, but `.approved.json`
        // must never change once written. The approved snapshot is
        // integrity-critical, so propagate any write failure.
        let approved_path = self.approved_version_path(&plan.id, plan.version)?;
        self.write_approved_snapshot(&approved_path, &bytes)?;
        self.append_index(&IndexEntry {
            timestamp: now_iso(),
            plan_id: plan.id.0.clone(),
            version: plan.version,
            status: plan.status,
            event: IndexEvent::Approved,
            session_id: plan.session_id.as_ref().map(|s| s.as_str().to_string()),
        })?;
        Ok(plan)
    }

    async fn reject(&self, req: PlanRejectRequest) -> Result<PlanDocument, PlanError> {
        Self::validate_id(&req.plan_id)?;
        let _file_lock = self.lock_plan_file(&req.plan_id)?;
        let mut plan = self.load(&req.plan_id).await?;
        if matches!(plan.status, PlanStatus::Archived) {
            return Err(PlanError::BadStatus("cannot reject archived plan".into()));
        }
        plan.status = PlanStatus::Rejected;
        plan.updated_at = OffsetDateTime::now_utc();
        plan.approval = None;
        if let Some(reason) = req.reason {
            // Stash the reason in metadata.last_rejection_reason without
            // touching plan body fields.
            let m = plan.metadata.as_object_mut();
            match m {
                Some(map) => {
                    map.insert(
                        "last_rejection_reason".into(),
                        serde_json::Value::String(reason),
                    );
                }
                None => {
                    plan.metadata = serde_json::json!({ "last_rejection_reason": reason });
                }
            }
        }
        let bytes = self.validate_plan_size(&plan)?;
        let cur = self.current_path(&plan.id)?;
        self.write_atomic(&cur, &bytes)?;
        self.append_index(&IndexEntry {
            timestamp: now_iso(),
            plan_id: plan.id.0.clone(),
            version: plan.version,
            status: plan.status,
            event: IndexEvent::Rejected,
            session_id: plan.session_id.as_ref().map(|s| s.as_str().to_string()),
        })?;
        Ok(plan)
    }

    async fn archive(&self, plan_id: &PlanId) -> Result<(), PlanError> {
        Self::validate_id(plan_id)?;
        let _file_lock = self.lock_plan_file(plan_id)?;
        let mut plan = self.load(plan_id).await?;
        plan.status = PlanStatus::Archived;
        plan.updated_at = OffsetDateTime::now_utc();
        let bytes = self.validate_plan_size(&plan)?;
        let cur = self.current_path(plan_id)?;
        self.write_atomic(&cur, &bytes)?;
        self.append_index(&IndexEntry {
            timestamp: now_iso(),
            plan_id: plan_id.0.clone(),
            version: plan.version,
            status: plan.status,
            event: IndexEvent::Archived,
            session_id: plan.session_id.as_ref().map(|s| s.as_str().to_string()),
        })?;
        Ok(())
    }

    async fn latest_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<PlanId>, PlanError> {
        let summaries = self
            .list(PlanListFilter {
                status: None,
                session_id: Some(session_id.clone()),
                limit: None,
            })
            .await?;
        Ok(summaries.into_iter().next().map(|s| s.id))
    }

    async fn writable(&self) -> bool {
        if self.ensure_root().is_err() {
            return false;
        }
        // Round-trip a probe file.
        let probe = self.root.join(".write-probe");
        match std::fs::write(&probe, b"ok") {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IndexEntry {
    timestamp: String,
    plan_id: String,
    version: u32,
    status: PlanStatus,
    event: IndexEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum IndexEvent {
    Created,
    Revised,
    Approved,
    Rejected,
    Archived,
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| String::new())
}

impl FsPlanStore {
    /// Best-effort blocking read of the on-disk current plan, used by
    /// `save_revision` to detect content drift while keeping the existing
    /// approval. Prefers the write-once `<id>.v<n>.approved.json` when one
    /// exists for the current version — that snapshot is the immutable
    /// record of what the reviewer actually approved, whereas the live
    /// `<id>.json` may have been overwritten with post-approval status
    /// transitions. Returns `None` when neither path exists or the file
    /// is unreadable — the caller treats that as "no comparison possible"
    /// rather than erroring out.
    fn load_existing_for_compare(&self, id: &PlanId) -> Option<PlanDocument> {
        let path = self.current_path(id).ok()?;
        if !path.exists() {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let current: PlanDocument = serde_json::from_slice(&bytes).ok()?;
        // Prefer the approved snapshot for the current version when
        // present. Falls back to the live current state if it's missing.
        let approved_path = self
            .approved_version_path(id, current.version)
            .ok()
            .filter(|p| p.exists());
        if let Some(p) = approved_path {
            if let Ok(b) = std::fs::read(&p) {
                if let Ok(approved) = serde_json::from_slice::<PlanDocument>(&b) {
                    return Some(approved);
                }
            }
        }
        Some(current)
    }
}

/// Returns true iff the body fields that an approval is bound to differ
/// between two plan revisions. We compare every field a reviewer would
/// have evaluated when approving — title, goal, summary, files, steps,
/// requested_tools, validations, checkpoints, risks, rollback_plan,
/// success_criteria, constraints, assumptions, metadata. We deliberately
/// do NOT compare `updated_at`, `status`, `execution`, or `approval`
/// since those move on every save.
fn plan_body_changed(prev: &PlanDocument, next: &PlanDocument) -> bool {
    prev.title != next.title
        || prev.goal != next.goal
        || prev.summary != next.summary
        || prev.user_request != next.user_request
        || prev.assumptions != next.assumptions
        || prev.constraints != next.constraints
        || prev.context_gathered != next.context_gathered
        || prev.files != next.files
        || prev.steps != next.steps
        || prev.requested_tools != next.requested_tools
        || prev.validations != next.validations
        || prev.checkpoints != next.checkpoints
        || prev.risks != next.risks
        || prev.rollback_plan != next.rollback_plan
        || prev.success_criteria != next.success_criteria
        || prev.metadata != next.metadata
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::artifact::{
        new_plan_id, PlanCreatedBy, PlanRiskLevel, PlanStep, PlanStepKind, PlanStepStatus,
    };
    use tempfile::TempDir;

    fn store() -> (FsPlanStore, TempDir) {
        let tmp = TempDir::new().unwrap();
        let s = FsPlanStore::new(tmp.path(), PlanningOptions::default());
        (s, tmp)
    }

    fn small_plan() -> PlanDocument {
        let mut p = PlanDocument::new_draft(new_plan_id(), "T", "u");
        p.steps.push(PlanStep {
            id: "1".into(),
            title: "look".into(),
            description: String::new(),
            status: PlanStepStatus::Pending,
            kind: PlanStepKind::Inspect,
            expected_tools: Vec::new(),
            expected_files: Vec::new(),
            expected_commands: Vec::new(),
            expected_outcome: None,
            risk: PlanRiskLevel::Low,
            requires_user_approval: false,
            validation: None,
        });
        p.status = PlanStatus::PendingApproval;
        p
    }

    #[tokio::test]
    // A plan can be created and round-tripped from disk.
    async fn create_and_load() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        let back = s.load(&id).await.unwrap();
        assert_eq!(back.id, id);
        assert_eq!(back.steps.len(), 1);
    }

    #[tokio::test]
    // Listing returns only top-level current plans, sorted newest first.
    async fn list_skips_versioned_snapshots() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        // Save a revision; should produce <id>.v2.json but list still only
        // shows one entry.
        let mut rev = s.load(&id).await.unwrap();
        rev.version = 2;
        rev.created_by = PlanCreatedBy::RevisedByUser;
        s.save_revision(rev).await.unwrap();
        let items = s.list(PlanListFilter::default()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
    }

    #[tokio::test]
    // Approval bumps status, persists approval metadata, and rejects mismatched versions.
    async fn approve_flow() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();
        let approved = s
            .approve(PlanApprovalRequest {
                plan_id: id.clone(),
                approved_by: PlanApprover::TestHarness,
                approved_version: v,
                scope: PlanApprovalScope::FullPlan,
                notes: None,
            })
            .await
            .unwrap();
        assert_eq!(approved.status, PlanStatus::Approved);
        // Bumping version invalidates approval; approving the wrong version errors.
        let mut rev = s.load(&id).await.unwrap();
        rev.version = 2;
        rev.created_by = PlanCreatedBy::RevisedByUser;
        s.save_revision(rev).await.unwrap();
        let after_rev = s.load(&id).await.unwrap();
        assert_eq!(after_rev.status, PlanStatus::PendingApproval);
        let err = s
            .approve(PlanApprovalRequest {
                plan_id: id,
                approved_by: PlanApprover::TestHarness,
                approved_version: 1,
                scope: PlanApprovalScope::FullPlan,
                notes: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, PlanError::VersionMismatch { .. }));
    }

    #[tokio::test]
    // Reject keeps the plan loadable but flips status; archive ditto.
    async fn reject_then_archive() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        s.reject(PlanRejectRequest {
            plan_id: id.clone(),
            reason: Some("nope".into()),
        })
        .await
        .unwrap();
        let p = s.load(&id).await.unwrap();
        assert_eq!(p.status, PlanStatus::Rejected);
        assert_eq!(p.metadata["last_rejection_reason"], "nope");
        s.archive(&id).await.unwrap();
        let p = s.load(&id).await.unwrap();
        assert_eq!(p.status, PlanStatus::Archived);
    }

    #[tokio::test]
    // Corrupted plan file does not block listing of healthy plans.
    async fn corruption_does_not_block_listing() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        // Write a garbage plan file with a different name.
        let bad = s.root.join("plan_garbage.json");
        std::fs::write(&bad, b"not json").unwrap();
        let items = s.list(PlanListFilter::default()).await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
    }

    #[tokio::test]
    // An approved plan whose body is mutated in place (without bumping
    // version or setting created_by = RevisedBy*) is forced back to
    // PendingApproval. Defends against a caller forgetting to mark the
    // revision as such — a stale approval would otherwise authorize
    // content the user never reviewed.
    async fn save_revision_invalidates_approval_on_silent_content_change() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();
        s.approve(PlanApprovalRequest {
            plan_id: id.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
        // Same version, same created_by, but the title changed. Approval
        // must be cleared.
        let mut tampered = s.load(&id).await.unwrap();
        assert_eq!(tampered.status, PlanStatus::Approved);
        tampered.title = "tampered".into();
        s.save_revision(tampered).await.unwrap();
        let after = s.load(&id).await.unwrap();
        assert_eq!(after.status, PlanStatus::PendingApproval);
        assert!(after.approval.is_none());
    }

    #[tokio::test]
    // A version-only bump (e.g. revising a previously rejected plan) is
    // also caught even if the caller leaves `created_by` alone.
    async fn save_revision_invalidates_approval_on_version_bump_only() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();
        s.approve(PlanApprovalRequest {
            plan_id: id.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
        let mut bumped = s.load(&id).await.unwrap();
        bumped.version = v + 1; // do NOT change `created_by`
        s.save_revision(bumped).await.unwrap();
        let after = s.load(&id).await.unwrap();
        assert_eq!(after.status, PlanStatus::PendingApproval);
        assert!(after.approval.is_none());
    }

    #[tokio::test]
    // Planning options enforce the byte budget.
    async fn rejects_oversized_plan() {
        let tmp = TempDir::new().unwrap();
        let s = FsPlanStore::new(
            tmp.path(),
            PlanningOptions {
                max_plan_bytes: 256,
                ..PlanningOptions::default()
            },
        );
        let mut p = small_plan();
        p.summary = "x".repeat(2048);
        let err = s.create(p).await.unwrap_err();
        assert!(matches!(err, PlanError::TooLarge { .. }));
    }

    #[tokio::test]
    // After approval, subsequent driver-style status transitions through
    // Executing -> Completed -> Failed at the same version must leave the
    // `.approved.json` write-once snapshot untouched, even though
    // `.v<n>.json` gets overwritten with the latest state.
    async fn approved_snapshot_survives_status_transitions() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();
        let approved = s
            .approve(PlanApprovalRequest {
                plan_id: id.clone(),
                approved_by: PlanApprover::TestHarness,
                approved_version: v,
                scope: PlanApprovalScope::FullPlan,
                notes: None,
            })
            .await
            .unwrap();
        let approved_path = s.root.join(format!("{}.v{}.approved.json", id.0, v));
        assert!(approved_path.exists(), "approved snapshot must be written");
        let original_approved_bytes = std::fs::read(&approved_path).unwrap();

        // Simulate the act driver: status -> Executing, save_revision.
        let mut running = approved.clone();
        running.status = PlanStatus::Executing;
        s.save_revision(running).await.unwrap();

        // Simulate completion: status -> Completed, save_revision.
        let mut done = s.load(&id).await.unwrap();
        done.status = PlanStatus::Completed;
        s.save_revision(done).await.unwrap();

        // The versioned snapshot has been overwritten with the Completed
        // state, but the approved snapshot must still match its capture.
        let ver_path = s.root.join(format!("{}.v{}.json", id.0, v));
        let ver_doc: PlanDocument = serde_json::from_slice(&std::fs::read(&ver_path).unwrap())
            .expect("versioned snapshot parses");
        assert_eq!(ver_doc.status, PlanStatus::Completed);

        let approved_after = std::fs::read(&approved_path).unwrap();
        assert_eq!(
            approved_after, original_approved_bytes,
            "approved snapshot must be write-once and not change after execution"
        );
        let approved_doc: PlanDocument =
            serde_json::from_slice(&approved_after).expect("approved snapshot parses");
        assert_eq!(approved_doc.status, PlanStatus::Approved);
    }

    #[tokio::test]
    // Race many concurrent `save_revision` calls against the same plan id.
    // With the per-plan async mutex in place each call observes the
    // serialized state, so the final on-disk plan body equals exactly one
    // writer's input and the index records every revision. Without the
    // lock, two writers could pass the body-drift / approval-invalidation
    // check against the same prior state and the loser's mutated body
    // could end up paired with the prior approval.
    async fn save_revision_serializes_concurrent_writers() {
        use std::sync::Arc;
        let tmp = TempDir::new().unwrap();
        let s = Arc::new(FsPlanStore::new(
            tmp.path(),
            PlanningOptions {
                max_plan_revisions: 64,
                ..PlanningOptions::default()
            },
        ));
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();

        let n: u32 = 16;
        let mut handles = Vec::with_capacity(n as usize);
        for i in 0..n {
            let s = s.clone();
            let id = id.clone();
            handles.push(tokio::spawn(async move {
                let mut rev = s.load(&id).await.unwrap();
                rev.version = 2 + i; // unique version per writer
                rev.created_by = PlanCreatedBy::RevisedByUser;
                rev.title = format!("writer-{i}");
                s.save_revision(rev).await
            }));
        }
        let mut titles_attempted: Vec<String> = Vec::new();
        for (i, h) in handles.into_iter().enumerate() {
            h.await.unwrap().expect("save_revision must succeed");
            titles_attempted.push(format!("writer-{i}"));
        }

        // The final on-disk body must match one of the attempted titles
        // (no torn writes, no garbled state).
        let final_plan = s.load(&id).await.unwrap();
        assert!(
            titles_attempted.contains(&final_plan.title),
            "final title {:?} must equal one of {:?}",
            final_plan.title,
            titles_attempted
        );

        // Every revision must have appended one Revised entry to the
        // index — a torn write would lose entries.
        let idx = std::fs::read_to_string(s.index_path()).unwrap();
        let revised: Vec<&str> = idx
            .lines()
            .filter(|l| l.contains("\"event\":\"revised\""))
            .collect();
        assert_eq!(revised.len(), n as usize);
    }

    #[tokio::test]
    // Revising a plan well past `max_plan_revisions` must succeed and the
    // store must GC the oldest versioned snapshots so exactly
    // `max_plan_revisions` `.v<n>.json` files remain. Any
    // `.approved.json` snapshot must be preserved through the sweep.
    async fn save_revision_soft_caps_with_gc() {
        let tmp = TempDir::new().unwrap();
        let limit: usize = 5;
        let s = FsPlanStore::new(
            tmp.path(),
            PlanningOptions {
                max_plan_revisions: limit,
                ..PlanningOptions::default()
            },
        );
        let p = small_plan();
        let id = p.id.clone();
        let v0 = p.version;
        s.create(p).await.unwrap();
        s.approve(PlanApprovalRequest {
            plan_id: id.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v0,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
        let approved_path = s.root.join(format!("{}.v{}.approved.json", id.0, v0));
        assert!(approved_path.exists());

        // Revise the plan limit + 5 times. Should never error.
        let extras: u32 = 5;
        for i in 0..(limit as u32 + extras) {
            let mut rev = s.load(&id).await.unwrap();
            rev.version = v0 + 1 + i;
            rev.created_by = PlanCreatedBy::RevisedByUser;
            rev.title = format!("rev-{i}");
            s.save_revision(rev).await.expect("save must not error");
        }

        // Count `.v<n>.json` snapshots (not `.approved.json`).
        let mut versions: Vec<u32> = Vec::new();
        for entry in std::fs::read_dir(&s.root).unwrap().flatten() {
            let name = entry.file_name().into_string().unwrap();
            let prefix = format!("{}.v", id.0);
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            if name.ends_with(".approved.json") {
                continue;
            }
            let middle = &name[prefix.len()..name.len() - ".json".len()];
            if let Ok(v) = middle.parse::<u32>() {
                versions.push(v);
            }
        }
        assert_eq!(
            versions.len(),
            limit,
            "snapshot count {} must equal limit {limit}",
            versions.len()
        );

        // The approved snapshot must survive the GC sweep.
        assert!(
            approved_path.exists(),
            "approved snapshot must be preserved by GC"
        );
    }

    #[tokio::test]
    // GC must protect the snapshot whose version matches the live current
    // plan, even when other snapshots have HIGHER numeric versions. This
    // covers the non-monotonic case: e.g. an out-of-order concurrent
    // writer or a manual edit left v3.json / v4.json on disk while the
    // current plan is still at v2. Numeric-only eviction would target v2
    // first; the protection rule must keep it.
    async fn gc_protects_current_version_when_higher_snapshots_exist() {
        let tmp = TempDir::new().unwrap();
        let limit: usize = 2;
        let s = FsPlanStore::new(
            tmp.path(),
            PlanningOptions {
                max_plan_revisions: limit,
                ..PlanningOptions::default()
            },
        );
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        // Current plan is at version 1 (`v1.json`).
        let v1_path = s.root.join(format!("{}.v1.json", id.0));
        assert!(v1_path.exists());

        // Simulate an out-of-order / racing writer that left higher
        // version snapshots on disk without advancing the current
        // pointer. Write them directly so `<id>.json` still says v1.
        let body = std::fs::read(&v1_path).unwrap();
        let v3_path = s.root.join(format!("{}.v3.json", id.0));
        let v4_path = s.root.join(format!("{}.v4.json", id.0));
        std::fs::write(&v3_path, &body).unwrap();
        std::fs::write(&v4_path, &body).unwrap();

        // Now perform a save_revision at the same low version 1 (idempotent
        // re-save) so GC fires. With limit=2 and three snapshots present
        // (v1, v3, v4), one must be evicted. Numeric ranking alone would
        // pick v1 (the lowest) — which is the CURRENT version — so
        // without the protection the live current plan would lose its
        // matching snapshot.
        let mut rev = s.load(&id).await.unwrap();
        // Keep version = 1; title bump just to trigger a write.
        rev.title = "still-v1".into();
        rev.created_by = PlanCreatedBy::RevisedByUser;
        s.save_revision(rev).await.unwrap();

        // GC must have kept the current version's snapshot.
        assert!(
            v1_path.exists(),
            "GC must preserve the snapshot pairing with current plan version (v1) \
             even though v3 and v4 outrank it numerically"
        );
        // And the snapshot count after GC must equal the limit.
        let mut versions: Vec<u32> = Vec::new();
        let prefix = format!("{}.v", id.0);
        for entry in std::fs::read_dir(&s.root).unwrap().flatten() {
            let name = entry.file_name().into_string().unwrap();
            if !name.starts_with(&prefix) || !name.ends_with(".json") {
                continue;
            }
            if name.ends_with(".approved.json") {
                continue;
            }
            let middle = &name[prefix.len()..name.len() - ".json".len()];
            if let Ok(v) = middle.parse::<u32>() {
                versions.push(v);
            }
        }
        versions.sort();
        assert_eq!(
            versions.len(),
            limit,
            "snapshot count {} must equal limit {limit} after GC",
            versions.len()
        );
        assert!(
            versions.contains(&1),
            "v1 (current) must remain among kept snapshots: {versions:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    // If the approved-snapshot temp+rename fails (e.g. read-only plans
    // dir), `approve` must surface the error rather than silently
    // dropping it. The approved snapshot is integrity-critical.
    async fn approve_propagates_approved_snapshot_write_error() {
        use std::os::unix::fs::PermissionsExt;
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();

        // Make the plans directory read-only so the temp file create or
        // the rename inside `write_approved_snapshot` fails.
        let mut perms = std::fs::metadata(&s.root).unwrap().permissions();
        let original = perms.mode();
        perms.set_mode(0o555);
        std::fs::set_permissions(&s.root, perms).unwrap();

        let err = s
            .approve(PlanApprovalRequest {
                plan_id: id,
                approved_by: PlanApprover::TestHarness,
                approved_version: v,
                scope: PlanApprovalScope::FullPlan,
                notes: None,
            })
            .await;

        // Restore perms so TempDir drop can clean up.
        let mut perms = std::fs::metadata(&s.root).unwrap().permissions();
        perms.set_mode(original);
        std::fs::set_permissions(&s.root, perms).unwrap();

        let err = err.expect_err("approve must fail when approved snapshot write fails");
        assert!(matches!(err, PlanError::Io { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    // `save_revision`'s first-transition-into-Approved path captures the
    // write-once `.approved.json`. If that write fails, the revision
    // itself must fail rather than silently dropping the snapshot.
    async fn save_revision_propagates_approved_snapshot_write_error() {
        use std::os::unix::fs::PermissionsExt;
        let (s, _g) = store();
        // Start in PendingApproval, then save a revision that transitions
        // into Approved with an approval payload. The first-approval
        // branch in `save_revision` will then try to write
        // `.approved.json`. We make the dir read-only just before that
        // save by performing it directly (no chmod between writes).
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();

        // Build a revision that flips status to Approved with an
        // approval payload aligned to the same version (so the
        // invalidation check doesn't reset it).
        let mut rev = s.load(&id).await.unwrap();
        rev.status = PlanStatus::Approved;
        rev.approval = Some(PlanApproval {
            approved_at: OffsetDateTime::now_utc(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            notes: None,
            scope: PlanApprovalScope::FullPlan,
        });

        // Make the plans dir read-only so the .approved.json write
        // fails. The current-file and version-file writes go to the
        // same dir, so they may fail first — either way, the result
        // must be an `Err`, not a silent drop of the approved snapshot.
        let mut perms = std::fs::metadata(&s.root).unwrap().permissions();
        let original = perms.mode();
        perms.set_mode(0o555);
        std::fs::set_permissions(&s.root, perms).unwrap();

        let res = s.save_revision(rev).await;

        let mut perms = std::fs::metadata(&s.root).unwrap().permissions();
        perms.set_mode(original);
        std::fs::set_permissions(&s.root, perms).unwrap();

        let err = res.expect_err("save_revision must propagate write failure");
        assert!(matches!(err, PlanError::Io { .. }), "got: {err:?}");
    }

    #[tokio::test]
    // Approving twice for the same plan version must not error: the
    // second approve sees an existing `.approved.json` and is expected
    // to leave it in place. This pins the temp-and-rename + noclobber
    // behavior: a partial-file race / repeated approve is tolerated as
    // a no-op, not an integrity failure.
    async fn approve_is_idempotent_when_snapshot_already_exists() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();

        s.approve(PlanApprovalRequest {
            plan_id: id.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
        let approved_path = s.root.join(format!("{}.v{}.approved.json", id.0, v));
        let original = std::fs::read(&approved_path).unwrap();

        // A second approve at the same version must succeed and must
        // leave the existing approved snapshot byte-for-byte unchanged.
        s.approve(PlanApprovalRequest {
            plan_id: id,
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: Some("second pass".into()),
        })
        .await
        .expect("second approve must not error when snapshot exists");
        let after = std::fs::read(&approved_path).unwrap();
        assert_eq!(
            after, original,
            "approved snapshot must remain the original capture"
        );
    }

    #[tokio::test]
    // Crash-safety regression: if a pre-existing `.approved.json` is
    // present (e.g. a partial leftover from a prior crash, or an
    // intentional capture), the writer must not panic and must not
    // overwrite it. Approving over a pre-existing file is a no-op.
    async fn approve_leaves_preexisting_approved_snapshot_intact() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        let v = p.version;
        s.create(p).await.unwrap();

        // Pre-create an `.approved.json` (simulating a leftover from a
        // crashed earlier writer). Use distinctive bytes so we can
        // assert it was not overwritten.
        let approved_path = s.root.join(format!("{}.v{}.approved.json", id.0, v));
        std::fs::write(&approved_path, b"preexisting").unwrap();

        s.approve(PlanApprovalRequest {
            plan_id: id,
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .expect("approve must succeed when a preexisting snapshot is present");

        let after = std::fs::read(&approved_path).unwrap();
        assert_eq!(after, b"preexisting", "preexisting snapshot must be kept");
    }

    #[tokio::test]
    // `save_revision_atomic` holds the per-plan lock across load +
    // validator + save, so a concurrent writer cannot mutate the on-disk
    // plan between the validator inspecting it and the save persisting
    // the edited body. Spawn a background `save_revision` that races
    // the atomic call; the atomic call must observe its post-write state
    // at validator time (not the pre-spawn snapshot), so the final
    // on-disk title equals one writer's output without torn state.
    async fn save_revision_atomic_serializes_against_concurrent_writer() {
        use std::sync::Arc;
        let (s, _g) = store();
        let s = Arc::new(s);
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();

        // Spawn a background writer that races the atomic edit. It bumps
        // version and title, just like a concurrent `plans revise`.
        let s_bg = s.clone();
        let id_bg = id.clone();
        let bg = tokio::spawn(async move {
            let mut rev = s_bg.load(&id_bg).await.unwrap();
            rev.version += 1;
            rev.created_by = PlanCreatedBy::RevisedByUser;
            rev.title = "background-writer".into();
            s_bg.save_revision(rev).await
        });

        // Wait for the background save to complete before our atomic
        // edit, so we KNOW the validator must observe its state (the
        // version we read at start is stale). This is the load-plus-stale
        // edit pattern the CLI fix prevents: the user opened the plan,
        // a concurrent writer bumped its version, and now the user's save
        // is racing in. With atomic we want the validator to see the
        // post-concurrent state and decide accordingly.
        bg.await.unwrap().expect("background save must succeed");

        // Build the "edited" plan based on the stale pre-bg snapshot.
        let mut edited = s.load(&id).await.unwrap();
        edited.title = "atomic-edit".into();

        // The validator must see the latest on-disk version, NOT the
        // stale one. Capture the observed version into a cell.
        let observed_version = std::sync::Mutex::new(0u32);
        let validator = |current: &PlanDocument, _edited: &PlanDocument| -> Result<(), PlanError> {
            *observed_version.lock().unwrap() = current.version;
            Ok(())
        };
        s.save_revision_atomic(edited, validator)
            .await
            .expect("atomic save must succeed");

        // The validator must have observed the post-bg version (>=2),
        // not the pre-bg version (1). This is the load-after-lock
        // guarantee — without it, the CLI's load-then-save dance would
        // have validated against the stale pre-bg state.
        assert!(
            *observed_version.lock().unwrap() >= 2,
            "validator must observe post-concurrent-writer version, got {}",
            *observed_version.lock().unwrap()
        );

        // The final on-disk title is the atomic edit's.
        let final_plan = s.load(&id).await.unwrap();
        assert_eq!(final_plan.title, "atomic-edit");
    }

    #[tokio::test]
    // `save_revision_atomic` surfaces a validator error without writing.
    // The on-disk plan must be unchanged when the validator returns Err.
    async fn save_revision_atomic_propagates_validator_error() {
        let (s, _g) = store();
        let p = small_plan();
        let id = p.id.clone();
        s.create(p).await.unwrap();
        let pre = s.load(&id).await.unwrap();

        let mut edited = pre.clone();
        edited.title = "should-not-persist".into();
        let validator =
            |_current: &PlanDocument, _edited: &PlanDocument| -> Result<(), PlanError> {
                Err(PlanError::Invalid("validator says no".into()))
            };
        let err = s.save_revision_atomic(edited, validator).await.unwrap_err();
        assert!(matches!(err, PlanError::Invalid(_)));

        // On-disk state unchanged.
        let after = s.load(&id).await.unwrap();
        assert_eq!(after.title, pre.title);
    }

    // Env-var protocol for the cross-process flock test. When
    // `PLAN_STORE_XPROC_CHILD` is set, the test below re-execs itself as a
    // worker that performs N read-modify-write increments on a shared
    // plan's `metadata.counter` and exits.
    const XPROC_ROOT: &str = "PLAN_STORE_XPROC_ROOT";
    const XPROC_PLAN: &str = "PLAN_STORE_XPROC_PLAN";
    const XPROC_ITERS: &str = "PLAN_STORE_XPROC_ITERS";
    const XPROC_CHILD: &str = "PLAN_STORE_XPROC_CHILD";

    // Each child loads the plan, bumps `metadata.counter` by one, bumps
    // the version, and saves — `iters` times. The exclusive `<id>.lock`
    // flock must make the whole load-modify-save atomic across processes,
    // so no increment is lost.
    fn xproc_worker(root: PathBuf, plan_id: PlanId, iters: u32) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let store = FsPlanStore::new(
                root,
                PlanningOptions {
                    max_plan_revisions: 100_000,
                    ..PlanningOptions::default()
                },
            );
            for _ in 0..iters {
                store
                    .mutate_revision(&plan_id, |plan| {
                        let counter = plan
                            .metadata
                            .get("counter")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        plan.metadata = serde_json::json!({ "counter": counter + 1 });
                        plan.version += 1;
                        plan.created_by = PlanCreatedBy::RevisedByUser;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
        });
    }

    #[test]
    // Cross-process serialization: spawn several OS processes that each
    // do N read-modify-write increments to the same plan's counter
    // concurrently. The `<id>.lock` advisory flock must serialize the
    // load-modify-save across processes so the final counter equals
    // procs * iters with no lost updates. (In-process mutexes cannot help
    // here — these are genuinely separate processes.)
    fn save_revision_serializes_across_processes() {
        // Worker branch: this same test binary is re-exec'd with the env
        // vars set, runs the increments, and exits before reaching the
        // parent logic below.
        if std::env::var(XPROC_CHILD).is_ok() {
            let root = PathBuf::from(std::env::var(XPROC_ROOT).unwrap());
            let plan_id = PlanId(std::env::var(XPROC_PLAN).unwrap());
            let iters: u32 = std::env::var(XPROC_ITERS).unwrap().parse().unwrap();
            xproc_worker(root, plan_id, iters);
            std::process::exit(0);
        }

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let store = FsPlanStore::new(root.clone(), PlanningOptions::default());
        let mut plan = small_plan();
        plan.metadata = serde_json::json!({ "counter": 0 });
        let id = plan.id.clone();
        rt.block_on(store.create(plan)).unwrap();

        let procs: u32 = 4;
        let iters: u32 = 25;
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..procs {
            let child = std::process::Command::new(&exe)
                // Re-run only this test, single-threaded, with output
                // captured so the worker's process exit is the signal.
                .args([
                    "--exact",
                    "plan::store::tests::save_revision_serializes_across_processes",
                    "--nocapture",
                ])
                .env(XPROC_CHILD, "1")
                .env(XPROC_ROOT, &root)
                .env(XPROC_PLAN, &id.0)
                .env(XPROC_ITERS, iters.to_string())
                .spawn()
                .expect("spawn worker process");
            children.push(child);
        }
        for mut child in children {
            let status = child.wait().expect("wait for worker");
            assert!(status.success(), "worker process failed: {status:?}");
        }

        // Final counter must reflect every increment from every process.
        let final_plan = rt.block_on(store.load(&id)).unwrap();
        let counter = final_plan
            .metadata
            .get("counter")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        assert_eq!(
            counter,
            (procs * iters) as u64,
            "lost updates: counter {counter} != {procs} procs * {iters} iters"
        );
    }
}
