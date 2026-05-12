use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::patch::{FilePatch, PatchSet};
use crate::workspace::{
    compute_hash, is_binary_bytes, ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch,
    SearchQuery, Workspace, WorkspaceCapabilities, WorkspaceError, WorkspacePath,
};

/// In-memory workspace: a `BTreeMap<WorkspacePath, Vec<u8>>` snapshot of
/// files. Reads, writes, list, search, and patch apply all run against the
/// in-memory map; no filesystem is touched. Useful for tests, web services,
/// and any host that has files in memory already.
#[derive(Clone)]
pub struct InMemoryWorkspace {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    files: BTreeMap<WorkspacePath, Vec<u8>>,
    initial: BTreeMap<WorkspacePath, Vec<u8>>,
}

impl InMemoryWorkspace {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                files: BTreeMap::new(),
                initial: BTreeMap::new(),
            })),
        }
    }

    /// Build an in-memory workspace from a list of (path, content) pairs.
    pub fn from_iter<I, P, B>(iter: I) -> Result<Self, crate::error::AgentSdkError>
    where
        I: IntoIterator<Item = (P, B)>,
        P: TryInto<WorkspacePath, Error = crate::error::AgentSdkError>,
        B: Into<Vec<u8>>,
    {
        let mut files = BTreeMap::new();
        for (p, b) in iter {
            let path = p.try_into()?;
            files.insert(path, b.into());
        }
        let initial = files.clone();
        Ok(Self {
            inner: Arc::new(RwLock::new(Inner { files, initial })),
        })
    }

    /// Read the in-memory map directly. Useful in tests.
    pub async fn snapshot(&self) -> BTreeMap<WorkspacePath, Vec<u8>> {
        self.inner.read().await.files.clone()
    }

    /// Replace the "initial" snapshot used for [`diff`]. Useful when you
    /// want to rebase the diff baseline.
    pub async fn reset_initial(&self) {
        let mut g = self.inner.write().await;
        g.initial = g.files.clone();
    }
}

