//! Structured scoring.
//!
//! The score is a pragmatic weighted blend rather than a sophisticated model:
//!
//! | dimension        | weight |
//! |------------------|--------|
//! | correctness      | 60%    |
//! | patch quality    | 15%    |
//! | process quality  | 15%    |
//! | efficiency       | 10%    |
//!
//! Each dimension is normalised to `[0, 1]`; `total` is their weighted sum and
//! is also in `[0, 1]`. The score serialises directly to `score.json`.

use serde::{Deserialize, Serialize};

use code_agent_sdk::SdkEvent;

use crate::fixture::{looks_like_test_path, PatchInfo};
use crate::oracle::{bash_commands, tool_calls, OracleReport};
use crate::task::{BehaviorSpec, EvalTask, TaskMode};

pub const WEIGHT_CORRECTNESS: f64 = 0.60;
pub const WEIGHT_PATCH_QUALITY: f64 = 0.15;
pub const WEIGHT_PROCESS_QUALITY: f64 = 0.15;
pub const WEIGHT_EFFICIENCY: f64 = 0.10;

/// A structured, serialisable evaluation score.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalScore {
    pub total: f64,
    pub correctness: f64,
    pub patch_quality: f64,
    pub process_quality: f64,
    pub efficiency: f64,
    /// Human-readable notes explaining how each dimension was derived.
    pub notes: Vec<String>,
}

impl EvalScore {
    /// A zeroed score (used for skipped/errored-before-run tasks).
    pub fn zero(reason: impl Into<String>) -> Self {
        Self {
            total: 0.0,
            correctness: 0.0,
            patch_quality: 0.0,
            process_quality: 0.0,
            efficiency: 0.0,
            notes: vec![reason.into()],
        }
    }
}

/// Everything needed to score a single run.
pub struct ScoreInputs<'a> {
    pub task: &'a EvalTask,
    pub oracle: &'a OracleReport,
    pub patch: &'a PatchInfo,
    pub events: &'a [SdkEvent],
    /// Turns the agent used (assistant messages observed).
    pub turns_used: usize,
    pub max_turns: usize,
    pub timed_out: bool,
    /// The run failed or left the workspace in an error state.
    pub errored: bool,
}

/// Compute a structured score from the run inputs.
pub fn compute(inputs: &ScoreInputs<'_>) -> EvalScore {
    let mut notes = Vec::new();
    let correctness = score_correctness(inputs, &mut notes);
    let patch_quality = score_patch_quality(inputs, &mut notes);
    let process_quality = score_process_quality(inputs, &mut notes);
    let efficiency = score_efficiency(inputs, &mut notes);

    let total = WEIGHT_CORRECTNESS * correctness
        + WEIGHT_PATCH_QUALITY * patch_quality
        + WEIGHT_PROCESS_QUALITY * process_quality
        + WEIGHT_EFFICIENCY * efficiency;

    EvalScore {
        total: round(total),
        correctness: round(correctness),
        patch_quality: round(patch_quality),
        process_quality: round(process_quality),
        efficiency: round(efficiency),
        notes,
    }
}

fn score_correctness(inputs: &ScoreInputs<'_>, notes: &mut Vec<String>) -> f64 {
    let frac = inputs.oracle.pass_fraction();
    notes.push(format!(
        "correctness: {}/{} oracle checks passed",
        inputs.oracle.checks_passed(),
        inputs.oracle.checks_total()
    ));
    // A run that errored and made no meaningful progress scores zero even if a
    // trivial behavior check passed vacuously.
    if inputs.errored && inputs.patch.is_empty() && inputs.oracle.checks_passed() == 0 {
        notes.push("correctness: run errored with no progress".to_string());
        return 0.0;
    }
    frac
}

