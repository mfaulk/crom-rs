use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use crate::patch::{FilePatch, PatchSet};
use crate::workspace::{
    compute_hash, is_binary_bytes, ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch,
    SearchQuery, Workspace, WorkspaceCapabilities, WorkspaceError, WorkspacePath,
};

/// A workspace wrapper that records mutations as patches without mutating
/// the base workspace.
///
/// Writes are stored in an internal "pending" map and (a) reflected back to
/// `read_file` so subsequent reads see the model's intended state, but (b)
/// never propagated to the base. `take_patch()` consumes the pending changes
/// as a [`PatchSet`].
pub struct PatchOnlyWorkspace {
    base: Arc<dyn Workspace>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Mutated files relative to base. `None` = pending deletion.
    pending: BTreeMap<WorkspacePath, Option<Vec<u8>>>,
}

impl PatchOnlyWorkspace {
    pub fn new(base: Arc<dyn Workspace>) -> Self {
        Self {
            base,
            state: Mutex::new(State::default()),
        }
    }

    /// Consume the recorded patches and return them as a `PatchSet`. The
    /// pending state is cleared afterwards.
    pub async fn take_patch(&self) -> Result<PatchSet, WorkspaceError> {
        let mut state = self.state.lock().await;
        let pending = std::mem::take(&mut state.pending);
        let mut files: Vec<FilePatch> = Vec::new();
        for (path, content) in pending {
            match content {
                Some(new) => match self.base.read_file(&path).await {
                    Ok(existing) => {
                        if existing.bytes != new {
                            files.push(PatchSet::build_modify(path.clone(), &existing.bytes, &new));
                        }
                    }
                    Err(e) if e.code == "workspace.not_found" => {
                        files.push(PatchSet::build_create(path.clone(), &new));
                    }
                    Err(e) => return Err(e),
                },
                None => files.push(FilePatch::Delete { path }),
            }
        }
        Ok(PatchSet::from_files(files))
    }

    /// Peek at the recorded patches without consuming them.
    pub async fn snapshot_patch(&self) -> Result<PatchSet, WorkspaceError> {
        let state = self.state.lock().await;
        let mut files: Vec<FilePatch> = Vec::new();
        for (path, content) in &state.pending {
            match content {
                Some(new) => match self.base.read_file(path).await {
                    Ok(existing) => {
                        if existing.bytes != *new {
                            files.push(PatchSet::build_modify(path.clone(), &existing.bytes, new));
                        }
                    }
                    Err(e) if e.code == "workspace.not_found" => {
                        files.push(PatchSet::build_create(path.clone(), new));
                    }
                    Err(e) => return Err(e),
                },
                None => files.push(FilePatch::Delete { path: path.clone() }),
            }
        }
        Ok(PatchSet::from_files(files))
    }
}

#[async_trait]
impl Workspace for PatchOnlyWorkspace {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
        let state = self.state.lock().await;
        if let Some(entry) = state.pending.get(path) {
            return match entry {
                Some(bytes) => {
                    let is_binary = is_binary_bytes(bytes);
                    let hash = compute_hash(bytes);
                    Ok(FileContent {
                        bytes: bytes.clone(),
                        is_binary,
                        content_hash: Some(hash),
                        language: None,
                    })
                }
                None => Err(WorkspaceError::not_found(path.clone())),
            };
        }
        drop(state);
        self.base.read_file(path).await
    }

    async fn write_file(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let mut state = self.state.lock().await;
        state.pending.insert(path.clone(), Some(content.to_vec()));
        Ok(())
    }

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        let mut state = self.state.lock().await;
        state.pending.insert(path.clone(), None);
        Ok(())
    }

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
        let mut base = self.base.list_files(query.clone()).await?;
        let state = self.state.lock().await;
        // Apply pending mutations: add newly created files, remove deleted ones.
        for (path, entry) in &state.pending {
            match entry {
                Some(_) if !base.contains(path) => base.push(path.clone()),
                None => base.retain(|p| p != path),
                _ => {}
            }
        }
        Ok(base)
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
        // PatchOnly delegates search to the base; pending writes are typically
        // small relative to the workspace and search results that lag are
        // acceptable for headless usage.
        self.base.search(query).await
    }

    async fn apply_patch(&self, patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
        // Record the patch as additional pending changes.
        let mut state = self.state.lock().await;
        let mut report = ApplyPatchReport {
            files_changed: 0,
            files_skipped: 0,
            failures: Vec::new(),
        };
        for f in &patch.files {
            match f {
                FilePatch::Create { path, content, .. } => {
                    state.pending.insert(path.clone(), Some(content.clone()));
                    report.files_changed += 1;
                }
                FilePatch::Modify {
                    path, new_content, ..
                } => {
                    state
                        .pending
                        .insert(path.clone(), Some(new_content.clone()));
                    report.files_changed += 1;
                }
                FilePatch::Delete { path } => {
                    state.pending.insert(path.clone(), None);
                    report.files_changed += 1;
                }
                FilePatch::Rename { from, to } => {
                    state.pending.insert(from.clone(), None);
                    // Read the source content from base or pending and reuse it.
                    drop(state);
                    let src = self.read_file(from).await?;
                    let mut state2 = self.state.lock().await;
                    state2.pending.insert(to.clone(), Some(src.bytes));
                    state = state2;
                    report.files_changed += 1;
                }
            }
        }
        Ok(report)
    }

    async fn diff(&self, _base: DiffBase) -> Result<PatchSet, WorkspaceError> {
        self.snapshot_patch().await
    }

    fn capabilities(&self) -> WorkspaceCapabilities {
        let mut c = self.base.capabilities();
        c.can_write = true;
        c.can_apply_patch = true;
        c.can_execute_commands = false;
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

    /// PatchOnlyWorkspace stores writes without mutating the base workspace.
    #[tokio::test]
    async fn writes_dont_mutate_base() {
        let base =
            Arc::new(InMemoryWorkspace::from_iter(vec![("a.txt", b"old\n".to_vec())]).unwrap());
        let ws = PatchOnlyWorkspace::new(base.clone());
        let p = WorkspacePath::new("a.txt").unwrap();
        ws.write_file(&p, b"new\n").await.unwrap();
        let from_ws = ws.read_file(&p).await.unwrap();
        assert_eq!(from_ws.bytes, b"new\n");
        let from_base = base.read_file(&p).await.unwrap();
        assert_eq!(from_base.bytes, b"old\n");
        let patch = ws.take_patch().await.unwrap();
        assert_eq!(patch.summary.files_modified, 1);
    }

    /// PatchOnlyWorkspace records creates as Create entries when path is absent in base.
    #[tokio::test]
    async fn records_create_as_create() {
        let base = Arc::new(InMemoryWorkspace::new());
        let ws = PatchOnlyWorkspace::new(base.clone());
        let p = WorkspacePath::new("new.txt").unwrap();
        ws.write_file(&p, b"hi\n").await.unwrap();
        let patch = ws.take_patch().await.unwrap();
        assert_eq!(patch.summary.files_created, 1);
    }
}
