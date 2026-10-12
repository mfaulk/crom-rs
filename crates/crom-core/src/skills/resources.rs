//! Safe Skill-resource loading.
//!
//! Resources are arbitrary files inside a Skill directory. They may be
//! Markdown, scripts, templates, JSON data, or binary. The loader:
//! - Resolves resource paths *strictly inside* the Skill root.
//! - Rejects path traversal and symlink escapes.
//! - Caps individual reads and the cumulative size per Skill.
//! - Marks binary files but never returns binary content as a string.
//! - **Never executes** anything, including scripts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::definition::{resource_kind_for, SkillResourceKind};
use super::errors::SkillError;

/// Maximum bytes returned per textual resource preview.
pub const MAX_RESOURCE_PREVIEW_BYTES: usize = 8 * 1024;

/// Compact summary of a Skill resource — suitable for telemetry, session
/// JSONL, and the Skill tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResourceSummary {
    /// Skill-relative path (forward-slash separated for portability).
    pub path: String,
    pub kind: SkillResourceKind,
    pub size_bytes: u64,
    pub loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(default)]
    pub truncated: bool,
}

/// Load (or summarize) a Skill resource.
///
/// `skill_root` must be an *already-canonicalized* path. `requested` is a
/// Skill-relative path as written in the SKILL.md frontmatter. The loader
/// rejects:
/// - absolute paths,
/// - any segment equal to `..`,
/// - any path that resolves outside `skill_root` once symlinks are
///   followed.
///
/// The returned summary's `preview` is `Some` for textual files within the
/// per-skill cap, and `None` (with `loaded = false`) for binary files or
/// files exceeding the cap.
pub fn load_resource_summary(
    skill_root: &Path,
    requested: &str,
    skill_name: &str,
    remaining_budget: &mut usize,
) -> Result<SkillResourceSummary, SkillError> {
    // Preserve the underlying error: a missing file should report
    // `Io(NotFound)`, not `ResourceEscape`. The `resolve_resource_path`
    // helper is responsible for distinguishing the two.
    let resolved = match resolve_resource_path(skill_root, requested) {
        Ok(p) => p,
        Err(SkillError::ResourceEscape { path, .. }) => {
            return Err(SkillError::ResourceEscape {
                name: skill_name.to_string(),
                path,
            })
        }
        Err(other) => return Err(other),
    };
    let metadata = std::fs::metadata(&resolved).map_err(|source| SkillError::Io {
        path: resolved.clone(),
        source,
    })?;
    let size_bytes = metadata.len();
    let kind = resource_kind_for(requested);
    if !metadata.is_file() {
        return Ok(SkillResourceSummary {
            path: normalize_path_str(requested),
            kind,
            size_bytes,
            loaded: false,
            preview: None,
            truncated: false,
        });
    }

    // Even if we won't load the content, charge an entry-level cost.
    let max_preview = (*remaining_budget).min(MAX_RESOURCE_PREVIEW_BYTES);
    if size_bytes > max_preview as u64 {
        return Ok(SkillResourceSummary {
            path: normalize_path_str(requested),
            kind,
            size_bytes,
            loaded: false,
            preview: None,
            truncated: true,
        });
    }
    let bytes = std::fs::read(&resolved).map_err(|source| SkillError::Io {
        path: resolved.clone(),
        source,
    })?;
    let to_take = bytes.len().min(max_preview);
    let truncated = bytes.len() > to_take;
    let prefix = &bytes[..to_take];
    if !is_likely_text(prefix) {
        return Ok(SkillResourceSummary {
            path: normalize_path_str(requested),
            kind: SkillResourceKind::Binary,
            size_bytes,
            loaded: false,
            preview: None,
            truncated: false,
        });
    }
    let text = String::from_utf8_lossy(prefix).into_owned();
    *remaining_budget = remaining_budget.saturating_sub(to_take);
    Ok(SkillResourceSummary {
        path: normalize_path_str(requested),
        kind,
        size_bytes,
        loaded: true,
        preview: Some(text),
        truncated,
    })
}

