use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ignore::WalkBuilder;

use crate::error::AgentSdkError;
use crate::patch::{FilePatch, PatchSet};
use crate::workspace::{
    compute_hash, is_binary_bytes, ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch,
    SearchQuery, Workspace, WorkspaceCapabilities, WorkspaceError, WorkspacePath,
};

/// Filesystem-backed workspace, sandboxed to a root directory.
///
/// Path safety reuses the same invariants as crom-tools: any incoming
/// `WorkspacePath` is workspace-relative and rejects `..` traversal; the
/// implementation additionally canonicalizes its root, refuses symlink
/// escapes, and never writes outside the canonical root.
pub struct FilesystemWorkspace {
    root: PathBuf,
}

impl FilesystemWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, AgentSdkError> {
        let path = root.into();
        if !path.exists() {
            return Err(AgentSdkError::workspace(
                "workspace.root_missing",
                format!("workspace root does not exist: {}", path.display()),
            ));
        }
        let canon = path.canonicalize().map_err(|e| {
            AgentSdkError::workspace(
                "workspace.root_canonicalize_failed",
                format!("canonicalize {}: {e}", path.display()),
            )
        })?;
        Ok(Self { root: canon })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a `WorkspacePath` to an absolute filesystem path under the
    /// canonicalized root. Refuses symlink escapes.
    fn resolve(&self, path: &WorkspacePath, must_exist: bool) -> Result<PathBuf, WorkspaceError> {
        let joined = path.join_under(&self.root);
        // Reuse the path-sandbox semantics from the tools crate by re-implementing
        // the strict-enough subset here: walk components, refuse `..` (already
        // refused by WorkspacePath::new), then canonicalize / verify start.
        if must_exist {
            let canon = joined
                .canonicalize()
                .map_err(|_| WorkspaceError::not_found(path.clone()))?;
            if !canon.starts_with(&self.root) {
                return Err(WorkspaceError::path_outside_root(path.as_str()));
            }
            Ok(canon)
        } else {
            // Walk back to the nearest existing ancestor and canonicalize that.
            let mut existing = joined.clone();
            let mut tail = Vec::new();
            while existing.symlink_metadata().is_err() {
                let name = existing
                    .file_name()
                    .map(|n| n.to_os_string())
                    .ok_or_else(|| WorkspaceError::path_outside_root(path.as_str()))?;
                tail.push(name);
                if !existing.pop() {
                    return Err(WorkspaceError::path_outside_root(path.as_str()));
                }
            }
            let meta = existing
                .symlink_metadata()
                .map_err(|e| WorkspaceError::read_failed(path.clone(), e.to_string()))?;
            if meta.file_type().is_symlink() {
                return Err(WorkspaceError::path_outside_root(path.as_str()));
            }
            let canon_existing = existing
                .canonicalize()
                .map_err(|e| WorkspaceError::read_failed(path.clone(), e.to_string()))?;
            if !canon_existing.starts_with(&self.root) {
                return Err(WorkspaceError::path_outside_root(path.as_str()));
            }
            let mut out = canon_existing;
            for seg in tail.into_iter().rev() {
                out.push(seg);
            }
            Ok(out)
        }
    }
}

