//! Oracles: how the harness decides whether the agent succeeded.
//!
//! Four oracle families are supported, all derived from the task's
//! [`OracleSpec`](crate::task::OracleSpec):
//!
//! - **Command** — run shell commands; pass iff all exit zero.
//! - **Rust** — a convenience that expands to common cargo quality gates.
//! - **File assertion** — `file_contains` / `file_not_contains` / exists /
//!   missing checks against the post-run workspace.
//! - **Behavior** — checks evaluated from the structured event stream, never
//!   from model prose (`must_emit_plan`, `must_use_tool`, `must_run_command`,
//!   and their negations).

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use code_agent_sdk::SdkEvent;

use crate::fixture::read_workspace_file;
use crate::task::{AssertionSpec, BehaviorSpec, EvalTask};

/// Result of running a single command oracle command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResult {
    pub command: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub timed_out: bool,
}

/// Result of a single file assertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssertionResult {
    pub description: String,
    pub passed: bool,
    pub detail: String,
}

/// Result of a single behavior check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BehaviorResult {
    pub description: String,
    pub passed: bool,
    pub detail: String,
}

/// Aggregated outcome of evaluating all oracles for a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OracleReport {
    pub commands: Vec<CommandResult>,
    pub assertions: Vec<AssertionResult>,
    pub behavior: Vec<BehaviorResult>,
}

impl OracleReport {
    /// Number of discrete checks that passed.
    pub fn checks_passed(&self) -> usize {
        self.commands.iter().filter(|c| c.success).count()
            + self.assertions.iter().filter(|a| a.passed).count()
            + self.behavior.iter().filter(|b| b.passed).count()
    }

    /// Total number of discrete checks evaluated.
    pub fn checks_total(&self) -> usize {
        self.commands.len() + self.assertions.len() + self.behavior.len()
    }

    /// Whether every evaluated check passed. An oracle with no checks is
    /// considered passing (the harness still scored the run via efficiency and
    /// process quality).
    pub fn all_passed(&self) -> bool {
        self.commands.iter().all(|c| c.success)
            && self.assertions.iter().all(|a| a.passed)
            && self.behavior.iter().all(|b| b.passed)
    }

    /// Fraction of checks that passed in `[0, 1]`. Returns `1.0` when there are
    /// no checks.
    pub fn pass_fraction(&self) -> f64 {
        let total = self.checks_total();
        if total == 0 {
            return 1.0;
        }
        self.checks_passed() as f64 / total as f64
    }
}

/// Evaluate every oracle for `task` against the post-run `workspace_dir` and
/// the captured `events`. Uses the local backend (commands run directly).
pub async fn evaluate(task: &EvalTask, workspace_dir: &Path, events: &[SdkEvent]) -> OracleReport {
    let backend = crate::isolation::LocalBackend;
    let timeout = task_command_timeout(task);
    let limits = crate::isolation::CommandLimits {
        timeout,
        network: task.requires_network,
        ..Default::default()
    };
    evaluate_oracle(&task.oracle, workspace_dir, events, &backend, &limits).await
}

/// Per-command timeout for a task's oracle.
pub fn task_command_timeout(task: &EvalTask) -> Duration {
    Duration::from_secs(
        task.oracle
            .command_timeout_secs
            .unwrap_or_else(|| task.timeout.as_secs().max(1)),
    )
}

/// Evaluate an arbitrary [`OracleSpec`](crate::task::OracleSpec) against a
/// workspace and event stream, running commands through the supplied isolation
/// backend. This is the shared path used for both public and hidden verification.
pub async fn evaluate_oracle(
    oracle: &crate::task::OracleSpec,
    workspace_dir: &Path,
    events: &[SdkEvent],
    backend: &dyn crate::isolation::IsolationBackend,
    limits: &crate::isolation::CommandLimits,
) -> OracleReport {
    let mut commands = Vec::new();
    for cmd in oracle.primary_commands() {
        match backend.run(workspace_dir, &cmd, limits).await {
            Ok(outcome) => commands.push(outcome.result),
            Err(e) => commands.push(CommandResult {
                command: cmd,
                success: false,
                exit_code: None,
                stdout: String::new(),
                stderr: format!("isolation error: {e}"),
                duration_ms: 0,
                timed_out: false,
            }),
        }
    }

    let assertions = oracle
        .assertions
        .iter()
        .map(|a| evaluate_assertion(a, workspace_dir))
        .collect();

    let behavior = oracle
        .behavior
        .iter()
        .map(|b| evaluate_behavior(b, events))
        .collect();

    OracleReport {
        commands,
        assertions,
        behavior,
    }
}

/// Run a single shell command in `dir` with a timeout, capturing output.
pub async fn run_command(command: &str, dir: &Path, timeout: Duration) -> CommandResult {
    let start = std::time::Instant::now();
    let mut cmd = tokio::process::Command::new("sh");
    // `kill_on_drop` terminates the `sh` wrapper when the timeout future is
    // dropped. Note: `sh` does not forward the signal to its own children, so a
    // long-running child (e.g. cargo) may briefly outlive the timeout — see the
    // "known limitations" section of docs/eval.md.
    cmd.arg("-c")
        .arg(command)
        .current_dir(dir)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());

    // `output()` returns a future; drive it under a timeout below.
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => CommandResult {
            command: command.to_string(),
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: truncate(&String::from_utf8_lossy(&output.stdout)),
            stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
        },
        Ok(Err(e)) => CommandResult {
            command: command.to_string(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run command: {e}"),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
        },
        Err(_) => CommandResult {
            command: command.to_string(),
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs()),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: true,
        },
    }
}

