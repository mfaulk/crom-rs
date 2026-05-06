//! End-to-end smoke tests for the M9 CLI commands. Each test runs the
//! actual `code-agent` binary against a tempdir so commands such as
//! `init`, `doctor`, `paths`, `config`, `status`, and `completions` are
//! exercised through the same plumbing real users hit.

use std::path::PathBuf;
use std::process::Command;

use tempfile::TempDir;

fn binary() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    // target/debug/deps/<this-test>
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.push("code-agent");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

fn run(args: &[&str], dir: &std::path::Path) -> std::process::Output {
    Command::new(binary())
        .args(args)
        .current_dir(dir)
        // Strip any global API-key var so the doctor reports a fail in a
        // way the test can rely on.
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("failed to launch code-agent binary")
}

#[test]
// `init --minimal` writes settings.json and AGENTS.md.
fn init_minimal_writes_files() {
    let dir = TempDir::new().unwrap();
    let out = run(&["init", "--minimal"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dir.path().join(".agent/settings.json").exists());
    assert!(dir.path().join("AGENTS.md").exists());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Welcome to code-agent"));
}

#[test]
// `init --ci` produces a plain-renderer settings.json.
fn init_ci_uses_plain_renderer() {
    let dir = TempDir::new().unwrap();
    let out = run(&["init", "--ci"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".agent/settings.json")).unwrap();
    assert!(body.contains("\"renderer\": \"plain\""));
}

#[test]
// `doctor --json` emits valid JSON listing the standard checks.
fn doctor_json_emits_valid_json() {
    let dir = TempDir::new().unwrap();
    let out = run(&["doctor", "--json", "--settings", "none"], dir.path());
    // Doctor exits non-zero when the API key is missing — that's fine.
    let s = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json parse");
    let checks = parsed.as_array().expect("array");
    assert!(checks.iter().any(|c| c["id"] == "provider.api_key"));
    assert!(checks.iter().any(|c| c["id"] == "workspace.agent_dir"));
}

#[test]
// `paths --json` emits the resolved paths object.
fn paths_json_lists_known_paths() {
    let dir = TempDir::new().unwrap();
    let out = run(&["paths", "--json"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json parse");
    assert!(parsed["agent_dir"].is_string());
    assert!(parsed["sessions_dir"].is_string());
    assert!(parsed["checkpoints_dir"].is_string());
}

#[test]
// `completions bash` emits a non-empty completion script.
fn completions_bash_emits_output() {
    let dir = TempDir::new().unwrap();
    let out = run(&["completions", "bash"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("code-agent"));
    assert!(s.contains("complete"));
}

#[test]
// All four shells produce some output.
fn completions_supports_four_shells() {
    let dir = TempDir::new().unwrap();
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = run(&["completions", shell], dir.path());
        assert!(
            out.status.success(),
            "shell {shell} failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!out.stdout.is_empty(), "shell {shell} produced no output");
    }
}

#[test]
// `config` prints an effective config block after init.
fn config_shows_effective_after_init() {
    let dir = TempDir::new().unwrap();
    run(&["init", "--minimal"], dir.path());
    let out = run(&["config", "--settings", "project"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Effective configuration"));
    assert!(s.contains("renderer:"));
}

#[test]
// `status` prints a session-status block.
fn status_shows_summary() {
    let dir = TempDir::new().unwrap();
    let out = run(&["status", "--settings", "none"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Session status"));
}

#[test]
// `statusline set` writes the new template into settings.json.
fn statusline_set_persists() {
    let dir = TempDir::new().unwrap();
    run(&["init", "--minimal"], dir.path());
    let out = run(&["statusline", "set", "{project} | {model}"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join(".agent/settings.json")).unwrap();
    assert!(body.contains("{project} | {model}"));
}

#[test]
// `statusline set` with an unknown variable rejects the change.
fn statusline_set_rejects_unknown_variable() {
    let dir = TempDir::new().unwrap();
    run(&["init", "--minimal"], dir.path());
    let out = run(&["statusline", "set", "{bogus}"], dir.path());
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown") || stderr.contains("bogus"));
}

#[test]
// `sessions-ext recent` reports zero sessions on a fresh dir.
fn sessions_ext_recent_empty() {
    let dir = TempDir::new().unwrap();
    let out = run(&["sessions-ext", "recent"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("(no sessions found)"));
}

#[test]
// `sessions-ext recover` reports nothing recoverable on a fresh dir.
fn sessions_ext_recover_empty() {
    let dir = TempDir::new().unwrap();
    let out = run(&["sessions-ext", "recover"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("(no recoverable"));
}

#[test]
// `tui --renderer plain --snapshot` prints a status-line preview line and exits.
fn tui_plain_snapshot_runs() {
    let dir = TempDir::new().unwrap();
    run(&["init", "--minimal"], dir.path());
    let out = run(&["tui", "--renderer", "plain", "--snapshot"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("[tui]"));
}

#[test]
// `statusline set` against a settings file with invalid JSON refuses to
// overwrite. Regression test for the silent-data-loss bug where the prior
// implementation `unwrap_or_else`'d to `{}` and clobbered user content.
fn statusline_set_refuses_invalid_settings_file() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
    std::fs::write(dir.path().join(".agent/settings.json"), b"not json{").unwrap();
    let out = run(&["statusline", "set", "{project}"], dir.path());
    assert!(
        !out.status.success(),
        "expected failure, got success. stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let body = std::fs::read_to_string(dir.path().join(".agent/settings.json")).unwrap();
    assert_eq!(body, "not json{", "file must not be modified");
}

#[test]
// `doctor --fix` without --yes in a non-interactive shell skips fixes
// rather than applying them silently.
fn doctor_fix_requires_yes_in_non_interactive() {
    let dir = TempDir::new().unwrap();
    let out = run(&["doctor", "--fix", "--settings", "none"], dir.path());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("skipped"), "stdout: {s}");
    assert!(!dir.path().join(".agent/sessions").exists());
}

#[test]
// `doctor --fix --yes` creates the .agent directory non-interactively.
fn doctor_fix_with_yes_creates_dirs() {
    let dir = TempDir::new().unwrap();
    let out = run(
        &["doctor", "--fix", "--yes", "--settings", "none"],
        dir.path(),
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("created"), "stdout: {s}");
    assert!(dir.path().join(".agent/sessions").exists());
    assert!(dir.path().join(".agent/checkpoints").exists());
}

#[test]
// `sessions-ext clean --yes` also removes the orphan lock file.
fn sessions_ext_clean_removes_orphan_lock() {
    use std::time::Duration;
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".agent/sessions")).unwrap();
    let session = "01ABCDEFGHIJKLMNOPQRSTUVWX";
    let jsonl = dir.path().join(format!(".agent/sessions/{session}.jsonl"));
    let lock = dir.path().join(format!(".agent/sessions/{session}.lock"));
    std::fs::write(&jsonl, b"{}\n").unwrap();
    std::fs::write(&lock, b"{}").unwrap();
    // Make the file appear old by setting mtime far in the past.
    let one_year_ago = std::time::SystemTime::now() - Duration::from_secs(365 * 86_400);
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&jsonl)
        .unwrap();
    f.set_modified(one_year_ago).unwrap();
    drop(f);

    let out = run(
        &["sessions-ext", "clean", "--older-than", "30d", "--yes"],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!jsonl.exists(), "JSONL should be removed");
    assert!(!lock.exists(), "orphan lock should be removed");
}

#[test]
// `--version --verbose` prints build profile and target information.
fn verbose_version_includes_build_info() {
    let dir = TempDir::new().unwrap();
    let out = run(&["--version", "--verbose"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("code-agent"));
    assert!(s.contains("build_profile"));
    assert!(s.contains("target"));
}

#[test]
// `paths` (no JSON) prints the agent_dir line.
fn paths_pretty_lists_agent_dir() {
    let dir = TempDir::new().unwrap();
    let out = run(&["paths"], dir.path());
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("agent_dir:"));
    assert!(s.contains("sessions_dir:"));
}

#[test]
// `init --minimal` then `doctor --json` shows the workspace dirs as ok.
fn init_then_doctor_reports_writable_dirs() {
    let dir = TempDir::new().unwrap();
    run(&["init", "--minimal"], dir.path());
    let out = run(&["doctor", "--json", "--settings", "project"], dir.path());
    let s = String::from_utf8_lossy(&out.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&s).expect("json parse");
    let checks = parsed.as_array().unwrap();
    let agent_dir_check = checks
        .iter()
        .find(|c| c["id"] == "workspace.agent_dir")
        .unwrap();
    assert_eq!(agent_dir_check["status"], "ok");
}

#[test]
// Explicitly omitted commands stay omitted: `feedback`, `report`, `theme`,
// `output-style` should not appear in the top-level help.
fn omitted_commands_not_present() {
    let dir = TempDir::new().unwrap();
    let out = run(&["--help"], dir.path());
    let s = String::from_utf8_lossy(&out.stdout);
    for forbidden in ["feedback", "report", "theme", "output-style", "beta"] {
        assert!(
            !s.contains(forbidden),
            "top-level help contains forbidden command {forbidden}: {s}"
        );
    }
}
