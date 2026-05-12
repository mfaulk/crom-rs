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
//!     index.jsonl                    # one line per write (never rewritten)
//! ```
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

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::options::SessionId;

use super::artifact::{
    PlanApproval, PlanApprovalScope, PlanApprover, PlanDocument, PlanId, PlanStatus, PlanSummary,
};
use super::deviation::redact_secrets_in_plan;
use super::errors::PlanError;
use super::options::PlanningOptions;

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
#[derive(Debug, Clone)]
pub struct FsPlanStore {
    root: PathBuf,
    options: PlanningOptions,
}

impl FsPlanStore {
    pub fn new(cwd: impl Into<PathBuf>, options: PlanningOptions) -> Self {
        let cwd = cwd.into();
        let root = cwd.join(".agent").join("plans");
        Self { root, options }
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

    /// Write-once create of an `<id>.v<n>.approved.json` snapshot. Uses
    /// `OpenOptions::create_new(true)` so an already-captured approval is
    /// never overwritten. `AlreadyExists` is returned as `Ok(())` because
    /// preserving the prior capture is the point.
    fn write_approved_snapshot(&self, target: &Path, bytes: &[u8]) -> Result<(), PlanError> {
        self.ensure_root()?;
        use std::io::Write as _;
        let mut f = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
            Err(e) => {
                return Err(PlanError::Io {
                    path: target.to_path_buf(),
                    source: e,
                })
            }
        };
        f.write_all(bytes).map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
        })?;
        f.sync_data().map_err(|e| PlanError::Io {
            path: target.to_path_buf(),
            source: e,
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
        if plan.version as usize > self.options.max_plan_revisions {
            return Err(PlanError::TooManyRevisions {
                got: plan.version,
                limit: self.options.max_plan_revisions,
            });
        }
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
}

#[async_trait]
impl PlanStore for FsPlanStore {
    async fn create(&self, mut plan: PlanDocument) -> Result<PlanId, PlanError> {
        Self::validate_id(&plan.id)?;
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

    async fn save_revision(&self, mut plan: PlanDocument) -> Result<(), PlanError> {
        Self::validate_id(&plan.id)?;
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
            // place — that's the audit record.
            let _ = self.write_approved_snapshot(&approved, &bytes);
        }

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
        // must never change once written.
        let approved_path = self.approved_version_path(&plan.id, plan.version)?;
        let _ = self.write_approved_snapshot(&approved_path, &bytes);
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
}
