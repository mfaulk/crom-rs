//! Task validation and flake detection (M15C) plus canary checks (M15J).
//!
//! Validation answers: is this task *well-formed and runnable* without invoking
//! a model? It materializes the fixture, runs public/hidden verification on the
//! pristine base (which should fail the hidden check for a real task), applies
//! the oracle patch if present (which should pass), and runs a battery of
//! structural and canary checks (e.g. hidden test names leaking into the
//! prompt). `kind = capability` tasks (schema v3) must additionally have
//! hidden verification and a passing reference, and their unmodified fixture
//! must fail (or, for `no_change`, pass) verification; these are errors.
//! Repeated validation classifies a task's flakiness.

use std::path::Path;

use serde::Serialize;

use crate::fixture::{copy_dir_recursive, prepare_workspace};
use crate::isolation::{build_backend, CommandLimits, IsolationBackend, IsolationKind};
use crate::oracle::{self, OracleReport};
use crate::schema::{Level, ReferenceKind, TaskKind, SUPPORTED_SCHEMA_VERSIONS};
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

/// Where validation runs verification commands.
#[derive(Debug, Clone, Default)]
pub struct ValidationOptions {
    /// Verification backend (`local`, or opt-in `docker`/`podman`).
    pub isolation: IsolationKind,
    /// Container image override (container backends only).
    pub container_image: Option<String>,
}

