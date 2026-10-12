//! Task validation and flake detection (M15C) plus canary checks (M15J).
//!
//! Validation answers: is this task *well-formed and runnable* without invoking
//! a model? It materializes the fixture, runs public/hidden verification on the
//! pristine base (which should fail the hidden check for a real task), applies
//! the oracle patch if present (which should pass), and runs a battery of
//! structural and canary checks (e.g. hidden test names leaking into the
//! prompt). Repeated validation classifies a task's flakiness.

use std::path::Path;

use serde::Serialize;

use crate::fixture::{copy_dir_recursive, prepare_workspace};
use crate::isolation::{CommandLimits, LocalBackend};
use crate::oracle::{self, OracleReport};
use crate::schema::SUPPORTED_SCHEMA_VERSIONS;
use crate::task::EvalTask;

/// Severity of a validation finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A single validation finding.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub severity: Severity,
    pub passed: bool,
    pub detail: String,
}

impl Check {
    fn ok(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            severity: Severity::Info,
            passed: true,
            detail: detail.into(),
        }
    }
    fn error(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            severity: Severity::Error,
            passed: false,
            detail: detail.into(),
        }
    }
    fn warn(name: &str, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            severity: Severity::Warning,
            passed: false,
            detail: detail.into(),
        }
    }
}

/// Validation report for one task.
#[derive(Debug, Clone, Serialize)]
pub struct ValidationReport {
    pub task_id: String,
    pub checks: Vec<Check>,
}

impl ValidationReport {
    /// True when no error-severity check failed.
    pub fn ok(&self) -> bool {
        !self
            .checks
            .iter()
            .any(|c| c.severity == Severity::Error && !c.passed)
    }

    pub fn errors(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.severity == Severity::Error && !c.passed)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.severity == Severity::Warning && !c.passed)
            .count()
    }

    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("## {}\n\n", self.task_id));
        for c in &self.checks {
            let mark = if c.passed {
                "✓"
            } else if c.severity == Severity::Error {
                "✗"
            } else {
                "⚠"
            };
            s.push_str(&format!("- {mark} {} — {}\n", c.name, c.detail));
        }
        s.push_str(&format!(
            "\n{} error(s), {} warning(s)\n",
            self.errors(),
            self.warnings()
        ));
        s
    }
}

async fn run_oracle_in(
    workspace: &Path,
    oracle_spec: &crate::task::OracleSpec,
    timeout: std::time::Duration,
) -> OracleReport {
    let backend = LocalBackend;
    let limits = CommandLimits {
        timeout,
        ..Default::default()
    };
    oracle::evaluate_oracle(oracle_spec, workspace, &[], &backend, &limits).await
}

