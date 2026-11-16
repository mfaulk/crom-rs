//! Fixture copying, isolated workspace setup, and patch generation.
//!
//! Each task runs in its own workspace directory. The fixture repository is
//! copied in, a fresh git repository is initialised so a final diff/patch can
//! be generated, and a baseline commit captures the pristine state. After the
//! agent runs, [`generate_patch`] diffs the workspace against that baseline.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::task::{EvalTask, FixtureKind};

/// Summary of the patch produced by an agent run.
#[derive(Debug, Clone, Default)]
pub struct PatchInfo {
    /// Full unified diff (git diff against the baseline commit).
    pub unified_diff: String,
    /// A `git diff --stat`-style summary.
    pub stat: String,
    /// Files added, modified, or deleted relative to the baseline.
    pub changed_files: Vec<String>,
    /// Files that were deleted relative to the baseline.
    pub deleted_files: Vec<String>,
    /// Total inserted lines across all files.
    pub insertions: usize,
    /// Total deleted lines across all files.
    pub deletions: usize,
}

impl PatchInfo {
    pub fn is_empty(&self) -> bool {
        self.changed_files.is_empty()
    }
}

/// Errors that arise while preparing workspaces or generating patches.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture io error: {0}")]
    Io(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("fixture path not found: {0}")]
    FixtureNotFound(String),
}

/// Prepare an isolated workspace for a task at `workspace_dir`.
///
/// Copies the fixture (when `kind = copy_dir`), then initialises a fresh git
/// repository with a baseline commit. The directory is created if missing.
pub fn prepare_workspace(task: &EvalTask, workspace_dir: &Path) -> Result<(), FixtureError> {
    prepare_workspace_with_cache(task, workspace_dir, &fixture_cache_dir())
}

/// [`prepare_workspace`] with an explicit cache root for `git` fixtures.
pub fn prepare_workspace_with_cache(
    task: &EvalTask,
    workspace_dir: &Path,
    cache_root: &Path,
) -> Result<(), FixtureError> {
    std::fs::create_dir_all(workspace_dir)
        .map_err(|e| FixtureError::Io(format!("create {}: {e}", workspace_dir.display())))?;

    match task.fixture.kind {
        FixtureKind::CopyDir => {
            let rel = task.fixture.path.as_deref().unwrap_or("repo");
            let src = task.dir.join(rel);
            if !src.exists() {
                return Err(FixtureError::FixtureNotFound(src.display().to_string()));
            }
            copy_dir_recursive(&src, workspace_dir)?;
        }
        FixtureKind::Git => {
            let checkout = fetch_git_fixture(&task.fixture, cache_root)?;
            let rel = task.fixture.path.as_deref().unwrap_or(".");
            let src = checkout.join(rel);
            if !src.is_dir() {
                return Err(FixtureError::FixtureNotFound(format!(
                    "{rel} at {}",
                    task.fixture.rev.as_deref().unwrap_or("?")
                )));
            }
            copy_dir_recursive(&src, workspace_dir)?;
        }
        FixtureKind::Empty => {}
    }

    git_init_baseline(workspace_dir)?;
    Ok(())
}

/// Root of the `git` fixture cache: `$CROM_EVAL_FIXTURE_CACHE`, else
/// `$XDG_CACHE_HOME/crom/eval-fixtures`, else `~/.cache/crom/eval-fixtures`.
pub fn fixture_cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("CROM_EVAL_FIXTURE_CACHE") {
        return PathBuf::from(p);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("crom").join("eval-fixtures")
}