impl Default for InMemoryWorkspace {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Workspace for InMemoryWorkspace {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
        let g = self.inner.read().await;
        let bytes = g
            .files
            .get(path)
            .cloned()
            .ok_or_else(|| WorkspaceError::not_found(path.clone()))?;
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
        let mut g = self.inner.write().await;
        g.files.insert(path.clone(), content.to_vec());
        Ok(())
    }

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
        let mut g = self.inner.write().await;
        g.files
            .remove(path)
            .map(|_| ())
            .ok_or_else(|| WorkspaceError::not_found(path.clone()))
    }

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
        let g = self.inner.read().await;
        let matcher = build_glob(&query.glob);
        let mut out: Vec<WorkspacePath> = g
            .files
            .keys()
            .filter(|p| match (&query.prefix, &matcher) {
                (Some(prefix), _) if !p.as_str().starts_with(prefix.as_str()) => false,
                (_, Some(m)) => m.is_match(p.as_str()),
                _ => true,
            })
            .cloned()
            .collect();
        if let Some(limit) = query.limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
        let g = self.inner.read().await;
        let mut out = Vec::new();
        let regex = if query.regex {
            Some(
                regex_from_pattern(&query.pattern, query.case_sensitive)
                    .map_err(|e| WorkspaceError::new("workspace.search_failed", e))?,
            )
        } else {
            None
        };
        for (path, content) in g.files.iter() {
            if let Some(p) = &query.prefix {
                if !path.as_str().starts_with(p.as_str()) {
                    continue;
                }
            }
            if is_binary_bytes(content) {
                continue;
            }
            let text = String::from_utf8_lossy(content);
            for (line_idx, line) in text.lines().enumerate() {
                let hit = match &regex {
                    Some(r) => r.find(line).map(|m| m.start()),
                    None => find_substring(line, &query.pattern, query.case_sensitive),
                };
                if let Some(col) = hit {
                    out.push(SearchMatch {
                        path: path.clone(),
                        line: (line_idx + 1) as u32,
                        column: (col + 1) as u32,
                        line_text: line.to_string(),
                    });
                    if let Some(limit) = query.limit {
                        if out.len() >= limit {
                            return Ok(out);
                        }
                    }
                }
            }
        }
        Ok(out)
    }

    async fn apply_patch(&self, patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
        let mut g = self.inner.write().await;
        let mut report = ApplyPatchReport {
            files_changed: 0,
            files_skipped: 0,
            failures: Vec::new(),
        };
        for file in &patch.files {
            match file {
                FilePatch::Create { path, content, .. } => {
                    if g.files.contains_key(path) {
                        report.failures.push(
                            WorkspaceError::new(
                                "patch.create_exists",
                                format!("create target already exists: {path}"),
                            )
                            .with_path(path.clone()),
                        );
                        report.files_skipped += 1;
                    } else {
                        g.files.insert(path.clone(), content.clone());
                        report.files_changed += 1;
                    }
                }
                FilePatch::Modify {
                    path,
                    new_content,
                    old_content_hash,
                    ..
                } => {
                    if let Some(expected) = old_content_hash {
                        match g.files.get(path) {
                            Some(existing) => {
                                if compute_hash(existing) != *expected {
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
                    g.files.insert(path.clone(), new_content.clone());
                    report.files_changed += 1;
                }
                FilePatch::Delete { path } => match g.files.remove(path) {
                    Some(_) => report.files_changed += 1,
                    None => {
                        report
                            .failures
                            .push(WorkspaceError::not_found(path.clone()));
                        report.files_skipped += 1;
                    }
                },
                FilePatch::Rename { from, to } => match g.files.remove(from) {
                    Some(bytes) => {
                        g.files.insert(to.clone(), bytes);
                        report.files_changed += 1;
                    }
                    None => {
                        report
                            .failures
                            .push(WorkspaceError::not_found(from.clone()));
                        report.files_skipped += 1;
                    }
                },
            }
        }
        Ok(report)
    }

    async fn diff(&self, base: DiffBase) -> Result<PatchSet, WorkspaceError> {
        let g = self.inner.read().await;
        let baseline = match base {
            DiffBase::Initial => g.initial.clone(),
            DiffBase::Snapshot(s) => s,
        };
        let mut files: Vec<FilePatch> = Vec::new();
        // Modifications and creations.
        for (path, new_content) in g.files.iter() {
            match baseline.get(path) {
                None => files.push(PatchSet::build_create(path.clone(), new_content)),
                Some(old) if old != new_content => {
                    files.push(PatchSet::build_modify(path.clone(), old, new_content));
                }
                _ => {}
            }
        }
        // Deletions.
        for path in baseline.keys() {
            if !g.files.contains_key(path) {
                files.push(FilePatch::Delete { path: path.clone() });
            }
        }
        Ok(PatchSet::from_files(files))
    }

    fn capabilities(&self) -> WorkspaceCapabilities {
        WorkspaceCapabilities {
            can_read: true,
            can_write: true,
            can_list: true,
            can_search: true,
            can_execute_commands: false,
            can_apply_patch: true,
        }
    }
}

fn build_glob(pat: &Option<String>) -> Option<globset::GlobMatcher> {
    pat.as_ref()
        .and_then(|p| globset::Glob::new(p).ok())
        .map(|g| g.compile_matcher())
}

fn regex_from_pattern(pattern: &str, case_sensitive: bool) -> Result<regex::Regex, String> {
    let mut builder = regex::RegexBuilder::new(pattern);
    builder.case_insensitive(!case_sensitive);
    builder.build().map_err(|e| e.to_string())
}

fn find_substring(line: &str, pat: &str, case_sensitive: bool) -> Option<usize> {
    if case_sensitive {
        line.find(pat)
    } else {
        let l = line.to_lowercase();
        let p = pat.to_lowercase();
        l.find(&p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> InMemoryWorkspace {
        InMemoryWorkspace::from_iter(vec![
            ("src/lib.rs", b"fn main() {}\n".to_vec()),
            ("README.md", b"# Hello\n".to_vec()),
        ])
        .unwrap()
    }

    /// InMemoryWorkspace reads, writes, lists, and searches as expected.
    #[tokio::test]
    async fn read_write_list_search() {
        let w = ws();
        let p = WorkspacePath::new("src/lib.rs").unwrap();
        let f = w.read_file(&p).await.unwrap();
        assert_eq!(f.bytes, b"fn main() {}\n");
        w.write_file(&p, b"fn new() {}\n").await.unwrap();
        let after = w.read_file(&p).await.unwrap();
        assert_eq!(after.bytes, b"fn new() {}\n");
        let listed = w.list_files(FileQuery::new()).await.unwrap();
        assert_eq!(listed.len(), 2);
        let hits = w.search(SearchQuery::substring("new")).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path.as_str(), "src/lib.rs");
    }

    /// apply_patch with mismatching hash produces a failure, doesn't change the file.
    #[tokio::test]
    async fn apply_patch_hash_mismatch_skips() {
        let w = ws();
        let p = WorkspacePath::new("README.md").unwrap();
        let patch = PatchSet::from_files(vec![FilePatch::Modify {
            path: p.clone(),
            old_content_hash: Some("deadbeef".into()),
            new_content_hash: None,
            unified_diff: "--- a/README.md\n+++ b/README.md\n".to_string(),
            new_content: b"oops\n".to_vec(),
            is_binary: false,
        }]);
        let report = w.apply_patch(&patch).await.unwrap();
        assert_eq!(report.files_skipped, 1);
        assert_eq!(report.files_changed, 0);
        let after = w.read_file(&p).await.unwrap();
        assert_eq!(after.bytes, b"# Hello\n");
    }

    /// diff returns Create entries for newly added files and Modify entries for changed files.
    #[tokio::test]
    async fn diff_returns_changes_against_initial() {
        let w = ws();
        let p_new = WorkspacePath::new("new.txt").unwrap();
        let p_mod = WorkspacePath::new("README.md").unwrap();
        w.write_file(&p_new, b"hi\n").await.unwrap();
        w.write_file(&p_mod, b"# Bye\n").await.unwrap();
        let patch = w.diff(DiffBase::Initial).await.unwrap();
        assert_eq!(patch.summary.files_created, 1);
        assert_eq!(patch.summary.files_modified, 1);
    }
}