/// Run a program with an explicit argv (no intermediate shell) in `dir` with a
/// timeout, capturing output. Used by container backends so multi-word
/// verification commands are not re-split by a wrapping `sh -c`.
pub async fn run_argv(
    program: &str,
    args: &[String],
    dir: &Path,
    timeout: Duration,
) -> CommandResult {
    let start = std::time::Instant::now();
    let display = format!("{program} {}", args.join(" "));
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(dir)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null());

    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => CommandResult {
            command: display,
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: truncate(&String::from_utf8_lossy(&output.stdout)),
            stderr: truncate(&String::from_utf8_lossy(&output.stderr)),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
        },
        Ok(Err(e)) => CommandResult {
            command: display,
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("failed to run command: {e}"),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: false,
        },
        Err(_) => CommandResult {
            command: display,
            success: false,
            exit_code: None,
            stdout: String::new(),
            stderr: format!("command timed out after {}s", timeout.as_secs()),
            duration_ms: start.elapsed().as_millis() as u64,
            timed_out: true,
        },
    }
}

const MAX_CAPTURE: usize = 64 * 1024;

fn truncate(s: &str) -> String {
    if s.len() <= MAX_CAPTURE {
        return s.to_string();
    }
    // Slice on a valid UTF-8 boundary: a fixed byte offset can land mid-codepoint
    // (common in cargo's box-drawing / quoted output) and panic.
    let end = (0..=MAX_CAPTURE)
        .rev()
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(0);
    let mut out = s[..end].to_string();
    out.push_str("\n... [truncated]");
    out
}

/// Evaluate a file assertion against the workspace.
pub fn evaluate_assertion(spec: &AssertionSpec, workspace_dir: &Path) -> AssertionResult {
    let description = spec.describe();
    let (passed, detail) = match spec {
        AssertionSpec::FileContains { path, text } => {
            match read_workspace_file(workspace_dir, path) {
                Some(content) if content.contains(text) => (true, "found".to_string()),
                Some(_) => (false, "text not present".to_string()),
                None => (false, "file missing".to_string()),
            }
        }
        AssertionSpec::FileNotContains { path, text } => {
            match read_workspace_file(workspace_dir, path) {
                Some(content) if content.contains(text) => (false, "text present".to_string()),
                Some(_) => (true, "absent as required".to_string()),
                None => (true, "file missing (vacuously true)".to_string()),
            }
        }
        AssertionSpec::FileExists { path } => {
            if workspace_dir.join(path).exists() {
                (true, "exists".to_string())
            } else {
                (false, "missing".to_string())
            }
        }
        AssertionSpec::FileMissing { path } => {
            if workspace_dir.join(path).exists() {
                (false, "present".to_string())
            } else {
                (true, "absent".to_string())
            }
        }
    };
    AssertionResult {
        description,
        passed,
        detail,
    }
}

/// Evaluate a behavior check from the structured event stream.
pub fn evaluate_behavior(spec: &BehaviorSpec, events: &[SdkEvent]) -> BehaviorResult {
    let description = spec.describe();
    let (passed, detail) = match spec {
        BehaviorSpec::MustEmitPlan => {
            let emitted = events.iter().any(|e| matches!(e, SdkEvent::PlanCreated(_)));
            (
                emitted,
                if emitted {
                    "plan emitted".to_string()
                } else {
                    "no PlanCreated event".to_string()
                },
            )
        }
        BehaviorSpec::MustUseTool { tool } => {
            let used = tool_calls(events).any(|name| name == tool);
            (
                used,
                if used {
                    format!("{tool} was called")
                } else {
                    format!("{tool} never called")
                },
            )
        }
        BehaviorSpec::MustNotUseTool { tool } => {
            let used = tool_calls(events).any(|name| name == tool);
            (
                !used,
                if used {
                    format!("{tool} was called")
                } else {
                    format!("{tool} not called")
                },
            )
        }
        BehaviorSpec::MustRunCommand { command_contains } => {
            let ran = bash_commands(events).any(|c| c.contains(command_contains));
            (
                ran,
                if ran {
                    format!("ran a command containing {command_contains:?}")
                } else {
                    format!("no command contained {command_contains:?}")
                },
            )
        }
        BehaviorSpec::MustNotRunCommand { command_contains } => {
            let ran = bash_commands(events).any(|c| c.contains(command_contains));
            (
                !ran,
                if ran {
                    format!("ran a forbidden command containing {command_contains:?}")
                } else {
                    format!("no command contained {command_contains:?}")
                },
            )
        }
    };
    BehaviorResult {
        description,
        passed,
        detail,
    }
}