/// Whether `rev` is a full 40-hex commit SHA (the only pin a git fixture accepts).
pub fn is_full_sha(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Checkout of a `git` fixture at its pinned commit, fetched into
/// `cache_root/<rev>` on first use. The cache entry is only published after
/// `HEAD` is verified to equal `rev`, so a cached entry is always complete and
/// later runs need no network.
pub fn fetch_git_fixture(
    spec: &crate::task::FixtureSpec,
    cache_root: &Path,
) -> Result<PathBuf, FixtureError> {
    let repo = spec
        .repo
        .as_deref()
        .ok_or_else(|| FixtureError::Git("git fixture missing `repo`".into()))?;
    let rev = spec
        .rev
        .as_deref()
        .ok_or_else(|| FixtureError::Git("git fixture missing `rev`".into()))?;
    if !is_full_sha(rev) {
        return Err(FixtureError::Git(format!(
            "git fixture `rev` must be a full 40-hex commit SHA, got {rev:?}"
        )));
    }
    let rev = rev.to_ascii_lowercase();
    let dest = cache_root.join(&rev);
    if dest.join(".git").is_dir() {
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root)
        .map_err(|e| FixtureError::Io(format!("create {}: {e}", cache_root.display())))?;
    let staging = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(cache_root)
        .map_err(|e| FixtureError::Io(format!("staging dir: {e}")))?;
    let dir = staging.path();
    let git = |args: &[&str]| -> Result<String, FixtureError> {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .map_err(|e| FixtureError::Git(format!("spawn git: {e}")))?;
        if !out.status.success() {
            return Err(FixtureError::Git(format!(
                "git {} failed for {repo}@{rev}: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    git(&["init", "-q"])?;
    git(&["fetch", "-q", "--depth", "1", repo, &rev])?;
    git(&[
        "-c",
        "advice.detachedHead=false",
        "checkout",
        "-q",
        "FETCH_HEAD",
    ])?;
    let head = git(&["rev-parse", "HEAD"])?;
    if head != rev {
        return Err(FixtureError::Git(format!(
            "fetched {head} but the fixture is pinned to {rev}"
        )));
    }
    let staged = staging.keep();
    if let Err(e) = std::fs::rename(&staged, &dest) {
        let _ = std::fs::remove_dir_all(&staged);
        // A concurrent fetch may have published the same commit first.
        if !dest.join(".git").is_dir() {
            return Err(FixtureError::Io(format!("publish {}: {e}", dest.display())));
        }
    }
    Ok(dest)
}

/// Recursively copy the contents of `src` into `dst`. Skips `.git`, `target`,
/// and other build artifacts so fixtures stay small and reproducible.
pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), FixtureError> {
    std::fs::create_dir_all(dst)
        .map_err(|e| FixtureError::Io(format!("create {}: {e}", dst.display())))?;
    let entries = std::fs::read_dir(src)
        .map_err(|e| FixtureError::Io(format!("read {}: {e}", src.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| FixtureError::Io(e.to_string()))?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if matches!(name_str.as_ref(), ".git" | "target" | "node_modules") {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let file_type = entry
            .file_type()
            .map_err(|e| FixtureError::Io(e.to_string()))?;
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| FixtureError::Io(format!("copy {}: {e}", from.display())))?;
        }
    }
    Ok(())
}

/// Run a git command in `dir`, returning stdout on success.
fn git(dir: &Path, args: &[&str]) -> Result<String, FixtureError> {
    let output = Command::new("git")
        // Local identity, gpg-off, and safe.directory so git operations never
        // depend on (or fail against) the host's global git config — important
        // in CI containers where the directory owner may differ from the user.
        .args(["-c", "user.email=eval@crom.local"])
        .args(["-c", "user.name=crom-eval"])
        .args(["-c", "commit.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main"])
        .args(["-c", "safe.directory=*"])
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| FixtureError::Git(format!("spawn git {args:?}: {e}")))?;
    if !output.status.success() {
        return Err(FixtureError::Git(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Name of the tag that pins the pristine baseline commit.
const BASELINE_TAG: &str = "eval-baseline";

/// Build-artifact paths excluded from the baseline and the final patch.
///
/// Oracle commands (`cargo test`/`clippy`) regenerate `target/` and `Cargo.lock`
/// inside the workspace during the run. Without excluding them, `git add -A`
/// would sweep them into `final.patch` and inflate the changed-file count that
/// patch-quality scoring reads. They are never the thing under evaluation.
const PATCH_EXCLUDES: &[&str] = &["/target/", "/Cargo.lock", "/**/target/"];

/// Initialise a fresh git repo and create a baseline commit of the fixture.
fn git_init_baseline(dir: &Path) -> Result<(), FixtureError> {
    git(dir, &["init", "-q"])?;
    // Exclude build artifacts via `.git/info/exclude` (untracked, so it never
    // shows up in the diff) before the first `add`.
    write_git_exclude(dir)?;
    git(dir, &["add", "-A"])?;
    // `--allow-empty` so empty fixtures still get a baseline to diff against.
    git(
        dir,
        &["commit", "-q", "--allow-empty", "-m", "eval baseline"],
    )?;
    // Pin the baseline so the final diff is anchored even if the agent makes
    // its own commits, resets, or amends.
    git(dir, &["tag", BASELINE_TAG])?;
    Ok(())
}

/// Write `.git/info/exclude` so build artifacts never enter the index.
fn write_git_exclude(dir: &Path) -> Result<(), FixtureError> {
    let exclude_path = dir.join(".git").join("info").join("exclude");
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| FixtureError::Io(format!("create {}: {e}", parent.display())))?;
    }
    let body = format!("{}\n", PATCH_EXCLUDES.join("\n"));
    std::fs::write(&exclude_path, body)
        .map_err(|e| FixtureError::Io(format!("write {}: {e}", exclude_path.display())))
}

/// Generate the patch for a workspace by diffing against the baseline commit.
///
/// Stages all changes (so new and deleted files are captured) and computes a
/// unified diff plus a summary. Staging mutates the index only; the working
/// tree is left intact for inspection.
pub fn generate_patch(workspace_dir: &Path) -> Result<PatchInfo, FixtureError> {
    // Stage everything (so new and deleted files are captured), then diff the
    // index against the pinned baseline tag. Diffing against the tag rather than
    // HEAD keeps the patch anchored to the pristine fixture even if the agent
    // created its own commits.
    git(workspace_dir, &["add", "-A"])?;
    let base = &["diff", "--cached", BASELINE_TAG][..];
    let unified_diff = git(workspace_dir, base)?;
    let stat = git(workspace_dir, &["diff", "--cached", BASELINE_TAG, "--stat"])?;
    let numstat = git(
        workspace_dir,
        &["diff", "--cached", BASELINE_TAG, "--numstat"],
    )?;
    let name_status = git(
        workspace_dir,
        &["diff", "--cached", BASELINE_TAG, "--name-status"],
    )?;

    let mut info = PatchInfo {
        unified_diff,
        stat,
        ..Default::default()
    };

    for line in numstat.lines() {
        let mut cols = line.split('\t');
        let added = cols.next().unwrap_or("0");
        let removed = cols.next().unwrap_or("0");
        info.insertions += added.parse::<usize>().unwrap_or(0);
        info.deletions += removed.parse::<usize>().unwrap_or(0);
    }

    for line in name_status.lines() {
        let mut cols = line.split('\t');
        let status = cols.next().unwrap_or("");
        let path = cols.next().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }
        info.changed_files.push(path.clone());
        if status.starts_with('D') {
            info.deleted_files.push(path);
        }
    }

    Ok(info)
}

/// Read a file from the workspace, returning `None` if it does not exist.
pub fn read_workspace_file(workspace_dir: &Path, rel: &str) -> Option<String> {
    let path = workspace_dir.join(rel);
    std::fs::read_to_string(path).ok()
}

/// Whether a relative path looks like a test file (used by patch-quality
/// heuristics that penalise deleting tests).
pub fn looks_like_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    p.contains("/tests/")
        || p.starts_with("tests/")
        || p.ends_with("_test.rs")
        || p.ends_with("test.rs")
}

/// Best-effort resolution of the eval crate's tasks directory. Used by the CLI
/// and tests when no explicit path is supplied.
pub fn default_tasks_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/crom-eval at compile time; the tasks live
    // alongside the crate under eval/tasks.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("eval").join("tasks")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn copy_dir_skips_git_and_target() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        write(&src.join("a.txt"), "hello");
        write(&src.join("nested/b.txt"), "world");
        write(&src.join(".git/config"), "should be skipped");
        write(&src.join("target/junk"), "should be skipped");

        let dst = tmp.path().join("dst");
        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(dst.join("nested/b.txt")).unwrap(),
            "world"
        );
        assert!(!dst.join(".git").exists());
        assert!(!dst.join("target").exists());
    }

    #[test]
    fn baseline_and_patch_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write(&ws.join("file.txt"), "value=0\n");
        git_init_baseline(&ws).unwrap();

        // No changes yet -> empty patch.
        let empty = generate_patch(&ws).unwrap();
        assert!(empty.is_empty(), "expected empty patch, got {empty:?}");

        // Mutate, add a file, and re-diff.
        fs::write(ws.join("file.txt"), "value=42\n").unwrap();
        fs::write(ws.join("new.txt"), "brand new\n").unwrap();
        let patch = generate_patch(&ws).unwrap();
        assert!(!patch.is_empty());
        assert!(patch.unified_diff.contains("value=42"));
        assert!(patch.changed_files.iter().any(|f| f == "file.txt"));
        assert!(patch.changed_files.iter().any(|f| f == "new.txt"));
        assert!(patch.insertions >= 2);
    }

    #[test]
    fn excludes_build_artifacts_from_patch() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write(&ws.join("src/lib.rs"), "pub fn f() {}\n");
        git_init_baseline(&ws).unwrap();

        // Simulate cargo populating target/ and a lockfile during the run, plus
        // a real source edit.
        write(&ws.join("target/debug/junk.o"), "binary");
        write(&ws.join("Cargo.lock"), "# generated\n");
        fs::write(ws.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let patch = generate_patch(&ws).unwrap();
        assert!(patch.changed_files.iter().any(|f| f == "src/lib.rs"));
        assert!(
            !patch.changed_files.iter().any(|f| f.contains("target/")),
            "target/ leaked into patch: {:?}",
            patch.changed_files
        );
        assert!(
            !patch.changed_files.iter().any(|f| f == "Cargo.lock"),
            "Cargo.lock leaked into patch: {:?}",
            patch.changed_files
        );
    }

    #[test]
    fn detects_deleted_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        write(&ws.join("keep.txt"), "a\n");
        write(&ws.join("tests/it.rs"), "#[test] fn t() {}\n");
        git_init_baseline(&ws).unwrap();

        fs::remove_file(ws.join("tests/it.rs")).unwrap();
        let patch = generate_patch(&ws).unwrap();
        assert!(patch.deleted_files.iter().any(|f| f == "tests/it.rs"));
        assert!(looks_like_test_path("tests/it.rs"));
    }
}

