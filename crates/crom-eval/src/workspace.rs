//! Agent workspaces are allocated in a temp root disjoint from the run
//! artifacts and the task source (M17.2, #286), so nothing graded — task.toml,
//! normalized-task.json, hidden/, the reference patch, earlier results — sits
//! inside the workspace or beside it. After the agent exits the workspace is
//! moved into the artifact bundle as the verification copy, and only then is
//! the hidden fixture overlaid. Absolute-path reads by agent Bash are not
//! blocked here; that needs the #253 OS sandbox.

use std::io;
use std::path::{Path, PathBuf};

/// Default parent for agent workspaces, under the system temp dir (`TMPDIR`).
pub fn default_root() -> PathBuf {
    std::env::temp_dir().join("crom-eval-ws")
}

/// A live agent workspace: `<root>/ws-<random>/`, mode 0700 on unix.
/// Removed on drop unless moved by [`AgentWorkspace::into_verification_copy`].
#[derive(Debug)]
pub struct AgentWorkspace {
    dir: tempfile::TempDir,
}

impl AgentWorkspace {
    /// Allocate a workspace under `root`. Fails if `root` overlaps (is inside,
    /// or contains) any of `protected` — the runs dir and task source dir.
    pub fn create(root: &Path, protected: &[&Path]) -> io::Result<Self> {
        std::fs::create_dir_all(root)?;
        ensure_disjoint(root, protected)?;
        let dir = tempfile::Builder::new().prefix("ws-").tempdir_in(root)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Move the workspace (including `.git` and build output) to `dst`,
    /// replacing anything already there. Falls back to copy + delete when a
    /// rename crosses filesystems.
    pub fn into_verification_copy(self, dst: &Path) -> io::Result<PathBuf> {
        if dst.exists() {
            std::fs::remove_dir_all(dst)?;
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let src = self.dir.path();
        if std::fs::rename(src, dst).is_err() {
            copy_tree(src, dst)?;
            // `self.dir` drops here and removes the source.
        }
        Ok(dst.to_path_buf())
    }
}

/// Error unless `root` and every existing `protected` path are disjoint
/// (neither is an ancestor of the other) after canonicalization.
pub fn ensure_disjoint(root: &Path, protected: &[&Path]) -> io::Result<()> {
    let root = root.canonicalize()?;
    for p in protected {
        let Ok(p) = p.canonicalize() else { continue };
        if root.starts_with(&p) || p.starts_with(&root) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "agent workspace root {} overlaps grader material at {}",
                    root.display(),
                    p.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Exact recursive copy: keeps `.git`/`target` and copies symlinks as links
/// (never following them out of the tree).
fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            copy_symlink(&from, &to)?;
        } else if ft.is_dir() {
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(from: &Path, to: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
}

#[cfg(not(unix))]
fn copy_symlink(_from: &Path, _to: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace is created under the root and is removed when dropped.
    #[test]
    fn creates_under_root_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = AgentWorkspace::create(&tmp.path().join("ws"), &[]).unwrap();
        let p = ws.path().to_path_buf();
        assert!(p.starts_with(tmp.path().join("ws")));
        assert!(p.is_dir());
        drop(ws);
        assert!(!p.exists());
    }

    /// A root inside the runs dir (or containing the task source) is rejected.
    #[test]
    fn rejects_root_overlapping_grader_material() {
        let tmp = tempfile::tempdir().unwrap();
        let runs = tmp.path().join("runs");
        let task = tmp.path().join("tasks/t1");
        std::fs::create_dir_all(&runs).unwrap();
        std::fs::create_dir_all(&task).unwrap();
        let protected = [runs.as_path(), task.as_path()];
        assert!(AgentWorkspace::create(&runs.join("ws"), &protected).is_err());
        assert!(AgentWorkspace::create(tmp.path(), &protected).is_err());
        assert!(AgentWorkspace::create(&tmp.path().join("ws"), &protected).is_ok());
    }

    /// Two workspaces under the same root are distinct, so one never sees another's path.
    #[test]
    fn workspaces_are_unique() {
        let tmp = tempfile::tempdir().unwrap();
        let a = AgentWorkspace::create(tmp.path(), &[]).unwrap();
        let b = AgentWorkspace::create(tmp.path(), &[]).unwrap();
        assert_ne!(a.path(), b.path());
    }

    /// The exact-copy fallback keeps `.git` and `target` and copies symlinks as links.
    #[test]
    fn copy_tree_is_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::create_dir_all(src.join("target")).unwrap();
        std::fs::write(src.join(".git/HEAD"), "ref").unwrap();
        std::fs::write(src.join("target/x"), "bin").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hostname", src.join("link")).unwrap();
        let dst = tmp.path().join("dst");
        copy_tree(&src, &dst).unwrap();
        assert!(dst.join(".git/HEAD").exists());
        assert!(dst.join("target/x").exists());
        #[cfg(unix)]
        assert!(dst
            .join("link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// Moving to the verification copy relocates contents and removes the agent workspace.
    #[test]
    fn into_verification_copy_moves_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = AgentWorkspace::create(&tmp.path().join("ws"), &[]).unwrap();
        std::fs::write(ws.path().join("a.txt"), "hi").unwrap();
        let src = ws.path().to_path_buf();
        let dst = tmp.path().join("art/workspace");
        std::fs::create_dir_all(&dst).unwrap();
        std::fs::write(dst.join("stale"), "x").unwrap();
        let out = ws.into_verification_copy(&dst).unwrap();
        assert_eq!(std::fs::read_to_string(out.join("a.txt")).unwrap(), "hi");
        assert!(!out.join("stale").exists());
        assert!(!src.exists());
    }
}
