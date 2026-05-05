use std::path::{Component, Path, PathBuf};

use code_agent_core::ToolError;

/// Resolve `requested` relative to `cwd_root` and reject anything that escapes
/// `cwd_root` via `..`, absolute paths outside the root, or symlinks pointing
/// outside the root.
///
/// `must_exist`:
/// - When `true`, the resolved path must already exist and is canonicalized
///   directly. Symlink targets are checked against the canonical root.
/// - When `false`, the parent directory must exist (or equal the root). The
///   filename component is appended back without canonicalization. This
///   supports tools like Write that create new files.
pub fn resolve_within(
    cwd_root: &Path,
    requested: &str,
    must_exist: bool,
) -> Result<PathBuf, ToolError> {
    if requested.is_empty() {
        return Err(ToolError::InvalidInput("file_path is empty".into()));
    }
    let canon_root = canonicalize_root(cwd_root)?;

    let mut joined = PathBuf::from(&canon_root);
    let req = Path::new(requested);
    for c in req.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                // For absolute paths, restart from root.
                joined = canon_root.clone();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !joined.pop() {
                    return Err(ToolError::PathEscape(requested.to_string()));
                }
                if !joined.starts_with(&canon_root) {
                    return Err(ToolError::PathEscape(requested.to_string()));
                }
            }
            Component::Normal(seg) => {
                joined.push(seg);
            }
        }
    }

    if must_exist {
        let canon = joined
            .canonicalize()
            .map_err(|_| ToolError::NotFound(requested.to_string()))?;
        if !canon.starts_with(&canon_root) {
            return Err(ToolError::PathEscape(requested.to_string()));
        }
        Ok(canon)
    } else {
        // Walk back to the closest ancestor that already exists on disk, then
        // canonicalize it and ensure it stays inside the workspace.
        //
        // We use `symlink_metadata` instead of `Path::exists()` because
        // `exists()` follows symlinks: a dangling symlink in the workspace
        // reports `false`, which would let the loop pop past the symlink and
        // happily resolve the parent — at which point a subsequent open of
        // the returned path would follow the symlink out of the sandbox.
        let mut existing = joined.clone();
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        while existing.symlink_metadata().is_err() {
            let name = match existing.file_name() {
                Some(n) => n.to_os_string(),
                None => return Err(ToolError::InvalidInput(requested.to_string())),
            };
            tail.push(name);
            if !existing.pop() {
                return Err(ToolError::PathEscape(requested.to_string()));
            }
        }
        // Refuse if the closest existing component is a symlink — even one
        // that resolves inside the workspace — because following it for a
        // write would step outside the strict cwd-rooted hierarchy we expose
        // to the agent.
        let meta = existing.symlink_metadata().map_err(ToolError::Io)?;
        if meta.file_type().is_symlink() {
            return Err(ToolError::PathEscape(requested.to_string()));
        }
        let canon_existing = existing.canonicalize().map_err(ToolError::Io)?;
        if !canon_existing.starts_with(&canon_root) {
            return Err(ToolError::PathEscape(requested.to_string()));
        }
        let mut out = canon_existing;
        for seg in tail.into_iter().rev() {
            out.push(seg);
        }
        Ok(out)
    }
}

pub fn canonicalize_root(root: &Path) -> Result<PathBuf, ToolError> {
    root.canonicalize().map_err(ToolError::Io)
}

/// Convert an absolute resolved path to a path relative to the workspace root.
pub fn relative_to(root: &Path, p: &Path) -> PathBuf {
    let canon_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    p.strip_prefix(&canon_root).unwrap_or(p).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn rejects_parent_traversal() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let err = resolve_within(root, "../etc/passwd", false).unwrap_err();
        assert!(matches!(err, ToolError::PathEscape(_)));
    }

    #[test]
    fn rejects_absolute_outside() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        // An absolute path resets to the workspace root, but if it canonicalizes
        // to something inside, we still allow it. The key concern is `..`.
        // Simulate `/etc/passwd` which would resolve to root + "etc/passwd".
        // That path doesn't exist, so for must_exist it should be NotFound.
        let err = resolve_within(root, "/etc/passwd", true).unwrap_err();
        assert!(matches!(err, ToolError::NotFound(_)));
    }

    #[test]
    fn allows_simple_relative() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.txt"), "hi").unwrap();
        let p = resolve_within(root, "a.txt", true).unwrap();
        assert!(p.ends_with("a.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let outer = TempDir::new().unwrap();
        let root_dir = TempDir::new().unwrap();
        let root = root_dir.path();
        let target = outer.path().join("secret.txt");
        std::fs::write(&target, "secret").unwrap();
        let link = root.join("link.txt");
        symlink(&target, &link).unwrap();

        let err = resolve_within(root, "link.txt", true).unwrap_err();
        assert!(matches!(err, ToolError::PathEscape(_)));
    }
}