#[cfg(test)]
mod git_fixture_tests {
    use super::*;
    use crate::task::{FixtureSpec, OracleSpec};
    use std::fs;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A local fixtures repo holding `l3-demo/src/lib.rs`; returns (dir, url, sha).
    fn fixtures_repo() -> (tempfile::TempDir, String, String) {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        git(
            d.path(),
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        fs::create_dir_all(d.path().join("l3-demo/src")).unwrap();
        fs::write(
            d.path().join("l3-demo/src/lib.rs"),
            "pub fn v() -> u8 { 1 }\n",
        )
        .unwrap();
        fs::write(d.path().join("other.txt"), "not part of the task\n").unwrap();
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-q", "-m", "snapshot"]);
        let sha = git(d.path(), &["rev-parse", "HEAD"]);
        let url = format!("file://{}", d.path().display());
        (d, url, sha)
    }

    fn task(url: &str, rev: &str) -> EvalTask {
        let mut t = EvalTask::minimal("t", "t", "p", OracleSpec::default(), PathBuf::from("."));
        t.fixture = FixtureSpec {
            kind: FixtureKind::Git,
            path: Some("l3-demo".into()),
            repo: Some(url.into()),
            rev: Some(rev.into()),
        };
        t
    }

    // A git fixture at a pinned commit materializes only its sub-path into the workspace.
    #[test]
    fn git_fixture_copies_pinned_subdir() {
        let (_repo, url, sha) = fixtures_repo();
        let cache = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        prepare_workspace_with_cache(&task(&url, &sha), ws.path(), cache.path()).unwrap();
        assert_eq!(
            fs::read_to_string(ws.path().join("src/lib.rs")).unwrap(),
            "pub fn v() -> u8 { 1 }\n"
        );
        assert!(!ws.path().join("other.txt").exists());
        assert!(!ws.path().join("l3-demo").exists());
    }

    // Once cached, a git fixture materializes without reaching the source repo (offline).
    #[test]
    fn git_fixture_cache_works_offline() {
        let (repo, url, sha) = fixtures_repo();
        let cache = tempfile::tempdir().unwrap();
        let ws1 = tempfile::tempdir().unwrap();
        prepare_workspace_with_cache(&task(&url, &sha), ws1.path(), cache.path()).unwrap();
        drop(repo);
        let ws2 = tempfile::tempdir().unwrap();
        prepare_workspace_with_cache(&task(&url, &sha), ws2.path(), cache.path()).unwrap();
        assert!(ws2.path().join("src/lib.rs").exists());
    }

    // A rev that is not in the repo fails with a clear error and leaves no cache entry.
    #[test]
    fn git_fixture_unknown_rev_fails() {
        let (_repo, url, _sha) = fixtures_repo();
        let cache = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let bogus = "0123456789abcdef0123456789abcdef01234567";
        let err =
            prepare_workspace_with_cache(&task(&url, bogus), ws.path(), cache.path()).unwrap_err();
        assert!(err.to_string().contains(bogus), "{err}");
        assert!(!cache.path().join(bogus).exists());
    }

    // A git fixture whose sub-path is missing at the pinned commit is reported as not found.
    #[test]
    fn git_fixture_missing_path_fails() {
        let (_repo, url, sha) = fixtures_repo();
        let cache = tempfile::tempdir().unwrap();
        let ws = tempfile::tempdir().unwrap();
        let mut t = task(&url, &sha);
        t.fixture.path = Some("l3-nope".into());
        let err = prepare_workspace_with_cache(&t, ws.path(), cache.path()).unwrap_err();
        assert!(matches!(err, FixtureError::FixtureNotFound(_)), "{err}");
    }
}