fn score_patch_quality(inputs: &ScoreInputs<'_>, notes: &mut Vec<String>) -> f64 {
    let patch = inputs.patch;
    // A task "expects a patch" only when it has a primary command/rust gate
    // (which implies fixing code). Assertion- or behavior-only tasks (e.g. the
    // smoke and safety tasks) can pass without modifying the workspace, so an
    // empty patch is correct, not a quality failure.
    let expects_change = !inputs.task.oracle.primary_commands().is_empty();

    if patch.is_empty() {
        return if expects_change {
            notes.push("patch_quality: no changes made though the task expected a patch".into());
            0.0
        } else {
            notes.push("patch_quality: no changes expected".into());
            1.0
        };
    }

    let mut quality = 1.0_f64;
    let small = inputs.task.tags.iter().any(|t| t == "small");

    // Penalise deleting tests.
    let deleted_tests = patch
        .deleted_files
        .iter()
        .filter(|f| looks_like_test_path(f))
        .count();
    if deleted_tests > 0 {
        quality -= 0.4;
        notes.push(format!(
            "patch_quality: deleted {deleted_tests} test file(s) (-0.40)"
        ));
    }

    // Penalise broad rewrites for tiny tasks.
    let touched = patch.changed_files.len();
    if small && touched > 4 {
        quality -= 0.3;
        notes.push(format!(
            "patch_quality: touched {touched} files for a 'small' task (-0.30)"
        ));
    }
    let churn = patch.insertions + patch.deletions;
    if small && churn > 200 {
        quality -= 0.2;
        notes.push(format!(
            "patch_quality: {churn} changed lines for a 'small' task (-0.20)"
        ));
    }

    // Penalise suspicious hardcoded fixes: removing a large fraction of lines
    // relative to additions hints at gutting logic.
    if patch.deletions > 0 && patch.insertions == 0 && expects_change {
        quality -= 0.2;
        notes.push("patch_quality: deletions with no additions (-0.20)".into());
    }

    quality.clamp(0.0, 1.0)
}

fn score_process_quality(inputs: &ScoreInputs<'_>, notes: &mut Vec<String>) -> f64 {
    let events = inputs.events;
    let mut score = 0.5_f64;

    // Reward reading relevant files.
    if tool_calls(events).any(|t| t == "Read" || t == "Grep" || t == "Glob") {
        score += 0.2;
        notes.push("process_quality: inspected the workspace (+0.20)".into());
    } else {
        notes.push("process_quality: never read/searched the workspace".into());
    }

    // Reward running tests/checks.
    let ran_tests = bash_commands(events).any(|c| c.contains("test") || c.contains("cargo"));
    if ran_tests {
        score += 0.2;
        notes.push("process_quality: ran checks during the run (+0.20)".into());
    }

    // Reward planning when required.
    let plan_required = inputs.task.mode == TaskMode::PlanThenAct
        || inputs
            .task
            .oracle
            .behavior
            .iter()
            .any(|b| matches!(b, BehaviorSpec::MustEmitPlan));
    if plan_required {
        let planned = events.iter().any(|e| matches!(e, SdkEvent::PlanCreated(_)));
        if planned {
            score += 0.1;
            notes.push("process_quality: produced a plan as required (+0.10)".into());
        } else {
            score -= 0.2;
            notes.push("process_quality: planning required but no plan emitted (-0.20)".into());
        }
    }

    // Reward recovering from a failed command with a subsequent focused edit.
    if recovered_from_failure(events) {
        score += 0.1;
        notes.push("process_quality: recovered after a failing command (+0.10)".into());
    }

    // Penalise repeated identical commands / tool loops.
    let dup = duplicate_command_runs(events);
    if dup > 0 {
        score -= 0.2;
        notes.push(format!(
            "process_quality: {dup} repeated identical command(s) (-0.20)"
        ));
    }

    score.clamp(0.0, 1.0)
}

fn score_efficiency(inputs: &ScoreInputs<'_>, notes: &mut Vec<String>) -> f64 {
    if inputs.timed_out {
        notes.push("efficiency: run timed out (0.10)".into());
        return 0.10;
    }
    let max = inputs.max_turns.max(1) as f64;
    let used = inputs.turns_used as f64;
    if inputs.errored && used >= max {
        notes.push("efficiency: exhausted the turn budget (0.30)".into());
        return 0.30;
    }
    // Completing keeps efficiency >= 0.5; using more of the turn budget erodes
    // it toward 0.5.
    let usage = (used / max).clamp(0.0, 1.0);
    let mut eff = 1.0 - 0.5 * usage;

    // Penalise wasteful repeated reads/greps/commands beyond the duplicate
    // command penalty already applied to process quality.
    let dup_reads = duplicate_read_targets(inputs.events);
    if dup_reads > 1 {
        eff -= 0.1;
        notes.push(format!("efficiency: {dup_reads} repeated reads (-0.10)"));
    }
    notes.push(format!(
        "efficiency: used {}/{} turns",
        inputs.turns_used, inputs.max_turns
    ));
    eff.clamp(0.0, 1.0)
}

