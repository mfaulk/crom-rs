use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::patch::{FilePatch, PatchSet};
use crate::workspace::{
    compute_hash, is_binary_bytes, ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch,
    SearchQuery, Workspace, WorkspaceCapabilities, WorkspaceError, WorkspacePath,
};

/// Reads from `base`, writes to an in-memory overlay. The base is never
/// mutated; `diff()` returns the overlay's changes against the base.
pub struct OverlayWorkspace {
    base: Arc<dyn Workspace>,
    overlay: RwLock<BTreeMap<WorkspacePath, Option<Vec<u8>>>>,
}

impl OverlayWorkspace {
    pub fn new(base: Arc<dyn Workspace>) -> Self {
        Self {
            base,
            overlay: RwLock::new(BTreeMap::new()),
        }
    }
}

#[async_trait]
impl Workspace for OverlayWorkspace {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
        if let Some(entry) = self.overlay.read().await.get(path).cloned() {
            return match entry {
                Some(bytes) => {
                    let is_binary = is_binary_bytes(&bytes);
                    let hash = compute_hash(&bytes);
                    Ok(FileContent {
                        bytes,
                        is_binary,
                        content_hash: Some(hash),
                        language: None,
                    })
                }
                None => Err(WorkspaceError::not_found(path.clone())),
            };
        }
        self.base.read_file(path).await
    }

    async fn write_file(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        self.overlay
            .write()
            .await
            .insert(path.clone(), Some(content.to_vec()));
        Ok(())
    }

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        self.overlay.write().await.insert(path.clone(), None);
        Ok(())
    }

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
        let mut base = self.base.list_files(query).await?;
        let overlay = self.overlay.read().await;
        for (p, entry) in overlay.iter() {
            match entry {
                Some(_) if !base.contains(p) => base.push(p.clone()),
                None => base.retain(|x| x != p),
                _ => {}
            }
        }
        Ok(base)
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
        self.base.search(query).await
    }

    async fn apply_patch(&self, patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
        let mut overlay = self.overlay.write().await;
        let mut report = ApplyPatchReport {
            files_changed: 0,
            files_skipped: 0,
            failures: Vec::new(),
        };
        for f in &patch.files {
            match f {
                FilePatch::Create { path, content, .. } => {
                    // Mirror InMemoryWorkspace: Create must target a path that
                    // is not visible in the merged (base + overlay) view. Hold
                    // the write lock across `base.read_file().await` for
                    // linearizability with other overlay mutators; the await
                    // does not block the executor.
                    let base_lookup = match overlay.get(path) {
                        Some(Some(_)) => Ok(true),
                        Some(None) => Ok(false), // tombstoned — Create is allowed
                        None => match self.base.read_file(path).await {
                            Ok(_) => Ok(true),
                            Err(e) if e.code == "workspace.not_found" => Ok(false),
                            Err(e) => Err(e),
                        },
                    };
                    let exists = match base_lookup {
                        Ok(v) => v,
                        Err(e) => {
                            report.failures.push(e);
                            report.files_skipped += 1;
                            continue;
                        }
                    };
                    if exists {
                        report.failures.push(
                            WorkspaceError::new(
                                "patch.create_exists",
                                format!("create target already exists: {path}"),
                            )
                            .with_path(path.clone()),
                        );
                        report.files_skipped += 1;
                    } else {
                        overlay.insert(path.clone(), Some(content.clone()));
                        report.files_changed += 1;
                    }
                }
                FilePatch::Modify {
                    path,
                    new_content,
                    old_content_hash,
                    ..
                } => {
                    // Mirror InMemoryWorkspace: when old_content_hash is set,
                    // verify against the overlay's merged view of `path` —
                    // not silently overwrite. None overlay entry → consult
                    // base. None base → not_found.
                    if let Some(expected) = old_content_hash {
                        let lookup = match overlay.get(path) {
                            Some(Some(bytes)) => Ok(Some(bytes.clone())),
                            Some(None) => Ok(None), // tombstoned
                            None => match self.base.read_file(path).await {
                                Ok(c) => Ok(Some(c.bytes)),
                                Err(e) if e.code == "workspace.not_found" => Ok(None),
                                Err(e) => Err(e),
                            },
                        };
                        let current = match lookup {
                            Ok(v) => v,
                            Err(e) => {
                                report.failures.push(e);
                                report.files_skipped += 1;
                                continue;
                            }
                        };
                        match current {
                            Some(existing) => {
                                if compute_hash(&existing) != *expected {
                                    report.failures.push(
                                        WorkspaceError::new(
                                            "patch.hash_mismatch",
                                            format!("file content hash mismatch: {path}"),
                                        )
                                        .with_path(path.clone()),
                                    );
                                    report.files_skipped += 1;
                                    continue;
                                }
                            }
                            None => {
                                report
                                    .failures
                                    .push(WorkspaceError::not_found(path.clone()));
                                report.files_skipped += 1;
                                continue;
                            }
                        }
                    }
                    overlay.insert(path.clone(), Some(new_content.clone()));
                    report.files_changed += 1;
                }
                FilePatch::Delete { path } => {
                    overlay.insert(path.clone(), None);
                    report.files_changed += 1;
                }
                FilePatch::Rename { from, to } => {
                    let existing = overlay.get(from).cloned();
                    overlay.insert(from.clone(), None);
                    let bytes = match existing {
                        Some(Some(bytes)) => bytes,
                        _ => {
                            drop(overlay);
                            let content = self.base.read_file(from).await?;
                            overlay = self.overlay.write().await;
                            content.bytes
                        }
                    };
                    overlay.insert(to.clone(), Some(bytes));
                    report.files_changed += 1;
                }
            }
        }
        Ok(report)
    }

    async fn diff(&self, _base: DiffBase) -> Result<PatchSet, WorkspaceError> {
        let overlay = self.overlay.read().await;
        let mut files: Vec<FilePatch> = Vec::new();
        for (path, entry) in overlay.iter() {
            match entry {
                Some(new_content) => match self.base.read_file(path).await {
                    Ok(existing) => {
                        if existing.bytes != *new_content {
                            files.push(PatchSet::build_modify(
                                path.clone(),
                                &existing.bytes,
                                new_content,
                            ));
                        }
                    }
                    Err(e) if e.code == "workspace.not_found" => {
                        files.push(PatchSet::build_create(path.clone(), new_content));
                    }
                    Err(e) => return Err(e),
                },
                None => files.push(FilePatch::Delete { path: path.clone() }),
            }
        }
        Ok(PatchSet::from_files(files))
    }

    fn capabilities(&self) -> WorkspaceCapabilities {
        let mut c = self.base.capabilities();
        c.can_write = true;
        c.can_apply_patch = true;
        c
    }

    fn filesystem_root(&self) -> Option<std::path::PathBuf> {
        self.base.filesystem_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    fn modify(path: WorkspacePath, new: &[u8], old_hash: Option<String>) -> FilePatch {
        FilePatch::Modify {
            path,
            old_content_hash: old_hash,
            new_content_hash: Some(compute_hash(new)),
            unified_diff: String::new(),
            new_content: new.to_vec(),
            is_binary: false,
        }
    }

    /// OverlayWorkspace returns diff against base for overlay writes.
    #[tokio::test]
    async fn overlay_diff_returns_changes() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"hi\n".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base);
        let p = WorkspacePath::new("a.txt").unwrap();
        ws.write_file(&p, b"hello\n").await.unwrap();
        let patch = ws.diff(DiffBase::Initial).await.unwrap();
        assert_eq!(patch.summary.files_modified, 1);
        let new_p = WorkspacePath::new("z.txt").unwrap();
        ws.write_file(&new_p, b"new\n").await.unwrap();
        let patch = ws.diff(DiffBase::Initial).await.unwrap();
        assert_eq!(patch.summary.files_modified, 1);
        assert_eq!(patch.summary.files_created, 1);
    }

    /// Create targeting a file that already exists in base must fail and not
    /// silently mutate the file.
    #[tokio::test]
    async fn apply_patch_create_on_existing_base_file_fails() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"old".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        let patch = PatchSet::from_files(vec![FilePatch::Create {
            path: p.clone(),
            content: b"new".to_vec(),
            is_binary: false,
        }]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 0);
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].code, "patch.create_exists");
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"old");
    }

    /// Create targeting a file that the overlay already has (writeover) must
    /// fail. The overlay's own view of the file is what matters.
    #[tokio::test]
    async fn apply_patch_create_on_existing_overlay_file_fails() {
        let base = Arc::new(InMemoryWorkspace::new());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("new.txt").unwrap();
        ws.write_file(&p, b"first").await.unwrap();
        let patch = PatchSet::from_files(vec![FilePatch::Create {
            path: p.clone(),
            content: b"second".to_vec(),
            is_binary: false,
        }]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures[0].code, "patch.create_exists");
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"first");
    }

    /// Create on a previously tombstoned path is allowed.
    #[tokio::test]
    async fn apply_patch_create_on_tombstoned_path_succeeds() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"old".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        ws.delete_file(&p).await.unwrap();
        let patch = PatchSet::from_files(vec![FilePatch::Create {
            path: p.clone(),
            content: b"reborn".to_vec(),
            is_binary: false,
        }]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 1);
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"reborn");
    }

    /// Modify with a stale old_content_hash must fail and not overwrite the
    /// existing content (silent lost-edit was the original bug).
    #[tokio::test]
    async fn apply_patch_modify_stale_hash_fails() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"current".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        let patch = PatchSet::from_files(vec![modify(p.clone(), b"new", Some("deadbeef".into()))]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 0);
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures[0].code, "patch.hash_mismatch");
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"current");
    }

    /// Modify with a matching hash succeeds.
    #[tokio::test]
    async fn apply_patch_modify_matching_hash_succeeds() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"current".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        let hash = compute_hash(b"current");
        let patch = PatchSet::from_files(vec![modify(p.clone(), b"updated", Some(hash))]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 1);
        assert!(report.failures.is_empty());
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"updated");
    }

    /// Modify hashes the overlay's view, not base, so a previous overlay
    /// write becomes the new baseline for the hash check.
    #[tokio::test]
    async fn apply_patch_modify_hashes_overlay_view() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"base".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        ws.write_file(&p, b"overlay").await.unwrap();
        // Stale hash of base content must NOT match the overlay's view.
        let base_hash = compute_hash(b"base");
        let bad = PatchSet::from_files(vec![modify(p.clone(), b"x", Some(base_hash))]);
        let report = ws.apply_patch(&bad).await.unwrap();
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures[0].code, "patch.hash_mismatch");
        // Hash of the overlay view matches.
        let overlay_hash = compute_hash(b"overlay");
        let good = PatchSet::from_files(vec![modify(p.clone(), b"final", Some(overlay_hash))]);
        let report = ws.apply_patch(&good).await.unwrap();
        assert_eq!(report.files_changed, 1);
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"final");
    }

    /// Modify targeting a missing file fails with not_found (when hash check
    /// is requested).
    #[tokio::test]
    async fn apply_patch_modify_missing_path_fails() {
        let base = Arc::new(InMemoryWorkspace::new());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("missing.txt").unwrap();
        let patch = PatchSet::from_files(vec![modify(p.clone(), b"new", Some("anything".into()))]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures[0].code, "workspace.not_found");
    }

    /// Modify on a path the overlay has tombstoned still fails as not_found
    /// even when base has the file (the merged view says "deleted").
    #[tokio::test]
    async fn apply_patch_modify_tombstoned_path_fails_not_found() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"keep".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        ws.delete_file(&p).await.unwrap();
        let hash = compute_hash(b"keep");
        let patch = PatchSet::from_files(vec![modify(p.clone(), b"new", Some(hash))]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 0);
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.failures[0].code, "workspace.not_found");
        // Tombstone is unchanged.
        assert_eq!(
            ws.read_file(&p).await.unwrap_err().code,
            "workspace.not_found"
        );
    }

    /// Modify without an old_content_hash skips the hash check (existing
    /// permissive behavior, matching InMemoryWorkspace).
    #[tokio::test]
    async fn apply_patch_modify_no_hash_skips_check() {
        let base =
            Arc::new(InMemoryWorkspace::from_pairs(vec![("a.txt", b"old".to_vec())]).unwrap());
        let ws = OverlayWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        let patch = PatchSet::from_files(vec![modify(p.clone(), b"new", None)]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 1);
        assert_eq!(ws.read_file(&p).await.unwrap().bytes, b"new");
    }
}
