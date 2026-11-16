//! Host-side `git` invocations against repositories Crom does not trust.
//!
//! Git honours repo-local config, so a cloned repo's `.git/config` can make
//! `git status` run arbitrary commands (`core.fsmonitor`, hooks, filter and
//! diff drivers). Every host `git` call must go through [`host_git`] or
//! [`host_git_async`], which override those keys on the command line
//! (`-c` beats every config file). Threat model gap G10, issue #334.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// Keys overridden for every invocation. Network-only exec keys
/// (`core.sshCommand`, `credential.helper`) are left alone: Crom never
/// fetches inside an untrusted repo, and overriding them would clobber the
/// user's global setup for fixture fetches.
const FIXED_OVERRIDES: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "core.pager=cat",
    "diff.ignoreSubmodules=all",
    "status.submoduleSummary=false",
    "submodule.recurse=false",
    "protocol.ext.allow=never",
];

/// Driver keys (`<section>.<name>.<key>`) whose value is a shell command.
/// Names are repo-chosen, so they are discovered and neutralised per repo.
const DRIVER_KEY_PATTERN: &str =
    r"^((filter|diff|merge)\..+\.(clean|smudge|process|textconv|command|driver)|diff\.external)$";

const PROBE_ARGS: &[&str] = &[
    "config",
    "--includes",
    "--name-only",
    "--get-regexp",
    DRIVER_KEY_PATTERN,
];

/// A `git` command for `cwd` with repo-controlled exec config neutralised.
/// Runs a short blocking `git config` probe; use [`host_git_async`] from
/// async code.
pub fn host_git(cwd: &Path) -> Command {
    let mut probe = base_command(cwd);
    probe
        .args(PROBE_ARGS)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let keys = probe
        .output()
        .map(|o| parse_keys(&o.stdout))
        .unwrap_or_default();
    hardened(cwd, &keys)
}

/// Async counterpart of [`host_git`]; the probe does not block the runtime.
pub async fn host_git_async(cwd: &Path) -> tokio::process::Command {
    let mut probe = tokio::process::Command::from(base_command(cwd));
    probe
        .args(PROBE_ARGS)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    let keys = probe
        .output()
        .await
        .map(|o| parse_keys(&o.stdout))
        .unwrap_or_default();
    let mut cmd = tokio::process::Command::from(hardened(cwd, &keys));
    cmd.kill_on_drop(true);
    cmd
}

/// Reading config never executes anything, so the probe needs only this
/// environment hardening.
fn base_command(cwd: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(cwd)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_PAGER");
    cmd
}

fn hardened(cwd: &Path, driver_keys: &[String]) -> Command {
    let mut cmd = base_command(cwd);
    for kv in FIXED_OVERRIDES {
        cmd.arg("-c").arg(kv);
    }
    for key in driver_keys {
        cmd.arg("-c")
            .arg(OsString::from(format!("{key}={}", neutral_value(key))));
    }
    cmd
}

/// Git skips empty filter commands but *runs* an empty textconv, diff or
/// merge command, so those get a harmless or fail-closed replacement.
fn neutral_value(key: &str) -> &'static str {
    if key.ends_with(".textconv") {
        "cat"
    } else if key == "diff.external" || key.ends_with(".command") || key.ends_with(".driver") {
        "false"
    } else {
        ""
    }
}

