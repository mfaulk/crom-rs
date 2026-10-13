//! Patch and artifact model.
//!
//! `PatchSet` is the typed unit of code change returned by the SDK. It is
//! deterministically renderable to unified-diff text and applies/serializes
//! cleanly. `CodeArtifacts` is the bag of all generated outputs from a run.

mod diff;

use serde::{Deserialize, Serialize};

use crate::workspace::{compute_hash, WorkspacePath};

pub use diff::{render_unified_diff, render_unified_diff_for_create, summarize_diff};

/// A typed file-level change. The variants cover create / modify / delete /
/// rename. Modify carries a unified diff so apply has the information to
/// faithfully rebuild the new content.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FilePatch {
    Create {
        path: WorkspacePath,
        content: Vec<u8>,
        is_binary: bool,
    },
    Modify {
        path: WorkspacePath,
        old_content_hash: Option<String>,
        new_content_hash: Option<String>,
        unified_diff: String,
        new_content: Vec<u8>,
        is_binary: bool,
    },
    Delete {
        path: WorkspacePath,
    },
    Rename {
        from: WorkspacePath,
        to: WorkspacePath,
    },
}

impl FilePatch {
    pub fn path(&self) -> &WorkspacePath {
        match self {
            Self::Create { path, .. } | Self::Modify { path, .. } | Self::Delete { path } => path,
            Self::Rename { to, .. } => to,
        }
    }

    pub fn is_binary(&self) -> bool {
        match self {
            Self::Create { is_binary, .. } | Self::Modify { is_binary, .. } => *is_binary,
            _ => false,
        }
    }
}

/// Aggregate summary of a [`PatchSet`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSummary {
    pub files_created: usize,
    pub files_modified: usize,
    pub files_deleted: usize,
    pub files_renamed: usize,
    pub added_lines: usize,
    pub removed_lines: usize,
}

/// A typed collection of file-level patches plus a deterministic unified-diff
/// rendering and a roll-up summary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PatchSet {
    pub files: Vec<FilePatch>,
    pub unified_diff: String,
    pub summary: PatchSummary,
}

impl PatchSet {
    pub fn from_files(files: Vec<FilePatch>) -> Self {
        let mut summary = PatchSummary::default();
        let mut diff = String::new();
        for f in &files {
            match f {
                FilePatch::Create {
                    path,
                    content,
                    is_binary,
                } => {
                    summary.files_created += 1;
                    if *is_binary {
                        diff.push_str(&format!(
                            "diff --binary {path} (created, {} bytes)\n",
                            content.len()
                        ));
                    } else {
                        let new_text = String::from_utf8_lossy(content);
                        let part = render_unified_diff_for_create(path.as_str(), &new_text);
                        let added = part
                            .lines()
                            .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                            .count();
                        summary.added_lines += added;
                        diff.push_str(&part);
                    }
                }
                FilePatch::Modify {
                    path,
                    unified_diff,
                    new_content,
                    is_binary,
                    ..
                } => {
                    summary.files_modified += 1;
                    if *is_binary {
                        diff.push_str(&format!(
                            "diff --binary {path} (modified, {} bytes)\n",
                            new_content.len()
                        ));
                    } else {
                        diff.push_str(unified_diff);
                        let (added, removed) = summarize_diff(unified_diff);
                        summary.added_lines += added;
                        summary.removed_lines += removed;
                    }
                }
                FilePatch::Delete { path } => {
                    summary.files_deleted += 1;
                    diff.push_str(&format!("diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n"));
                }
                FilePatch::Rename { from, to } => {
                    summary.files_renamed += 1;
                    diff.push_str(&format!(
                        "diff --git a/{from} b/{to}\nrename from {from}\nrename to {to}\n"
                    ));
                }
            }
        }
        Self {
            files,
            unified_diff: diff,
            summary,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Build a modify patch by computing a unified diff between two byte
    /// streams. If either side contains a NUL byte the patch is recorded as
    /// binary and the diff body is omitted (a one-line summary stays in the
    /// unified diff for visibility).
    pub fn build_modify(path: WorkspacePath, old: &[u8], new: &[u8]) -> FilePatch {
        let is_binary =
            crate::workspace::is_binary_bytes(old) || crate::workspace::is_binary_bytes(new);
        let old_hash = compute_hash(old);
        let new_hash = compute_hash(new);
        if is_binary {
            FilePatch::Modify {
                path,
                old_content_hash: Some(old_hash),
                new_content_hash: Some(new_hash),
                unified_diff: String::new(),
                new_content: new.to_vec(),
                is_binary: true,
            }
        } else {
            let old_s = String::from_utf8_lossy(old);
            let new_s = String::from_utf8_lossy(new);
            let unified = render_unified_diff(path.as_str(), &old_s, &new_s);
            FilePatch::Modify {
                path,
                old_content_hash: Some(old_hash),
                new_content_hash: Some(new_hash),
                unified_diff: unified,
                new_content: new.to_vec(),
                is_binary: false,
            }
        }
    }

    pub fn build_create(path: WorkspacePath, new: &[u8]) -> FilePatch {
        let is_binary = crate::workspace::is_binary_bytes(new);
        FilePatch::Create {
            path,
            content: new.to_vec(),
            is_binary,
        }
    }
}

/// A free-standing generated file. Returned through `CodeArtifacts.files`
/// even when no apply step took place. Hosts may persist these directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneratedFile {
    pub path: WorkspacePath,
    pub content: Vec<u8>,
    pub is_binary: bool,
    pub language: Option<String>,
    pub purpose: Option<String>,
}

/// A command the model suggests the host run. `safe_to_run` reflects whether
/// the SDK believes the command can run without prompting (validation
/// commands fall here); hosts decide their own gating.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSuggestion {
    pub command: String,
    pub reason: Option<String>,
    pub safe_to_run: bool,
}

