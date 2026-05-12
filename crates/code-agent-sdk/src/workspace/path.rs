use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use crate::error::AgentSdkError;

/// A workspace-relative path. Always normalized to forward-slash separators,
/// rejects absolute paths and `..` traversal. Empty paths are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspacePath(String);

impl WorkspacePath {
    /// Create a new workspace path. Rejects:
    /// - empty strings,
    /// - absolute paths (starting with `/` or having a Windows prefix),
    /// - paths containing a `..` component,
    /// - NUL bytes.
    pub fn new(s: impl Into<String>) -> Result<Self, AgentSdkError> {
        let raw: String = s.into();
        if raw.is_empty() {
            return Err(AgentSdkError::invalid_request(
                "workspace.path_empty",
                "workspace path is empty",
            ));
        }
        if raw.contains('\0') {
            return Err(AgentSdkError::invalid_request(
                "workspace.path_invalid",
                "workspace path contains NUL byte",
            ));
        }

        // Reject absolute paths early (cheap check before Path component walk).
        if raw.starts_with('/') || raw.starts_with('\\') {
            return Err(AgentSdkError::invalid_request(
                "workspace.path_absolute",
                format!("workspace path must be relative: {raw}"),
            ));
        }

        let p = Path::new(&raw);
        let mut normalized = String::new();
        for comp in p.components() {
            match comp {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(AgentSdkError::invalid_request(
                        "workspace.path_absolute",
                        format!("workspace path must be relative: {raw}"),
                    ));
                }
                Component::ParentDir => {
                    return Err(AgentSdkError::invalid_request(
                        "workspace.path_outside_root",
                        format!("workspace path traversal not allowed: {raw}"),
                    ));
                }
                Component::CurDir => {}
                Component::Normal(seg) => {
                    if !normalized.is_empty() {
                        normalized.push('/');
                    }
                    normalized.push_str(&seg.to_string_lossy());
                }
            }
        }
        if normalized.is_empty() {
            return Err(AgentSdkError::invalid_request(
                "workspace.path_empty",
                "workspace path is empty after normalization",
            ));
        }
        Ok(Self(normalized))
    }

    /// Construct a `WorkspacePath` without validation. **Internal use only.**
    pub(crate) fn from_validated(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Convert to a `PathBuf` rooted at `root`. The path is joined component
    /// by component to avoid OS-specific separator handling.
    pub fn join_under(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        for seg in self.0.split('/') {
            p.push(seg);
        }
        p
    }

    /// File name (final path segment).
    pub fn file_name(&self) -> Option<&str> {
        self.0.rsplit('/').next()
    }

    /// Parent path, if any.
    pub fn parent(&self) -> Option<WorkspacePath> {
        let idx = self.0.rfind('/')?;
        if idx == 0 {
            None
        } else {
            Some(Self(self.0[..idx].to_string()))
        }
    }
}

impl fmt::Display for WorkspacePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<&str> for WorkspacePath {
    type Error = AgentSdkError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for WorkspacePath {
    type Error = AgentSdkError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Absolute paths are rejected.
    #[test]
    fn rejects_absolute_paths() {
        assert!(WorkspacePath::new("/etc/passwd").is_err());
        assert!(WorkspacePath::new("/a").is_err());
    }

    /// Empty paths are rejected.
    #[test]
    fn rejects_empty_paths() {
        assert!(WorkspacePath::new("").is_err());
    }

    /// Paths with `..` traversal are rejected.
    #[test]
    fn rejects_traversal() {
        assert!(WorkspacePath::new("../etc/passwd").is_err());
        assert!(WorkspacePath::new("a/../b").is_err());
        assert!(WorkspacePath::new("a/b/..").is_err());
    }

    /// Relative paths normalize forward slashes and pass.
    #[test]
    fn accepts_relative_paths() {
        assert_eq!(
            WorkspacePath::new("src/lib.rs").unwrap().as_str(),
            "src/lib.rs"
        );
        assert_eq!(WorkspacePath::new("a.txt").unwrap().as_str(), "a.txt");
        assert_eq!(WorkspacePath::new("./a.txt").unwrap().as_str(), "a.txt");
    }

    /// NUL bytes are rejected.
    #[test]
    fn rejects_nul_bytes() {
        assert!(WorkspacePath::new("a\0b").is_err());
    }

    /// Parent/file_name navigation behaves as expected.
    #[test]
    fn parent_and_file_name() {
        let p = WorkspacePath::new("src/foo/bar.rs").unwrap();
        assert_eq!(p.file_name(), Some("bar.rs"));
        assert_eq!(p.parent().unwrap().as_str(), "src/foo");
        let p2 = WorkspacePath::new("a.txt").unwrap();
        assert_eq!(p2.parent(), None);
    }
}
