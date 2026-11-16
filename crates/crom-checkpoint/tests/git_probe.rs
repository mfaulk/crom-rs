use std::path::Path;
use std::process::{Command, Stdio};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git on PATH");
    assert!(st.success(), "git {args:?} failed");
}

/// Probing a repo whose config sets `core.fsmonitor` reports state without running it (#334).
#[tokio::test]
async fn probe_does_not_execute_repo_fsmonitor() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pwned");
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-qm", "init"]);
    let cmd = format!("touch '{}'; false", marker.display());
    git(dir.path(), &["config", "core.fsmonitor", &cmd]);
    std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();

    let state = crom_checkpoint::probe_git_state(dir.path())
        .await
        .expect("git repo");

    assert!(!marker.exists(), "core.fsmonitor executed");
    assert!(state.is_git_repo);
    assert!(state.dirty);
    assert!(state.head.is_some());
}