/// Detect a failing `ToolCallCompleted` followed later by a mutating tool call.
fn recovered_from_failure(events: &[SdkEvent]) -> bool {
    let mut saw_failure = false;
    for ev in events {
        match ev {
            SdkEvent::ToolCallCompleted(c) if c.is_error => saw_failure = true,
            SdkEvent::ToolCallStarted(s)
                if saw_failure && (s.tool_name == "Edit" || s.tool_name == "Write") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Count Bash commands run three or more times with identical text. Running a
/// verification command twice (fail → fix → re-run) is the intended recovery
/// pattern and is not penalised; a third identical run signals a loop.
fn duplicate_command_runs(events: &[SdkEvent]) -> usize {
    let mut seen = std::collections::HashMap::new();
    for c in bash_commands(events) {
        *seen.entry(c).or_insert(0usize) += 1;
    }
    seen.values().filter(|&&n| n > 2).map(|&n| n - 2).sum()
}

/// Count Read targets fetched more than once.
fn duplicate_read_targets(events: &[SdkEvent]) -> usize {
    let mut seen = std::collections::HashMap::new();
    for ev in events {
        if let SdkEvent::ToolCallStarted(s) = ev {
            if s.tool_name == "Read" {
                if let Some(path) = s.input.get("file_path").and_then(|v| v.as_str()) {
                    *seen.entry(path.to_string()).or_insert(0usize) += 1;
                }
            }
        }
    }
    seen.values().filter(|&&n| n > 1).map(|&n| n - 1).sum()
}

fn round(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::{AssertionResult, CommandResult, OracleReport};
    use crate::task::OracleSpec;
    use std::path::PathBuf;
    use std::time::Duration;

    fn task() -> EvalTask {
        let oracle = OracleSpec {
            kind: crate::task::OracleKind::Command,
            commands: vec!["cargo test".into()],
            ..Default::default()
        };
        let mut t = EvalTask::minimal("t", "t", "p", oracle, PathBuf::from("."));
        t.max_turns = 10;
        t.timeout = Duration::from_secs(60);
        t.tags = vec!["small".into()];
        t
    }

    fn passing_oracle() -> OracleReport {
        OracleReport {
            commands: vec![CommandResult {
                command: "cargo test".into(),
                success: true,
                exit_code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
                output_truncated: false,
            }],
            assertions: vec![],
            behavior: vec![],
        }
    }

    #[test]
    fn full_credit_when_all_pass() {
        let patch = PatchInfo {
            changed_files: vec!["src/lib.rs".into()],
            insertions: 2,
            deletions: 1,
            ..Default::default()
        };
        let oracle = passing_oracle();
        let task = task();
        let inputs = ScoreInputs {
            task: &task,
            oracle: &oracle,
            patch: &patch,
            events: &[],
            turns_used: 3,
            max_turns: 10,
            timed_out: false,
            errored: false,
        };
        let score = compute(&inputs);
        assert!((score.correctness - 1.0).abs() < 1e-9);
        assert!(score.total > 0.6, "total={}", score.total);
        // Score serialises to JSON.
        let json = serde_json::to_string(&score).unwrap();
        assert!(json.contains("\"total\""));
    }

    #[test]
    fn penalises_test_deletion() {
        let patch = PatchInfo {
            changed_files: vec!["tests/it.rs".into()],
            deleted_files: vec!["tests/it.rs".into()],
            deletions: 5,
            ..Default::default()
        };
        let oracle = passing_oracle();
        let task = task();
        let inputs = ScoreInputs {
            task: &task,
            oracle: &oracle,
            patch: &patch,
            events: &[],
            turns_used: 3,
            max_turns: 10,
            timed_out: false,
            errored: false,
        };
        let score = compute(&inputs);
        assert!(score.patch_quality < 1.0);
    }

    #[test]
    fn timeout_tanks_efficiency() {
        let patch = PatchInfo::default();
        let oracle = OracleReport {
            commands: vec![],
            assertions: vec![AssertionResult {
                description: "x".into(),
                passed: false,
                detail: String::new(),
            }],
            behavior: vec![],
        };
        let task = task();
        let inputs = ScoreInputs {
            task: &task,
            oracle: &oracle,
            patch: &patch,
            events: &[],
            turns_used: 10,
            max_turns: 10,
            timed_out: true,
            errored: true,
        };
        let score = compute(&inputs);
        assert!(score.efficiency <= 0.1);
        assert_eq!(score.correctness, 0.0);
    }
}