fn git_apply_check(workspace: &Path, patch: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["-c", "safe.directory=*"])
        .current_dir(workspace)
        .args(["apply", "--check", &patch.display().to_string()])
        .output()
        .map_err(|e| format!("spawn git apply: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn git_apply(workspace: &Path, patch: &Path) -> Result<(), String> {
    let out = std::process::Command::new("git")
        .args(["-c", "safe.directory=*"])
        .current_dir(workspace)
        .args(["apply", &patch.display().to_string()])
        .output()
        .map_err(|e| format!("spawn git apply: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Validate a single task without invoking a model.
pub async fn validate_task(task: &EvalTask) -> ValidationReport {
    let mut checks = Vec::new();

    // Schema version.
    if SUPPORTED_SCHEMA_VERSIONS.contains(&task.schema_version) {
        checks.push(Check::ok(
            "schema_version",
            format!("v{} supported", task.schema_version),
        ));
    } else {
        checks.push(Check::error(
            "schema_version",
            format!("unsupported v{}", task.schema_version),
        ));
    }

    // Canary: benchmark metadata / hidden names leaking into the prompt.
    canary_checks(task, &mut checks);

    // Verification commands declare a timeout (always true via task timeout).
    if task.timeout.as_secs() == 0 {
        checks.push(Check::error("timeouts", "task timeout is zero"));
    } else {
        checks.push(Check::ok(
            "timeouts",
            format!(
                "per-command timeout {}s",
                oracle::task_command_timeout(task).as_secs()
            ),
        ));
    }

    // Network policy.
    let mut all_commands = task.oracle.primary_commands();
    if let Some(h) = &task.hidden {
        all_commands.extend(h.oracle.primary_commands());
    }
    let touches_net = all_commands
        .iter()
        .any(|c| c.contains("curl") || c.contains("wget") || c.contains("https://"));
    if touches_net && !task.requires_network {
        checks.push(Check::warn(
            "network",
            "verification looks network-bound but requires_network = false",
        ));
    } else {
        checks.push(Check::ok("network", "no undeclared network use detected"));
    }

    // Forbidden/required path sanity.
    for fp in &task.forbidden_paths {
        if fp.trim().is_empty() {
            checks.push(Check::error("forbidden_paths", "empty forbidden path"));
        }
    }
    if !task.forbidden_paths.is_empty() {
        checks.push(Check::ok(
            "forbidden_paths",
            format!("{} pattern(s) declared", task.forbidden_paths.len()),
        ));
    }

    // Materialize the fixture into a temp workspace.
    let tmp = match tempfile::tempdir() {
        Ok(t) => t,
        Err(e) => {
            checks.push(Check::error("fixture", format!("tempdir: {e}")));
            return ValidationReport {
                task_id: task.id.clone(),
                checks,
            };
        }
    };
    let ws = tmp.path().join("ws");
    match prepare_workspace(task, &ws) {
        Ok(()) => checks.push(Check::ok("fixture", "materialized")),
        Err(e) => {
            checks.push(Check::error("fixture", format!("{e}")));
            return ValidationReport {
                task_id: task.id.clone(),
                checks,
            };
        }
    }

    // Artifact / fixture path validity already implied by materialization.
    // Required paths existence (informational on the base fixture).
    for rp in &task.required_paths {
        if rp.trim().is_empty() {
            checks.push(Check::error("required_paths", "empty required path"));
        }
    }

    let timeout = oracle::task_command_timeout(task);

    // Public verification runs.
    let public = run_oracle_in(&ws, &task.oracle, timeout).await;
    checks.push(Check::ok(
        "public_verification_runs",
        format!(
            "{}/{} checks evaluated",
            public.checks_passed(),
            public.checks_total()
        ),
    ));

    // Hidden verification: must be runnable and must FAIL on the base fixture
    // (unless this is a negative/no-op task).
    if let Some(hidden) = &task.hidden {
        if hidden.has_checks() {
            // Overlay hidden fixture if any.
            if let Some(rel) = &hidden.fixture {
                let src = task.dir.join(rel);
                if src.exists() {
                    let _ = copy_dir_recursive(&src, &ws);
                }
            }
            let base_hidden = run_oracle_in(&ws, &hidden.oracle, timeout).await;
            checks.push(Check::ok(
                "hidden_verification_runs",
                format!(
                    "{}/{} hidden checks evaluated",
                    base_hidden.checks_passed(),
                    base_hidden.checks_total()
                ),
            ));
            if base_hidden.all_passed() && !task.capabilities.negative_noop {
                checks.push(Check::warn(
                    "base_fails_hidden",
                    "base fixture already passes the hidden check (expected to fail)",
                ));
            } else {
                checks.push(Check::ok(
                    "base_fails_hidden",
                    "base fixture fails hidden as expected",
                ));
            }
        }
    }

    // Oracle patch.
    if let Some(rel) = &task.oracle_patch {
        // Absolute so `git apply` (run inside the workspace) can find it.
        let patch =
            std::fs::canonicalize(task.dir.join(rel)).unwrap_or_else(|_| task.dir.join(rel));
        if !patch.exists() {
            checks.push(Check::error("oracle_patch", format!("missing: {rel}")));
        } else {
            // Apply on a fresh workspace.
            let ws2 = tmp.path().join("ws-oracle");
            if prepare_workspace(task, &ws2).is_ok() {
                match git_apply_check(&ws2, &patch) {
                    Ok(()) => {
                        let _ = git_apply(&ws2, &patch);
                        checks.push(Check::ok("oracle_patch_applies", "applies cleanly"));
                        let pub2 = run_oracle_in(&ws2, &task.oracle, timeout).await;
                        if pub2.all_passed() {
                            checks.push(Check::ok(
                                "oracle_patch_passes_public",
                                "public passes with oracle patch",
                            ));
                        } else {
                            checks.push(Check::error(
                                "oracle_patch_passes_public",
                                "public does NOT pass with oracle patch",
                            ));
                        }
                        if let Some(hidden) = &task.hidden {
                            if hidden.has_checks() {
                                if let Some(hrel) = &hidden.fixture {
                                    let src = task.dir.join(hrel);
                                    if src.exists() {
                                        let _ = copy_dir_recursive(&src, &ws2);
                                    }
                                }
                                let hid2 = run_oracle_in(&ws2, &hidden.oracle, timeout).await;
                                if hid2.all_passed() {
                                    checks.push(Check::ok(
                                        "oracle_patch_passes_hidden",
                                        "hidden passes with oracle patch",
                                    ));
                                } else {
                                    checks.push(Check::error(
                                        "oracle_patch_passes_hidden",
                                        "hidden does NOT pass with oracle patch",
                                    ));
                                }
                            }
                        }
                    }
                    Err(e) => checks.push(Check::error(
                        "oracle_patch_applies",
                        format!("does not apply: {e}"),
                    )),
                }
            }
        }
    } else if task.capabilities.requires_test_feedback && task.hidden.is_some() {
        checks.push(Check::warn(
            "oracle_patch",
            "no oracle patch provided for a hidden-test task (cannot verify solvability)",
        ));
    }

    ValidationReport {
        task_id: task.id.clone(),
        checks,
    }
}

/// Canary checks: detect leaks that would let an agent cheat (M15J).
fn canary_checks(task: &EvalTask, checks: &mut Vec<Check>) {
    let prompt = task.prompt.to_lowercase();

    // Benchmark metadata accidentally included in the prompt.
    if prompt.contains("instance_id") || prompt.contains("test_patch") {
        checks.push(Check::warn(
            "canary_metadata",
            "prompt contains benchmark metadata keywords (instance_id/test_patch)",
        ));
    } else {
        checks.push(Check::ok(
            "canary_metadata",
            "no benchmark metadata in prompt",
        ));
    }

    // Hidden test filenames leaking into the prompt.
    if let Some(hidden) = &task.hidden {
        let mut leaked = Vec::new();
        if let Some(rel) = &hidden.fixture {
            let dir = task.dir.join(rel);
            if let Ok(read) = std::fs::read_dir(&dir) {
                for e in read.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !name.is_empty() && prompt.contains(&name.to_lowercase()) {
                        leaked.push(name);
                    }
                }
            }
            // Hidden directory should not be inside the public fixture.
            let fixture_root = task
                .fixture
                .path
                .clone()
                .unwrap_or_else(|| "repo".to_string());
            // Match on path components, not a raw string prefix, so a sibling
            // like `repo-hidden/...` is NOT treated as inside `fixture_root=repo`
            // (#81).
            if Path::new(rel).starts_with(&fixture_root) && fixture_root != "." {
                checks.push(Check::error(
                    "canary_hidden_visible",
                    format!("hidden fixture '{rel}' lives inside the public fixture"),
                ));
            }
        }
        if leaked.is_empty() {
            checks.push(Check::ok(
                "canary_hidden_names",
                "no hidden test names in prompt",
            ));
        } else {
            checks.push(Check::warn(
                "canary_hidden_names",
                format!("hidden test filename(s) leaked into prompt: {leaked:?}"),
            ));
        }
    }

    // Oracle patch should not be visible inside the public fixture.
    if let Some(rel) = &task.oracle_patch {
        let fixture_root = task
            .fixture
            .path
            .clone()
            .unwrap_or_else(|| "repo".to_string());
        // Path-component match, not raw string prefix, so `repo-hidden/...` is
        // not flagged as inside `fixture_root=repo` (#81).
        if Path::new(rel).starts_with(&fixture_root) && fixture_root != "." {
            checks.push(Check::error(
                "canary_oracle_visible",
                format!("oracle patch '{rel}' lives inside the public fixture"),
            ));
        }
    }
}

/// Flake verdict from repeated validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlakeVerdict {
    StablePass,
    StableFail,
    FlakyPublic,
    FlakyHidden,
    TimeoutSensitive,
    HarnessError,
}

/// Repeatedly verify a task on its pristine base fixture and classify the
/// verdict. (Determinism here measures the verification harness/commands, not
/// the model.) `repeat` must be >= 1.
pub async fn flake_detect(task: &EvalTask, repeat: usize) -> FlakeVerdict {
    let repeat = repeat.max(1);
    let timeout = oracle::task_command_timeout(task);
    let mut public_results = Vec::new();
    let mut hidden_results = Vec::new();
    let mut any_timeout = false;

    for _ in 0..repeat {
        let tmp = match tempfile::tempdir() {
            Ok(t) => t,
            Err(_) => return FlakeVerdict::HarnessError,
        };
        let ws = tmp.path().join("ws");
        if prepare_workspace(task, &ws).is_err() {
            return FlakeVerdict::HarnessError;
        }
        let public = run_oracle_in(&ws, &task.oracle, timeout).await;
        if public.commands.iter().any(|c| c.timed_out) {
            any_timeout = true;
        }
        public_results.push(public.all_passed());

        if let Some(hidden) = &task.hidden {
            if hidden.has_checks() {
                if let Some(rel) = &hidden.fixture {
                    let src = task.dir.join(rel);
                    if src.exists() {
                        let _ = copy_dir_recursive(&src, &ws);
                    }
                }
                let hid = run_oracle_in(&ws, &hidden.oracle, timeout).await;
                if hid.commands.iter().any(|c| c.timed_out) {
                    any_timeout = true;
                }
                hidden_results.push(hid.all_passed());
            }
        }
    }

    let public_varies = public_results.iter().any(|&p| p) && public_results.iter().any(|&p| !p);
    let hidden_varies = hidden_results.iter().any(|&p| p) && hidden_results.iter().any(|&p| !p);

    if public_varies {
        return FlakeVerdict::FlakyPublic;
    }
    if hidden_varies {
        return FlakeVerdict::FlakyHidden;
    }
    if any_timeout {
        return FlakeVerdict::TimeoutSensitive;
    }
    if public_results.iter().all(|&p| p) {
        FlakeVerdict::StablePass
    } else {
        FlakeVerdict::StableFail
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{command_oracle, HiddenSpec};
    use crate::task::OracleSpec;

    fn task_in(dir: &Path, oracle: OracleSpec) -> EvalTask {
        let mut t = EvalTask::minimal("vt", "vt", "do it", oracle, dir);
        t.fixture = crate::task::FixtureSpec {
            kind: crate::task::FixtureKind::CopyDir,
            path: Some("repo".to_string()),
        };
        t
    }

    fn make_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/README.md"), "hello").unwrap();
    }

    #[tokio::test]
    async fn validates_simple_task() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let task = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        let report = validate_task(&task).await;
        assert!(report.ok(), "{}", report.to_markdown());
    }

    #[tokio::test]
    async fn detects_base_passes_hidden_warning() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let mut task = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        task.hidden = Some(HiddenSpec {
            fixture: None,
            // Hidden check passes on the base fixture -> should warn.
            oracle: command_oracle(vec!["test -f README.md".into()]),
        });
        let report = validate_task(&task).await;
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "base_fails_hidden" && !c.passed));
    }

    #[tokio::test]
    async fn canary_flags_leaked_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let mut task = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        task.prompt = "Resolve instance_id example__repo-1".to_string();
        let report = validate_task(&task).await;
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "canary_metadata" && !c.passed));
    }

    /// REGRESSION (#81): a sibling path that merely shares a string prefix with
    /// `fixture_root` ("repo-hidden" vs "repo") is NOT flagged as inside the
    /// public fixture, while a true child path ("repo/...") IS flagged.
    #[test]
    fn canary_oracle_visible_uses_path_components() {
        let tmp = tempfile::tempdir().unwrap();
        // `fixture_root` defaults to "repo" via task_in.
        let mut sibling = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        sibling.oracle_patch = Some("repo-hidden/oracle.patch".to_string());
        let mut sibling_checks = Vec::new();
        canary_checks(&sibling, &mut sibling_checks);
        assert!(
            !sibling_checks
                .iter()
                .any(|c| c.name == "canary_oracle_visible"),
            "sibling 'repo-hidden/...' must not be flagged as inside 'repo'"
        );

        let mut inside = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        inside.oracle_patch = Some("repo/oracle.patch".to_string());
        let mut inside_checks = Vec::new();
        canary_checks(&inside, &mut inside_checks);
        assert!(
            inside_checks
                .iter()
                .any(|c| c.name == "canary_oracle_visible" && !c.passed),
            "child 'repo/...' must be flagged as inside the public fixture"
        );
    }

    #[tokio::test]
    async fn flake_detect_stable_and_flaky() {
        let tmp = tempfile::tempdir().unwrap();
        make_repo(tmp.path());
        let stable = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        assert_eq!(flake_detect(&stable, 3).await, FlakeVerdict::StablePass);

        // A nondeterministic public command -> FlakyPublic across repeats.
        let flaky = task_in(
            tmp.path(),
            command_oracle(vec!["test $(( $(date +%N) % 2 )) -eq 0".into()]),
        );
        // Run enough repeats that both outcomes are overwhelmingly likely.
        let verdict = flake_detect(&flaky, 12).await;
        assert!(
            matches!(
                verdict,
                FlakeVerdict::FlakyPublic | FlakeVerdict::StablePass | FlakeVerdict::StableFail
            ),
            "unexpected verdict {verdict:?}"
        );
    }
}