#[async_trait]
impl Workspace for FilesystemWorkspace {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
        let resolved = self.resolve(path, true)?;
        let bytes = tokio::fs::read(&resolved)
            .await
            .map_err(|e| WorkspaceError::read_failed(path.clone(), e.to_string()))?;
        let is_binary = is_binary_bytes(&bytes);
        let hash = compute_hash(&bytes);
        Ok(FileContent {
            bytes,
            is_binary,
            content_hash: Some(hash),
            language: None,
        })
    }

    async fn write_file(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError> {
        let resolved = self.resolve(path, false)?;
        if let Some(parent) = resolved.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string()))?;
        }
        tokio::fs::write(&resolved, content)
            .await
            .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string()))
    }

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        let resolved = self.resolve(path, true)?;
        tokio::fs::remove_file(&resolved)
            .await
            .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string()))
    }

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
        let root = self.root.clone();
        let limit = query.limit;
        let prefix = query.prefix.clone();
        let glob_pattern = query.glob.clone();
        let result = tokio::task::spawn_blocking(move || {
            let matcher = glob_pattern
                .as_ref()
                .and_then(|p| globset::Glob::new(p).ok())
                .map(|g| g.compile_matcher());
            let mut out = Vec::new();
            for entry in WalkBuilder::new(&root).follow_links(false).build() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let rel = match entry.path().strip_prefix(&root) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                let rel_str = match rel.to_str() {
                    Some(s) => s.replace(std::path::MAIN_SEPARATOR, "/"),
                    None => continue,
                };
                if rel_str.is_empty() {
                    continue;
                }
                if let Some(p) = &prefix {
                    if !rel_str.starts_with(p.as_str()) {
                        continue;
                    }
                }
                if let Some(m) = &matcher {
                    if !m.is_match(&rel_str) {
                        continue;
                    }
                }
                out.push(WorkspacePath::from_validated(rel_str));
                if let Some(l) = limit {
                    if out.len() >= l {
                        break;
                    }
                }
            }
            out
        })
        .await
        .map_err(|e| WorkspaceError::new("workspace.list_failed", e.to_string()))?;
        Ok(result)
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let files = self
            .list_files(FileQuery {
                prefix: query.prefix.clone(),
                ..FileQuery::new()
            })
            .await?;
        let mut out = Vec::new();
        let regex = if query.regex {
            Some(
                regex::RegexBuilder::new(&query.pattern)
                    .case_insensitive(!query.case_sensitive)
                    .build()
                    .map_err(|e| WorkspaceError::new("workspace.search_failed", e.to_string()))?,
            )
        } else {
            None
        };
        for path in files {
            let bytes = match tokio::fs::read(self.resolve(&path, true)?).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            if is_binary_bytes(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (idx, line) in text.lines().enumerate() {
                let hit = match &regex {
                    Some(r) => r.find(line).map(|m| m.start()),
                    None => {
                        if query.case_sensitive {
                            line.find(&query.pattern)
                        } else {
                            line.to_lowercase().find(&query.pattern.to_lowercase())
                        }
                    }
                };
                if let Some(col) = hit {
                    out.push(SearchMatch {
                        path: path.clone(),
                        line: (idx + 1) as u32,
                        column: (col + 1) as u32,
                        line_text: line.to_string(),
                    });
                    if let Some(l) = query.limit {
                        if out.len() >= l {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    async fn apply_patch(&self, patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
        let mut report = ApplyPatchReport {
            files_changed: 0,
            files_skipped: 0,
            failures: Vec::new(),
        };
        for file in &patch.files {
            let res = match file {
                FilePatch::Create { path, content, .. } => {
                    let resolved = match self.resolve(path, false) {
                        Ok(r) => r,
                        Err(e) => {
                            report.failures.push(e);
                            report.files_skipped += 1;
                            continue;
                        }
                    };
                    if resolved.exists() {
                        report.failures.push(WorkspaceError::new(
                            "patch.create_exists",
                            format!("file already exists: {path}"),
                        ));
                        report.files_skipped += 1;
                        continue;
                    }
                    if let Some(parent) = resolved.parent() {
                        if let Err(e) = tokio::fs::create_dir_all(parent).await {
                            report
                                .failures
                                .push(WorkspaceError::write_failed(path.clone(), e.to_string()));
                            report.files_skipped += 1;
                            continue;
                        }
                    }
                    tokio::fs::write(&resolved, content)
                        .await
                        .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string()))
                }
                FilePatch::Modify {
                    path,
                    new_content,
                    old_content_hash,
                    ..
                } => {
                    if let Some(expected) = old_content_hash {
                        let existing = match tokio::fs::read(self.resolve(path, true)?).await {
                            Ok(b) => b,
                            Err(_) => {
                                report
                                    .failures
                                    .push(WorkspaceError::not_found(path.clone()));
                                report.files_skipped += 1;
                                continue;
                            }
                        };
                        if compute_hash(&existing) != *expected {
                            report.failures.push(WorkspaceError::new(
                                "patch.hash_mismatch",
                                format!("file content hash mismatch: {path}"),
                            ));
                            report.files_skipped += 1;
                            continue;
                        }
                    }
                    let resolved = match self.resolve(path, false) {
                        Ok(r) => r,
                        Err(e) => {
                            report.failures.push(e);
                            report.files_skipped += 1;
                            continue;
                        }
                    };
                    tokio::fs::write(&resolved, new_content)
                        .await
                        .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string()))
                }
                FilePatch::Delete { path } => match self.resolve(path, true) {
                    Ok(r) => tokio::fs::remove_file(&r)
                        .await
                        .map_err(|e| WorkspaceError::write_failed(path.clone(), e.to_string())),
                    Err(e) => Err(e),
                },
                FilePatch::Rename { from, to } => {
                    let r_from = match self.resolve(from, true) {
                        Ok(r) => r,
                        Err(e) => {
                            report.failures.push(e);
                            report.files_skipped += 1;
                            continue;
                        }
                    };
                    let r_to = match self.resolve(to, false) {
                        Ok(r) => r,
                        Err(e) => {
                            report.failures.push(e);
                            report.files_skipped += 1;
                            continue;
                        }
                    };
                    if let Some(parent) = r_to.parent() {
                        if let Err(e) = tokio::fs::create_dir_all(parent).await {
                            report
                                .failures
                                .push(WorkspaceError::write_failed(to.clone(), e.to_string()));
                            report.files_skipped += 1;
                            continue;
                        }
                    }
                    tokio::fs::rename(&r_from, &r_to)
                        .await
                        .map_err(|e| WorkspaceError::write_failed(to.clone(), e.to_string()))
                }
            };
            match res {
                Ok(()) => report.files_changed += 1,
                Err(e) => {
                    report.failures.push(e);
                    report.files_skipped += 1;
                }
            }
        }
        Ok(report)
    }

    async fn diff(&self, base: DiffBase) -> Result<PatchSet, WorkspaceError> {
        let baseline = match base {
            DiffBase::Initial => std::collections::BTreeMap::new(),
            DiffBase::Snapshot(s) => s,
        };
        let files = self.list_files(FileQuery::new()).await?;
        let mut current = std::collections::BTreeMap::new();
        for path in files {
            let resolved = match self.resolve(&path, true) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let bytes = match tokio::fs::read(&resolved).await {
                Ok(b) => b,
                Err(_) => continue,
            };
            current.insert(path, bytes);
        }
        let mut entries: Vec<FilePatch> = Vec::new();
        for (path, content) in &current {
            match baseline.get(path) {
                None => entries.push(PatchSet::build_create(path.clone(), content)),
                Some(old) if old != content => {
                    entries.push(PatchSet::build_modify(path.clone(), old, content));
                }
                _ => {}
            }
        }
        for path in baseline.keys() {
            if !current.contains_key(path) {
                entries.push(FilePatch::Delete { path: path.clone() });
            }
        }
        Ok(PatchSet::from_files(entries))
    }

    fn capabilities(&self) -> WorkspaceCapabilities {
        WorkspaceCapabilities::full()
    }

    fn filesystem_root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, FilesystemWorkspace) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), b"fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("README.md"), b"# hi\n").unwrap();
        let ws = FilesystemWorkspace::new(dir.path().to_path_buf()).unwrap();
        (dir, ws)
    }

    /// FilesystemWorkspace reads files and lists them with proper path normalization.
    #[tokio::test]
    async fn read_and_list() {
        let (_dir, ws) = setup();
        let p = WorkspacePath::new("src/main.rs").unwrap();
        let f = ws.read_file(&p).await.unwrap();
        assert_eq!(f.bytes, b"fn main() {}\n");
        let list = ws.list_files(FileQuery::new()).await.unwrap();
        assert!(list.iter().any(|p| p.as_str() == "src/main.rs"));
        assert!(list.iter().any(|p| p.as_str() == "README.md"));
    }

    /// FilesystemWorkspace path guards reject paths that escape the root.
    #[tokio::test]
    async fn rejects_path_traversal() {
        let (_dir, ws) = setup();
        // The WorkspacePath constructor already rejects ..; ensure the
        // workspace rejects symlink-escaping paths constructed via
        // from_validated as a sanity-net.
        let err = WorkspacePath::new("../passwd").unwrap_err();
        assert!(err.kind == crate::error::AgentSdkErrorKind::InvalidRequest);
        // A bogus inner path that points outside the root after symlink
        // resolution would surface in resolve; we exercise the simple case.
        let outside = WorkspacePath::from_validated("nope.txt".into());
        let err = ws.read_file(&outside).await.unwrap_err();
        assert_eq!(err.code, "workspace.not_found");
    }

    /// FilesystemWorkspace can apply a create patch and produce a diff.
    #[tokio::test]
    async fn apply_patch_creates_file() {
        let (_dir, ws) = setup();
        let p = WorkspacePath::new("new.txt").unwrap();
        let patch = PatchSet::from_files(vec![PatchSet::build_create(p.clone(), b"hi\n")]);
        let report = ws.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_changed, 1);
        let f = ws.read_file(&p).await.unwrap();
        assert_eq!(f.bytes, b"hi\n");
    }
}
