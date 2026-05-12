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
                FilePatch::Create { path, content, .. }
                | FilePatch::Modify {
                    path,
                    new_content: content,
                    ..
                } => {
                    overlay.insert(path.clone(), Some(content.clone()));
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

    /// OverlayWorkspace returns diff against base for overlay writes.
    #[tokio::test]
    async fn overlay_diff_returns_changes() {
        let base =
            Arc::new(InMemoryWorkspace::from_iter(vec![("a.txt", b"hi\n".to_vec())]).unwrap());
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
}