/// All code-shaped artifacts a run produces. `notes` carries free-form
/// non-code annotations (provider responses, deviation reasons, etc.) so
/// applications never need to parse assistant text to recover them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeArtifacts {
    pub files: Vec<GeneratedFile>,
    pub patches: Vec<PatchSet>,
    pub commands: Vec<CommandSuggestion>,
    pub notes: Vec<String>,
}

impl CodeArtifacts {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn add_file(&mut self, file: GeneratedFile) {
        self.files.push(file);
    }

    pub fn add_patch(&mut self, patch: PatchSet) {
        self.patches.push(patch);
    }

    pub fn add_command(&mut self, suggestion: CommandSuggestion) {
        self.commands.push(suggestion);
    }

    pub fn add_note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PatchSet::from_files counts created and modified files in the summary.
    #[test]
    fn summary_counts_created_and_modified() {
        let path_a = WorkspacePath::new("a.txt").unwrap();
        let path_b = WorkspacePath::new("b.txt").unwrap();
        let create = PatchSet::build_create(path_a.clone(), b"hello\n");
        let modify = PatchSet::build_modify(path_b.clone(), b"old\n", b"new\n");
        let set = PatchSet::from_files(vec![create, modify]);
        assert_eq!(set.summary.files_created, 1);
        assert_eq!(set.summary.files_modified, 1);
        assert!(set.summary.added_lines >= 1);
    }

    /// PatchSet round-trips through JSON without loss for non-binary files.
    #[test]
    fn patch_set_json_roundtrip() {
        let path = WorkspacePath::new("a.txt").unwrap();
        let p = PatchSet::from_files(vec![PatchSet::build_create(path, b"hi\n")]);
        let s = serde_json::to_string(&p).unwrap();
        let back: PatchSet = serde_json::from_str(&s).unwrap();
        assert_eq!(back.summary.files_created, p.summary.files_created);
        assert_eq!(back.files.len(), p.files.len());
    }

    /// Binary modify patches are recorded as binary and skip the unified diff body.
    #[test]
    fn binary_modify_records_as_binary() {
        let path = WorkspacePath::new("a.bin").unwrap();
        let old = vec![0u8, 1, 2];
        let new = vec![0u8, 3, 4];
        match PatchSet::build_modify(path, &old, &new) {
            FilePatch::Modify {
                is_binary,
                unified_diff,
                ..
            } => {
                assert!(is_binary);
                assert!(unified_diff.is_empty());
            }
            _ => panic!("expected Modify"),
        }
    }

    /// Unified diff is deterministic for the same input.
    #[test]
    fn unified_diff_is_deterministic() {
        let path = WorkspacePath::new("a.txt").unwrap();
        let a = PatchSet::build_modify(path.clone(), b"a\nb\n", b"a\nb\nc\n");
        let b = PatchSet::build_modify(path, b"a\nb\n", b"a\nb\nc\n");
        match (a, b) {
            (
                FilePatch::Modify {
                    unified_diff: x, ..
                },
                FilePatch::Modify {
                    unified_diff: y, ..
                },
            ) => {
                assert_eq!(x, y);
            }
            _ => panic!(),
        }
    }
}