/// Iterate over the tool names of every `ToolCallStarted` event.
pub fn tool_calls(events: &[SdkEvent]) -> impl Iterator<Item = &str> {
    events.iter().filter_map(|e| match e {
        SdkEvent::ToolCallStarted(ev) => Some(ev.tool_name.as_str()),
        _ => None,
    })
}

/// Iterate over the command text of every Bash `ToolCallStarted` event.
pub fn bash_commands(events: &[SdkEvent]) -> impl Iterator<Item = String> + '_ {
    events.iter().filter_map(|e| match e {
        SdkEvent::ToolCallStarted(ev) if ev.tool_name == "Bash" => ev
            .input
            .get("command")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_sdk::event::ToolCallStartedEvent;
    use code_agent_sdk::RunId;

    fn tool_started(tool: &str, input: serde_json::Value) -> SdkEvent {
        SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: RunId::new(),
            tool_name: tool.to_string(),
            tool_use_id: "t1".to_string(),
            input,
        })
    }

    #[tokio::test]
    async fn command_success_and_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let ok = run_command("exit 0", tmp.path(), Duration::from_secs(10)).await;
        assert!(ok.success);
        assert_eq!(ok.exit_code, Some(0));
        let bad = run_command("exit 3", tmp.path(), Duration::from_secs(10)).await;
        assert!(!bad.success);
        assert_eq!(bad.exit_code, Some(3));
    }

    #[tokio::test]
    async fn run_argv_preserves_multiword_args() {
        let tmp = tempfile::tempdir().unwrap();
        // If args were re-split by a wrapping shell, this would print wrong
        // output or fail. printf joins its args with the format spec.
        let res = run_argv(
            "printf",
            &["%s-%s".into(), "cargo test".into(), "--all-targets".into()],
            tmp.path(),
            Duration::from_secs(10),
        )
        .await;
        assert!(res.success);
        assert_eq!(res.stdout, "cargo test---all-targets");
    }

    #[tokio::test]
    async fn command_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let res = run_command("sleep 5", tmp.path(), Duration::from_millis(200)).await;
        assert!(res.timed_out);
        assert!(!res.success);
    }

    #[test]
    fn file_assertions() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "pub fn parse_args() {}").unwrap();

        let contains = evaluate_assertion(
            &AssertionSpec::FileContains {
                path: "a.txt".to_string(),
                text: "parse_args".to_string(),
            },
            tmp.path(),
        );
        assert!(contains.passed);

        let not_contains = evaluate_assertion(
            &AssertionSpec::FileNotContains {
                path: "a.txt".to_string(),
                text: "unwrap()".to_string(),
            },
            tmp.path(),
        );
        assert!(not_contains.passed);

        let missing = evaluate_assertion(
            &AssertionSpec::FileMissing {
                path: "nope.txt".to_string(),
            },
            tmp.path(),
        );
        assert!(missing.passed);

        let exists = evaluate_assertion(
            &AssertionSpec::FileExists {
                path: "a.txt".to_string(),
            },
            tmp.path(),
        );
        assert!(exists.passed);
    }

    #[test]
    fn behavior_from_synthetic_events() {
        let events = vec![
            tool_started("Read", serde_json::json!({"file_path": "src/lib.rs"})),
            tool_started(
                "Bash",
                serde_json::json!({"command": "cargo test --all-targets"}),
            ),
        ];

        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustUseTool {
                    tool: "Read".into()
                },
                &events
            )
            .passed
        );
        assert!(
            !evaluate_behavior(
                &BehaviorSpec::MustUseTool {
                    tool: "Edit".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustNotUseTool {
                    tool: "Edit".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustRunCommand {
                    command_contains: "cargo test".into()
                },
                &events
            )
            .passed
        );
        assert!(
            evaluate_behavior(
                &BehaviorSpec::MustNotRunCommand {
                    command_contains: "rm -rf".into()
                },
                &events
            )
            .passed
        );
        assert!(
            !evaluate_behavior(&BehaviorSpec::MustEmitPlan, &events).passed,
            "no plan event present"
        );
    }

    #[test]
    fn truncate_handles_multibyte_boundaries() {
        // A string of multi-byte chars longer than the cap must not panic and
        // must remain valid UTF-8 after truncation.
        let s = "é".repeat(MAX_CAPTURE); // 2 bytes each → 2*MAX_CAPTURE bytes
        let out = truncate(&s);
        assert!(out.len() <= MAX_CAPTURE + "\n... [truncated]".len());
        assert!(out.ends_with("[truncated]"));
        // No panic and the prefix is valid UTF-8 (String guarantees this).
        assert!(out.starts_with('é'));
    }

    #[test]
    fn report_pass_fraction() {
        let report = OracleReport {
            commands: vec![CommandResult {
                command: "x".into(),
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
            }],
            assertions: vec![AssertionResult {
                description: "a".into(),
                passed: false,
                detail: String::new(),
            }],
            behavior: vec![],
        };
        assert_eq!(report.checks_total(), 2);
        assert_eq!(report.checks_passed(), 1);
        assert!((report.pass_fraction() - 0.5).abs() < 1e-9);
        assert!(!report.all_passed());
    }
}