async fn run_oracle_in(
    backend: &dyn IsolationBackend,
    workspace: &Path,
    oracle_spec: &crate::task::OracleSpec,
    timeout: std::time::Duration,
) -> OracleReport {
    let limits = CommandLimits {
        timeout,
        ..Default::default()
    };
    oracle::evaluate_oracle(oracle_spec, workspace, &[], backend, &limits).await
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

/// Validate a single task without invoking a model (local verification).
pub async fn validate_task(task: &EvalTask) -> ValidationReport {
    validate_task_with(task, &ValidationOptions::default()).await
}

/// Validate a single task, running verification on `opts.isolation`.
pub async fn validate_task_with(task: &EvalTask, opts: &ValidationOptions) -> ValidationReport {
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

    // An unavailable backend is an error, never a silent local fallback.
    let backend = match build_backend(&opts.isolation, opts.container_image.clone()) {
        Ok(b) => {
            checks.push(Check::ok(
                "isolation",
                format!("verification runs in {}", b.id()),
            ));
            b
        }
        Err(e) => {
            checks.push(Check::error("isolation", e.to_string()));
            return ValidationReport {
                task_id: task.id.clone(),
                checks,
            };
        }
    };
    let backend = backend.as_ref();

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
    let public = run_oracle_in(backend, &ws, &task.oracle, timeout).await;
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
    let mut base_hidden_passed: Option<bool> = None;
    if let Some(hidden) = &task.hidden {
        if hidden.has_checks() {
            // Overlay hidden fixture if any.
            if let Some(rel) = &hidden.fixture {
                let src = task.dir.join(rel);
                if src.exists() {
                    let _ = copy_dir_recursive(&src, &ws);
                }
            }
            let base_hidden = run_oracle_in(backend, &ws, &hidden.oracle, timeout).await;
            checks.push(Check::ok(
                "hidden_verification_runs",
                format!(
                    "{}/{} hidden checks evaluated",
                    base_hidden.checks_passed(),
                    base_hidden.checks_total()
                ),
            ));
            base_hidden_passed = Some(base_hidden.all_passed());
            if base_hidden.all_passed() && !task.expects_no_change() {
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
                        let pub2 = run_oracle_in(backend, &ws2, &task.oracle, timeout).await;
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
                                let hid2 =
                                    run_oracle_in(backend, &ws2, &hidden.oracle, timeout).await;
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

    if task.kind == Some(TaskKind::Capability) {
        capability_checks(task, public.all_passed(), base_hidden_passed, &mut checks);
    }

    ValidationReport {
        task_id: task.id.clone(),
        checks,
    }
}

/// Rules every `kind = capability` task must satisfy (M17.2): hidden
/// verification, a declared reference that passes public + hidden, and an
/// unmodified fixture that fails (or, for `no_change`, passes) verification.
/// The reference patch itself is exercised by the `oracle_patch_*` checks.
fn capability_checks(
    task: &EvalTask,
    base_public_passed: bool,
    base_hidden_passed: Option<bool>,
    checks: &mut Vec<Check>,
) {
    if task.level.is_none_or(|l| l == Level::L0) {
        checks.push(Check::error(
            "capability_level",
            "capability tasks need a level in L1..L5",
        ));
    }

    let has_hidden = task.hidden.as_ref().is_some_and(|h| h.has_checks());
    if has_hidden {
        checks.push(Check::ok(
            "capability_hidden",
            "hidden verification present",
        ));
    } else {
        checks.push(Check::error(
            "capability_hidden",
            "capability task has no hidden verification",
        ));
    }

    let no_change = task.expects_no_change();
    match task.reference {
        None => checks.push(Check::error(
            "capability_reference",
            "capability task declares no reference (oracle_patch | validated_oracle)",
        )),
        Some(ReferenceKind::OraclePatch) if task.oracle_patch.is_none() => {
            checks.push(Check::error(
                "capability_reference",
                "reference = oracle_patch but no oracle_patch path",
            ))
        }
        Some(ReferenceKind::ValidatedOracle) if !no_change => checks.push(Check::error(
            "capability_reference",
            "reference = validated_oracle requires expected_outcome = no_change",
        )),
        Some(kind) => checks.push(Check::ok("capability_reference", format!("{kind:?}"))),
    }

    let Some(base_hidden_passed) = base_hidden_passed else {
        return;
    };
    let base_passed = base_public_passed && base_hidden_passed;
    match (no_change, base_passed) {
        (false, false) => checks.push(Check::ok(
            "capability_base_outcome",
            "unmodified fixture fails verification",
        )),
        (false, true) => checks.push(Check::error(
            "capability_base_outcome",
            "unmodified fixture already passes public + hidden (expected to fail)",
        )),
        (true, true) => checks.push(Check::ok(
            "capability_base_outcome",
            "unmodified fixture passes verification (no_change)",
        )),
        (true, false) => checks.push(Check::error(
            "capability_base_outcome",
            "no_change task: unmodified fixture must pass public + hidden",
        )),
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

    // The public fixture must be a strict subdirectory of the task dir, or
    // task.toml / hidden/ / the reference patch would be copied into the agent
    // workspace (#286).
    if task.fixture.kind == crate::task::FixtureKind::CopyDir {
        use std::path::Component;
        let rel = task.fixture.path.as_deref().unwrap_or("repo");
        let comps: Vec<Component> = Path::new(rel).components().collect();
        let strict_child = comps.iter().any(|c| matches!(c, Component::Normal(_)))
            && comps
                .iter()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
        if strict_child {
            checks.push(Check::ok(
                "canary_fixture_root",
                format!("fixture '{rel}' is a subdirectory of the task dir"),
            ));
        } else {
            checks.push(Check::error(
                "canary_fixture_root",
                format!(
                    "fixture path '{rel}' must be a subdirectory of the task dir \
                     (it would expose task.toml/hidden/ to the agent)"
                ),
            ));
        }
    }

    // A git fixture must be pinned to an exact commit on an https (or local
    // file) repo, and its sub-path must stay inside that checkout (#289).
    if task.fixture.kind == crate::task::FixtureKind::Git {
        use std::path::Component;
        let f = &task.fixture;
        let mut problems = Vec::new();
        match f.repo.as_deref() {
            Some(r) if r.starts_with("https://") || r.starts_with("file://") => {}
            Some(r) => problems.push(format!("repo {r:?} must be an https:// or file:// URL")),
            None => problems.push("missing `repo`".to_string()),
        }
        match f.rev.as_deref() {
            Some(r) if crate::fixture::is_full_sha(r) => {}
            Some(r) => problems.push(format!("rev {r:?} must be a full 40-hex commit SHA")),
            None => problems.push("missing `rev`".to_string()),
        }
        match f.path.as_deref() {
            Some(p) => {
                let comps: Vec<Component> = Path::new(p).components().collect();
                let relative = comps.iter().any(|c| matches!(c, Component::Normal(_)))
                    && comps
                        .iter()
                        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
                if !relative {
                    problems.push(format!("path {p:?} must be a relative sub-directory"));
                }
            }
            None => problems.push("missing `path`".to_string()),
        }
        if problems.is_empty() {
            checks.push(Check::ok(
                "git_fixture_pin",
                format!(
                    "fixture pinned to {}@{}",
                    f.repo.as_deref().unwrap_or_default(),
                    f.rev.as_deref().unwrap_or_default()
                ),
            ));
        } else {
            checks.push(Check::error(
                "git_fixture_pin",
                format!("git fixture: {}", problems.join("; ")),
            ));
        }
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
    flake_detect_with(task, repeat, &ValidationOptions::default()).await
}

/// [`flake_detect`] on `opts.isolation`.
pub async fn flake_detect_with(
    task: &EvalTask,
    repeat: usize,
    opts: &ValidationOptions,
) -> FlakeVerdict {
    let Ok(backend) = build_backend(&opts.isolation, opts.container_image.clone()) else {
        return FlakeVerdict::HarnessError;
    };
    let backend = backend.as_ref();
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
        let public = run_oracle_in(backend, &ws, &task.oracle, timeout).await;
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
                let hid = run_oracle_in(backend, &ws, &hidden.oracle, timeout).await;
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
    use crate::schema::{
        command_oracle, ExpectedOutcome, HiddenSpec, Level, ReferenceKind, TaskKind,
    };
    use crate::task::OracleSpec;

    fn task_in(dir: &Path, oracle: OracleSpec) -> EvalTask {
        let mut t = EvalTask::minimal("vt", "vt", "do it", oracle, dir);
        t.fixture = crate::task::FixtureSpec {
            kind: crate::task::FixtureKind::CopyDir,
            path: Some("repo".to_string()),
            ..Default::default()
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

    const FIX_PATCH: &str = "--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-bad\n+good\n";

    /// Build a capability task whose fixture holds `value.txt = bad` and whose hidden check wants `good`.
    fn capability_task(dir: &Path, with_hidden: bool, patch: Option<&str>) -> EvalTask {
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/value.txt"), "bad\n").unwrap();
        let mut task = task_in(dir, command_oracle(vec!["test -f value.txt".into()]));
        task.schema_version = 3;
        task.kind = Some(TaskKind::Capability);
        task.level = Some(Level::L1);
        if with_hidden {
            task.hidden = Some(HiddenSpec {
                fixture: None,
                oracle: command_oracle(vec!["grep -qx good value.txt".into()]),
            });
        }
        if let Some(p) = patch {
            std::fs::write(dir.join("oracle.patch"), p).unwrap();
            task.oracle_patch = Some("oracle.patch".to_string());
            task.reference = Some(ReferenceKind::OraclePatch);
        }
        task
    }

    fn failed(report: &ValidationReport, name: &str) -> bool {
        report
            .checks
            .iter()
            .any(|c| c.name == name && !c.passed && c.severity == Severity::Error)
    }

    /// A capability task with hidden checks and a patch that turns base-fail into pass validates cleanly.
    #[tokio::test]
    async fn capability_task_with_hidden_and_reference_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let task = capability_task(tmp.path(), true, Some(FIX_PATCH));
        let report = validate_task(&task).await;
        assert!(report.ok(), "{}", report.to_markdown());
        assert!(report
            .checks
            .iter()
            .any(|c| c.name == "capability_base_outcome" && c.passed));
    }

    /// A capability task without hidden verification is a validation error.
    #[tokio::test]
    async fn capability_task_without_hidden_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let task = capability_task(tmp.path(), false, Some(FIX_PATCH));
        let report = validate_task(&task).await;
        assert!(
            failed(&report, "capability_hidden"),
            "{}",
            report.to_markdown()
        );
        assert!(!report.ok());
    }

    /// A capability task without a declared reference is a validation error.
    #[tokio::test]
    async fn capability_task_without_reference_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let task = capability_task(tmp.path(), true, None);
        let report = validate_task(&task).await;
        assert!(
            failed(&report, "capability_reference"),
            "{}",
            report.to_markdown()
        );
    }

    /// A capability `change` task whose unmodified fixture already passes is a validation error.
    #[tokio::test]
    async fn capability_change_task_base_must_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let task = capability_task(tmp.path(), true, Some(FIX_PATCH));
        std::fs::write(tmp.path().join("repo/value.txt"), "good\n").unwrap();
        let report = validate_task(&task).await;
        assert!(
            failed(&report, "capability_base_outcome"),
            "{}",
            report.to_markdown()
        );
    }

    /// A reference patch that does not make hidden verification pass is a validation error.
    #[tokio::test]
    async fn capability_reference_must_pass_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        let wrong = "--- a/value.txt\n+++ b/value.txt\n@@ -1 +1 @@\n-bad\n+meh\n";
        let task = capability_task(tmp.path(), true, Some(wrong));
        let report = validate_task(&task).await;
        assert!(
            failed(&report, "oracle_patch_passes_hidden"),
            "{}",
            report.to_markdown()
        );
    }

    /// A capability `no_change` task with a validated oracle passes when the unmodified fixture passes.
    #[tokio::test]
    async fn capability_no_change_task_base_must_pass() {
        let tmp = tempfile::tempdir().unwrap();
        let mut task = capability_task(tmp.path(), true, None);
        task.reference = Some(ReferenceKind::ValidatedOracle);
        task.expected_outcome = ExpectedOutcome::NoChange;
        std::fs::write(tmp.path().join("repo/value.txt"), "good\n").unwrap();
        let report = validate_task(&task).await;
        assert!(report.ok(), "{}", report.to_markdown());

        // The same task with a failing fixture is rejected.
        std::fs::write(tmp.path().join("repo/value.txt"), "bad\n").unwrap();
        let report = validate_task(&task).await;
        assert!(
            failed(&report, "capability_base_outcome"),
            "{}",
            report.to_markdown()
        );
    }

    /// Harness tasks are exempt from the capability rules.
    #[tokio::test]
    async fn harness_task_skips_capability_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let mut task = capability_task(tmp.path(), false, None);
        task.kind = Some(TaskKind::Harness);
        task.level = Some(Level::L0);
        let report = validate_task(&task).await;
        assert!(report.ok(), "{}", report.to_markdown());
        assert!(!report
            .checks
            .iter()
            .any(|c| c.name.starts_with("capability_")));
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

    /// A copy_dir fixture rooted at (or above) the task dir is an error, because it would copy task.toml, hidden/, and the reference patch into the agent workspace; a `repo` subdirectory passes.
    #[test]
    fn canary_rejects_fixture_rooted_at_task_dir() {
        let tmp = tempfile::tempdir().unwrap();
        for bad in [".", "", "./", "..", "../other", "/abs/repo", "repo/../.."] {
            let mut t = task_in(tmp.path(), command_oracle(vec!["true".into()]));
            t.fixture.path = Some(bad.to_string());
            let mut checks = Vec::new();
            canary_checks(&t, &mut checks);
            assert!(
                checks
                    .iter()
                    .any(|c| c.name == "canary_fixture_root" && !c.passed),
                "fixture path {bad:?} must be rejected"
            );
        }
        let good = task_in(tmp.path(), command_oracle(vec!["true".into()]));
        let mut checks = Vec::new();
        canary_checks(&good, &mut checks);
        assert!(checks
            .iter()
            .any(|c| c.name == "canary_fixture_root" && c.passed));
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

#[cfg(test)]
mod git_fixture_validation_tests {
    use super::*;
    use crate::task::{FixtureKind, FixtureSpec};

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn git_task(repo: Option<&str>, rev: Option<&str>, path: Option<&str>) -> EvalTask {
        let mut t = EvalTask::minimal(
            "t",
            "t",
            "p",
            crate::task::OracleSpec::default(),
            std::path::PathBuf::from("."),
        );
        t.fixture = FixtureSpec {
            kind: FixtureKind::Git,
            path: path.map(str::to_string),
            repo: repo.map(str::to_string),
            rev: rev.map(str::to_string),
        };
        t
    }

    fn pin_check(t: &EvalTask) -> Option<bool> {
        let mut checks = Vec::new();
        canary_checks(t, &mut checks);
        checks
            .iter()
            .find(|c| c.name == "git_fixture_pin")
            .map(|c| c.passed)
    }

    // A git fixture pinned to a full SHA on an https repo with a relative sub-path passes.
    #[test]
    fn git_fixture_pin_accepts_full_sha() {
        let t = git_task(
            Some("https://github.com/mfaulk/crom-eval-fixtures"),
            Some(SHA),
            Some("l3-x"),
        );
        assert_eq!(pin_check(&t), Some(true));
    }

    // Branch names, short SHAs, missing fields, non-https repos, and escaping paths are rejected.
    #[test]
    fn git_fixture_pin_rejects_unpinned_or_unsafe() {
        let repo = Some("https://github.com/mfaulk/crom-eval-fixtures");
        for (r, rev, path) in [
            (repo, Some("main"), Some("l3-x")),
            (repo, Some("0123456"), Some("l3-x")),
            (repo, None, Some("l3-x")),
            (None, Some(SHA), Some("l3-x")),
            (Some("http://example.com/r"), Some(SHA), Some("l3-x")),
            (Some("ssh://git@example.com/r"), Some(SHA), Some("l3-x")),
            (repo, Some(SHA), Some("../x")),
            (repo, Some(SHA), Some("/abs")),
            (repo, Some(SHA), None),
        ] {
            assert_eq!(
                pin_check(&git_task(r, rev, path)),
                Some(false),
                "{r:?} {rev:?} {path:?} must be rejected"
            );
        }
    }
}

#[cfg(test)]
mod isolation_option_tests {
    use super::*;
    use crate::isolation::IsolationKind;

    fn trivial_task(dir: &Path) -> EvalTask {
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo/marker.txt"), "x\n").unwrap();
        let mut t = EvalTask::minimal(
            "iso",
            "iso",
            "p",
            crate::task::OracleSpec {
                kind: crate::task::OracleKind::Command,
                commands: vec!["test -f marker.txt".into()],
                ..Default::default()
            },
            dir.to_path_buf(),
        );
        t.timeout = std::time::Duration::from_secs(120);
        t
    }

    // An unavailable container backend (opt-in env unset) is reported as an `isolation` error, not a local fallback.
    #[tokio::test]
    async fn unavailable_backend_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let task = trivial_task(tmp.path());
        let opts = ValidationOptions {
            isolation: IsolationKind::Docker,
            container_image: None,
        };
        let report = temp_env::async_with_vars(
            [(crate::isolation::DOCKER_ENV, None::<&str>)],
            validate_task_with(&task, &opts),
        )
        .await;
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "isolation" && !c.passed),
            "{}",
            report.to_markdown()
        );
    }

    // The report names the backend that ran verification.
    #[tokio::test]
    async fn local_backend_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let task = trivial_task(tmp.path());
        let report = validate_task_with(&task, &ValidationOptions::default()).await;
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "isolation" && c.passed && c.detail.contains("local")),
            "{}",
            report.to_markdown()
        );
    }

    // Opt-in: with CROM_EVAL_PODMAN=1 and podman present, verification runs inside a container.
    #[tokio::test]
    #[ignore = "needs podman and the rust image; run with CROM_EVAL_PODMAN=1"]
    async fn podman_backend_runs_public_oracle() {
        let tmp = tempfile::tempdir().unwrap();
        let task = trivial_task(tmp.path());
        let opts = ValidationOptions {
            isolation: IsolationKind::Podman,
            container_image: Some("docker.io/library/alpine:3.20".into()),
        };
        let report = validate_task_with(&task, &opts).await;
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "isolation" && c.passed && c.detail.contains("podman")),
            "{}",
            report.to_markdown()
        );
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "public_verification_runs" && c.detail.starts_with("1/1")),
            "{}",
            report.to_markdown()
        );
    }
}