fn parse_keys(stdout: &[u8]) -> Vec<String> {
    let mut keys: Vec<String> = String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string)
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .current_dir(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git on PATH");
        assert!(st.success(), "git {args:?} failed");
    }

    /// A repo with one tracked file and a marker path that must never appear.
    fn evil_repo() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        git(dir.path(), &["init", "-q"]);
        git(dir.path(), &["config", "user.email", "t@example.com"]);
        git(dir.path(), &["config", "user.name", "t"]);
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        git(dir.path(), &["commit", "-qm", "init"]);
        (dir, marker)
    }

    fn status(dir: &Path) -> std::process::Output {
        host_git(dir)
            .args(["status", "--porcelain"])
            .output()
            .unwrap()
    }

    /// `core.fsmonitor` in repo config does not execute on `git status`.
    #[test]
    fn fsmonitor_is_not_executed() {
        let (dir, marker) = evil_repo();
        let cmd = format!("touch '{}'; false", marker.display());
        git(dir.path(), &["config", "core.fsmonitor", &cmd]);
        assert!(status(dir.path()).status.success());
        assert!(!marker.exists(), "core.fsmonitor executed");
    }

    /// A repo-defined clean filter does not execute when status hashes a dirty file.
    #[test]
    fn filter_driver_is_not_executed() {
        let (dir, marker) = evil_repo();
        let cmd = format!("touch '{}'; cat", marker.display());
        git(dir.path(), &["config", "filter.evil.clean", &cmd]);
        git(dir.path(), &["config", "filter.evil.process", &cmd]);
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let out = status(dir.path());
        assert!(out.status.success());
        assert!(!marker.exists(), "filter driver executed");
        assert!(String::from_utf8_lossy(&out.stdout).contains("a.txt"));
    }

    /// Filter drivers defined in an included config file are also blanked.
    #[test]
    fn included_filter_driver_is_not_executed() {
        let (dir, marker) = evil_repo();
        let inc = dir.path().join("evil.inc");
        std::fs::write(
            &inc,
            format!(
                "[filter \"x\"]\n\tclean = touch '{}'; cat\n",
                marker.display()
            ),
        )
        .unwrap();
        git(
            dir.path(),
            &["config", "include.path", inc.to_str().unwrap()],
        );
        std::fs::write(dir.path().join(".gitattributes"), "*.txt filter=x\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        assert!(status(dir.path()).status.success());
        assert!(!marker.exists(), "included filter driver executed");
    }

    /// Repo hooks do not run for hook-firing commands such as `commit`.
    #[test]
    fn hooks_are_not_executed() {
        let (dir, marker) = evil_repo();
        let hooks = dir.path().join("myhooks");
        std::fs::create_dir(&hooks).unwrap();
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        git(
            dir.path(),
            &["config", "core.hooksPath", hooks.to_str().unwrap()],
        );
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        let ok = host_git(dir.path())
            .args(["add", "b.txt"])
            .status()
            .unwrap()
            .success()
            && host_git(dir.path())
                .args(["commit", "-qm", "b"])
                .status()
                .unwrap()
                .success();
        assert!(ok);
        assert!(!marker.exists(), "hook executed");
    }

    /// A repo-defined textconv driver does not run for `git diff`.
    #[test]
    fn diff_textconv_is_not_executed() {
        let (dir, marker) = evil_repo();
        let cmd = format!("sh -c 'touch {}; cat \"$1\"' --", marker.display());
        git(dir.path(), &["config", "diff.evil.textconv", &cmd]);
        std::fs::write(dir.path().join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let out = host_git(dir.path()).args(["diff"]).output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!marker.exists(), "textconv executed");
    }

    /// A repo `diff.external` does not run; the diff fails closed instead.
    #[test]
    fn diff_external_is_not_executed() {
        let (dir, marker) = evil_repo();
        let cmd = format!("touch '{}'", marker.display());
        git(dir.path(), &["config", "diff.external", &cmd]);
        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let _ = host_git(dir.path()).args(["diff"]).output().unwrap();
        assert!(!marker.exists(), "diff.external executed");
    }

    /// Outside a repository the helper still produces a usable command.
    #[test]
    fn works_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let out = host_git(dir.path())
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .unwrap();
        assert!(!out.status.success());
    }

    /// The async helper applies the same neutralisation as the sync one.
    #[tokio::test]
    async fn async_helper_does_not_execute_fsmonitor() {
        let (dir, marker) = evil_repo();
        let cmd = format!("touch '{}'; false", marker.display());
        git(dir.path(), &["config", "core.fsmonitor", &cmd]);
        let out = host_git_async(dir.path())
            .await
            .args(["status", "--porcelain"])
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
        assert!(!marker.exists(), "core.fsmonitor executed");
    }

    /// A submodule's own fsmonitor does not run when the parent repo is probed.
    #[test]
    fn submodule_config_is_not_executed() {
        let (dir, marker) = evil_repo();
        let (sub, _) = evil_repo();
        let st = Command::new("git")
            .current_dir(dir.path())
            .args(["-c", "protocol.file.allow=always", "submodule", "add", "-q"])
            .arg(sub.path())
            .arg("sub")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
        let cmd = format!("touch '{}'; false", marker.display());
        git(&dir.path().join("sub"), &["config", "core.fsmonitor", &cmd]);
        git(&dir.path().join("sub"), &["config", "filter.s.clean", &cmd]);
        std::fs::write(dir.path().join("sub/.gitattributes"), "*.txt filter=s\n").unwrap();
        std::fs::write(dir.path().join("sub/a.txt"), "two\n").unwrap();
        assert!(status(dir.path()).status.success());
        assert!(!marker.exists(), "submodule config executed");
    }
}