/// Resolve a skill-relative resource path to an absolute path inside the
/// Skill root. Rejects absolute paths, `..` segments, and symlink escapes.
pub fn resolve_resource_path(skill_root: &Path, requested: &str) -> Result<PathBuf, SkillError> {
    if requested.is_empty() {
        return Err(SkillError::Other("empty resource path".to_string()));
    }
    let candidate = Path::new(requested);
    if candidate.is_absolute() {
        return Err(SkillError::ResourceEscape {
            name: String::new(),
            path: requested.to_string(),
        });
    }
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(_) | std::path::Component::CurDir => {}
            _ => {
                return Err(SkillError::ResourceEscape {
                    name: String::new(),
                    path: requested.to_string(),
                })
            }
        }
    }
    let joined = skill_root.join(candidate);
    let canonical = joined.canonicalize().map_err(|source| SkillError::Io {
        path: joined.clone(),
        source,
    })?;
    let canonical_root = skill_root.canonicalize().map_err(|source| SkillError::Io {
        path: skill_root.to_path_buf(),
        source,
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err(SkillError::ResourceEscape {
            name: String::new(),
            path: requested.to_string(),
        });
    }
    Ok(canonical)
}

fn normalize_path_str(s: &str) -> String {
    s.replace('\\', "/")
}

/// Heuristic: a buffer is "text" if it contains no NUL bytes and the
/// fraction of non-printable, non-whitespace bytes is small.
pub fn is_likely_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    if bytes.contains(&0u8) {
        return false;
    }
    let suspicious = bytes
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\n' && b != b'\r' && b != b'\t')
        .count();
    (suspicious * 100) / bytes.len() < 5
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // A simple text resource loads with a preview.
    fn loads_text_resource() {
        let tmp = TempDir::new().unwrap();
        let skill_root = tmp.path();
        std::fs::create_dir_all(skill_root.join("templates")).unwrap();
        std::fs::write(skill_root.join("templates/pr.md"), "hello\n").unwrap();
        let mut budget = 1024;
        let s = load_resource_summary(skill_root, "templates/pr.md", "x", &mut budget).unwrap();
        assert_eq!(s.path, "templates/pr.md");
        assert_eq!(s.kind, SkillResourceKind::Markdown);
        assert!(s.loaded);
        assert_eq!(s.preview.as_deref(), Some("hello\n"));
        assert!(budget < 1024);
    }

    #[test]
    // Path traversal is rejected.
    fn path_traversal_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut budget = 1024;
        let err = load_resource_summary(tmp.path(), "../etc/passwd", "x", &mut budget);
        assert!(matches!(err, Err(SkillError::ResourceEscape { .. })));
    }

    #[test]
    // Absolute paths are rejected.
    fn absolute_path_rejected() {
        let tmp = TempDir::new().unwrap();
        let mut budget = 1024;
        let err = load_resource_summary(tmp.path(), "/etc/passwd", "x", &mut budget);
        assert!(matches!(err, Err(SkillError::ResourceEscape { .. })));
    }

    #[test]
    // A missing file surfaces as an Io error, *not* as ResourceEscape.
    // Authors typo'ing a resource path should not be told they tried a
    // path-traversal.
    fn missing_file_reports_io_not_escape() {
        let tmp = TempDir::new().unwrap();
        let mut budget = 1024;
        let err = load_resource_summary(tmp.path(), "no-such-file.md", "x", &mut budget);
        assert!(
            matches!(err, Err(SkillError::Io { .. })),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    // Binary content is summarized but not returned.
    fn binary_not_returned() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("blob.bin");
        std::fs::write(&path, [0u8, 1, 2, 3, 4, 5]).unwrap();
        let mut budget = 1024;
        let s = load_resource_summary(tmp.path(), "blob.bin", "x", &mut budget).unwrap();
        assert!(!s.loaded);
        assert!(s.preview.is_none());
        assert_eq!(s.kind, SkillResourceKind::Binary);
    }

    #[test]
    // Files larger than the per-skill budget are summarized as truncated.
    fn over_budget_resource_marked_truncated() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("big.txt");
        std::fs::write(&path, vec![b'x'; 200]).unwrap();
        let mut budget = 50;
        let s = load_resource_summary(tmp.path(), "big.txt", "x", &mut budget).unwrap();
        assert!(!s.loaded);
        assert!(s.truncated);
    }

    #[cfg(unix)]
    #[test]
    // Symlinks pointing outside the Skill root are rejected.
    fn symlink_escape_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        let target = outside.path().join("evil.txt");
        std::fs::write(&target, "x").unwrap();
        let inside = TempDir::new().unwrap();
        symlink(&target, inside.path().join("link.txt")).unwrap();
        let mut budget = 1024;
        let err = load_resource_summary(inside.path(), "link.txt", "x", &mut budget);
        assert!(matches!(err, Err(SkillError::ResourceEscape { .. })));
    }
}
