use std::sync::Arc;

use async_trait::async_trait;

use crate::patch::PatchSet;
use crate::workspace::{
    ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch, SearchQuery, Workspace,
    WorkspaceCapabilities, WorkspaceError, WorkspacePath,
};

/// Wraps any [`Workspace`] and denies writes/apply_patch. Returns structured
/// `WorkspaceError`s with `workspace.write_denied` so callers can surface a
/// clear diagnostic.
pub struct ReadOnlyWorkspace {
    inner: Arc<dyn Workspace>,
}

impl ReadOnlyWorkspace {
    pub fn new(inner: Arc<dyn Workspace>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Workspace for ReadOnlyWorkspace {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
        self.inner.read_file(path).await
    }

    async fn write_file(
        &self,
        path: &WorkspacePath,
        _content: &[u8],
    ) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::write_denied(path.clone()))
    }

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::write_denied(path.clone()))
    }

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
        self.inner.list_files(query).await
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
        self.inner.search(query).await
    }

    async fn apply_patch(&self, _patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
        Err(WorkspaceError::new(
            "workspace.write_denied",
            "read-only workspace refuses apply_patch",
        ))
    }

    async fn diff(&self, base: DiffBase) -> Result<PatchSet, WorkspaceError> {
        self.inner.diff(base).await
    }

    fn capabilities(&self) -> WorkspaceCapabilities {
        let mut c = self.inner.capabilities();
        c.can_write = false;
        c.can_apply_patch = false;
        c.can_execute_commands = false;
        c
    }

    fn filesystem_root(&self) -> Option<std::path::PathBuf> {
        self.inner.filesystem_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::InMemoryWorkspace;

    /// ReadOnlyWorkspace returns workspace.write_denied for writes and apply_patch.
    #[tokio::test]
    async fn write_and_apply_are_denied() {
        let inner =
            Arc::new(InMemoryWorkspace::from_iter(vec![("a.txt", b"hi\n".to_vec())]).unwrap());
        let ws = ReadOnlyWorkspace::new(inner);
        let p = WorkspacePath::new("a.txt").unwrap();
        let err = ws.write_file(&p, b"x").await.unwrap_err();
        assert_eq!(err.code, "workspace.write_denied");
        let p2 = WorkspacePath::new("b.txt").unwrap();
        let patch = PatchSet::from_files(vec![PatchSet::build_create(p2, b"x")]);
        let err = ws.apply_patch(&patch).await.unwrap_err();
        assert_eq!(err.code, "workspace.write_denied");
    }
}
