//! Bounded repair controller, state, and brief generation (M16H/M16I).
//!
//! The repair loop turns a failed validation command into a targeted,
//! bounded retry: it records the failure, detects repeated identical failures,
//! enforces an attempt budget, and produces a [`RepairBrief`] that is injected
//! back into the agent loop so the model can make a focused fix.
//!
//! All behavior is governed by [`RepairPolicy`] and can be fully disabled.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::diagnostics::{
    focused_validation_plan, CommandDiagnostic, DiagnosticFingerprint, DiagnosticKind,
    FailureHistory, RepeatStatus,
};

/// Static policy that bounds and configures the repair loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepairPolicy {
    pub enabled: bool,
    pub max_attempts: u32,
    pub max_same_failure_repeats: u32,
    pub require_validation_after_mutation: bool,
    pub prefer_focused_validation: bool,
    pub stop_on_repeated_identical_failure: bool,
}

impl Default for RepairPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_attempts: 3,
            max_same_failure_repeats: 2,
            require_validation_after_mutation: true,
            prefer_focused_validation: true,
            stop_on_repeated_identical_failure: true,
        }
    }
}

impl RepairPolicy {
    /// A disabled policy: the controller becomes a no-op.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// Mutable repair-loop state for a single agent run. Serializable so it can be
/// persisted into session/eval artifacts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairState {
    pub enabled: bool,
    pub attempts: u32,
    pub max_attempts: u32,
    pub failures: Vec<CommandDiagnostic>,
    pub repeated_fingerprints: HashMap<String, u32>,
    pub last_successful_validation: Option<String>,
    pub last_failed_validation: Option<CommandDiagnostic>,
    pub repaired_failures: Vec<DiagnosticFingerprint>,
    /// True once a validation command has succeeded after the most recent
    /// mutation. False after a mutation with no subsequent successful
    /// validation.
    pub validation_after_last_mutation: bool,
    /// True when a file-mutating tool has run since the last successful
    /// validation (i.e. validation is "needed").
    pub validation_needed: bool,
}

impl RepairState {
    pub fn new(policy: &RepairPolicy) -> Self {
        Self {
            enabled: policy.enabled,
            attempts: 0,
            max_attempts: policy.max_attempts,
            failures: Vec::new(),
            repeated_fingerprints: HashMap::new(),
            last_successful_validation: None,
            last_failed_validation: None,
            repaired_failures: Vec::new(),
            validation_after_last_mutation: true,
            validation_needed: false,
        }
    }

    /// The maximum number of times any single failure fingerprint has been
    /// observed (1 means "seen once, never repeated").
    pub fn max_repeats_observed(&self) -> u32 {
        self.repeated_fingerprints
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

/// The decision the controller reaches after observing a validation outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RepairDecision {
    /// Validation passed; nothing to do. Carries the command that succeeded.
    Cleared { command: String },
    /// A repair brief should be injected for the agent to act on.
    Repair { brief: RepairBrief },
    /// Stop repairing: attempt budget exhausted or the same failure repeated
    /// too many times. Carries a human-readable reason and the final brief.
    Stop {
        reason: StopReason,
        brief: RepairBrief,
    },
    /// Repair is disabled; the controller took no action.
    Disabled,
}

/// Why the repair loop stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    MaxAttemptsExceeded,
    RepeatedIdenticalFailure,
}

/// The bounded repair controller. Owns the [`RepairState`] and a
/// [`FailureHistory`] for fingerprint repeat detection.
#[derive(Debug, Clone)]
pub struct RepairController {
    pub policy: RepairPolicy,
    pub state: RepairState,
    history: FailureHistory,
}

impl RepairController {
    pub fn new(policy: RepairPolicy) -> Self {
        let state = RepairState::new(&policy);
        Self {
            policy,
            state,
            history: FailureHistory::new(),
        }
    }

    /// Record that a mutating tool (Write/Edit/fmt/etc.) ran. Sets the
    /// validation-needed flag and clears the "validated after mutation" flag.
    pub fn on_mutation(&mut self) {
        self.state.validation_needed = true;
        self.state.validation_after_last_mutation = false;
    }

    /// Whether validation is currently required before the agent may finalize.
    pub fn validation_required(&self) -> bool {
        self.policy.require_validation_after_mutation && self.state.validation_needed
    }

    /// Observe the outcome of a validation command and decide what to do.
    ///
    /// On success: clears the validation-needed flag and returns
    /// [`RepairDecision::Cleared`]. On failure: records the failure, detects
    /// repeats, bumps the attempt counter, and returns either a `Repair` brief
    /// or a `Stop` decision when a bound is hit.
    pub fn observe_validation(&mut self, diag: CommandDiagnostic) -> RepairDecision {
        if !self.policy.enabled {
            // Even when disabled, track validation-needed so the
            // completion policy still works.
            if !diag.is_failure() {
                self.state.validation_needed = false;
                self.state.validation_after_last_mutation = true;
                self.state.last_successful_validation = Some(diag.command.clone());
            }
            return RepairDecision::Disabled;
        }

        if !diag.is_failure() {
            self.state.validation_needed = false;
            self.state.validation_after_last_mutation = true;
            self.state.last_successful_validation = Some(diag.command.clone());
            // Anything we were repairing is now resolved.
            if let Some(failed) = self.state.last_failed_validation.take() {
                if !self.state.repaired_failures.contains(&failed.fingerprint) {
                    self.state.repaired_failures.push(failed.fingerprint);
                }
            }
            return RepairDecision::Cleared {
                command: diag.command,
            };
        }

        // Failure path.
        self.state.attempts += 1;
        let repeat = self.history.record(diag.clone());
        let repeat_count = repeat.count();
        *self
            .state
            .repeated_fingerprints
            .entry(diag.fingerprint.short_hash())
            .or_insert(0) += 1;
        self.state.last_failed_validation = Some(diag.clone());
        self.state.failures.push(diag.clone());

        let brief = self.build_brief(&diag, repeat);

        // Stop conditions.
        if self.policy.stop_on_repeated_identical_failure
            && matches!(repeat, RepeatStatus::Repeated { .. })
            && repeat_count >= self.policy.max_same_failure_repeats
        {
            return RepairDecision::Stop {
                reason: StopReason::RepeatedIdenticalFailure,
                brief,
            };
        }
        if self.state.attempts >= self.policy.max_attempts {
            return RepairDecision::Stop {
                reason: StopReason::MaxAttemptsExceeded,
                brief,
            };
        }

        RepairDecision::Repair { brief }
    }

    fn build_brief(&self, diag: &CommandDiagnostic, repeat: RepeatStatus) -> RepairBrief {
        let plan = if self.policy.prefer_focused_validation {
            focused_validation_plan(diag)
        } else {
            None
        };
        let focused_commands = plan.map(|p| p.focused_commands).unwrap_or_default();

        let failing_tests = diag
            .summary
            .failing_tests
            .iter()
            .map(|ft| ft.name.clone())
            .collect();

        let repeated = repeat.is_repeat();
        let repeat_count = repeat.count();

        let mut instructions = vec![
            "Inspect the reported file/span before editing.".to_string(),
            "Make the smallest targeted change that addresses this failure.".to_string(),
            "Do not change unrelated files.".to_string(),
        ];
        if !focused_commands.is_empty() {
            instructions.push(format!(
                "Re-run focused validation first if possible: {}",
                focused_commands.join(" ; ")
            ));
            instructions
                .push("Then run broader validation if the focused check passes.".to_string());
        }
        if let Some(hint) = &diag.summary.repair_hint {
            instructions.push(hint.clone());
        }
        if repeated {
            instructions.insert(
                0,
                format!(
                    "The same failure has repeated {repeat_count} times. Do not re-run the same \
                     command without changing the root cause. Re-read the diagnostic, inspect the \
                     exact file/span, and make one targeted change."
                ),
            );
        }

        let cfg = crate::diagnostics::CompactionConfig::default();
        let diagnostic_excerpt = crate::diagnostics::compact(diag, &cfg);

        RepairBrief {
            attempt_index: self.state.attempts,
            max_attempts: self.policy.max_attempts,
            command: diag.command.clone(),
            primary_failure: diag.summary.short_message.clone(),
            failing_tests,
            affected_files: diag.summary.affected_files.clone(),
            diagnostic_excerpt,
            focused_commands,
            repeated,
            repeat_count,
            instructions,
        }
    }
}

/// A standard, provider-independent repair brief injected back into the agent
/// loop after a failed validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairBrief {
    pub attempt_index: u32,
    pub max_attempts: u32,
    pub command: String,
    pub primary_failure: String,
    pub failing_tests: Vec<String>,
    pub affected_files: Vec<PathBuf>,
    pub diagnostic_excerpt: String,
    pub focused_commands: Vec<String>,
    pub repeated: bool,
    pub repeat_count: u32,
    pub instructions: Vec<String>,
}

impl RepairBrief {
    /// Render the brief to concise text suitable for a tool result / system
    /// message that steers the model.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str("Validation failed after your last edit.\n\n");
        s.push_str("Command:\n");
        s.push_str(&self.command);
        s.push_str("\n\n");

        if !self.primary_failure.is_empty() {
            s.push_str("Primary failure:\n");
            s.push_str(&self.primary_failure);
            s.push_str("\n\n");
        }

        if !self.failing_tests.is_empty() {
            s.push_str("Failing tests:\n");
            for t in &self.failing_tests {
                s.push_str(&format!("- {t}\n"));
            }
            s.push('\n');
        }

        if !self.affected_files.is_empty() {
            s.push_str("Affected files:\n");
            for f in &self.affected_files {
                s.push_str(&format!("- {}\n", f.display()));
            }
            s.push('\n');
        }

        s.push_str("Relevant excerpt:\n");
        s.push_str(&self.diagnostic_excerpt);
        s.push_str("\n\n");

        s.push_str(&format!(
            "This is repair attempt {} of {}.\n\n",
            self.attempt_index, self.max_attempts
        ));

        if self.repeated {
            s.push_str(&format!(
                "WARNING: the same failure has repeated {} times. Do not re-run the same command \
                 without changing the root cause.\n\n",
                self.repeat_count
            ));
        }

        s.push_str("Instructions:\n");
        for i in &self.instructions {
            s.push_str(&format!("- {i}\n"));
        }
        s
    }

    /// The primary diagnostic kind label, derived from the primary failure
    /// line (best-effort; for artifacts use the diagnostic directly).
    pub fn focused_summary(&self) -> &str {
        self.primary_failure.split([':', ' ']).next().unwrap_or("")
    }
}

/// A serializable, per-attempt record for the `repair.json` artifact (M16L).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairAttemptRecord {
    pub index: u32,
    pub trigger_command: String,
    pub failure_kind: DiagnosticKind,
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_line: Option<u32>,
    pub focused_commands: Vec<String>,
    pub repeated: bool,
    pub recovered: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{parse_command_diagnostic, CommandDiagnosticInput};

    fn fail(stderr: &str) -> CommandDiagnostic {
        parse_command_diagnostic(CommandDiagnosticInput {
            command: "cargo test".into(),
            cwd: "/tmp/work".into(),
            exit_code: Some(101),
            timed_out: false,
            stdout: String::new(),
            stderr: stderr.into(),
            duration_ms: Some(5),
        })
    }

    fn pass() -> CommandDiagnostic {
        parse_command_diagnostic(CommandDiagnosticInput {
            command: "cargo test".into(),
            cwd: "/tmp/work".into(),
            exit_code: Some(0),
            timed_out: false,
            stdout: "test result: ok. 5 passed; 0 failed\n".into(),
            stderr: String::new(),
            duration_ms: Some(5),
        })
    }

    const E0308: &str = "error[E0308]: mismatched types\n  --> src/config.rs:74:21\n";

    #[test]
    fn first_failure_creates_brief() {
        let mut c = RepairController::new(RepairPolicy::default());
        let d = c.observe_validation(fail(E0308));
        match d {
            RepairDecision::Repair { brief } => {
                assert_eq!(brief.attempt_index, 1);
                assert!(!brief.repeated);
                assert!(brief.render().contains("repair attempt 1 of 3"));
            }
            other => panic!("expected Repair, got {other:?}"),
        }
    }

    #[test]
    fn same_failure_increments_and_stops() {
        let mut c = RepairController::new(RepairPolicy {
            max_attempts: 10,
            max_same_failure_repeats: 2,
            ..RepairPolicy::default()
        });
        assert!(matches!(
            c.observe_validation(fail(E0308)),
            RepairDecision::Repair { .. }
        ));
        let d = c.observe_validation(fail(E0308));
        match d {
            RepairDecision::Stop { reason, brief } => {
                assert_eq!(reason, StopReason::RepeatedIdenticalFailure);
                assert!(brief.repeated);
                assert_eq!(brief.repeat_count, 2);
            }
            other => panic!("expected Stop, got {other:?}"),
        }
    }

    #[test]
    fn different_failure_does_not_count_as_repeat() {
        let mut c = RepairController::new(RepairPolicy {
            max_attempts: 10,
            ..RepairPolicy::default()
        });
        c.observe_validation(fail(E0308));
        let d = c.observe_validation(fail(
            "error[E0432]: unresolved import `foo`\n  --> src/lib.rs:1:5\n",
        ));
        match d {
            RepairDecision::Repair { brief } => assert!(!brief.repeated),
            other => panic!("expected Repair, got {other:?}"),
        }
    }

    #[test]
    fn max_attempts_stops() {
        let mut c = RepairController::new(RepairPolicy {
            max_attempts: 2,
            max_same_failure_repeats: 99,
            stop_on_repeated_identical_failure: false,
            ..RepairPolicy::default()
        });
        assert!(matches!(
            c.observe_validation(fail(E0308)),
            RepairDecision::Repair { .. }
        ));
        let d = c.observe_validation(fail(
            "error[E0277]: trait bound not satisfied\n  --> a.rs:1:1\n",
        ));
        assert!(matches!(
            d,
            RepairDecision::Stop {
                reason: StopReason::MaxAttemptsExceeded,
                ..
            }
        ));
    }

    #[test]
    fn success_clears_validation_flag() {
        let mut c = RepairController::new(RepairPolicy::default());
        c.on_mutation();
        assert!(c.validation_required());
        let d = c.observe_validation(pass());
        assert!(matches!(d, RepairDecision::Cleared { .. }));
        assert!(!c.validation_required());
        assert!(c.state.validation_after_last_mutation);
    }

    #[test]
    fn mutation_sets_validation_needed() {
        let mut c = RepairController::new(RepairPolicy::default());
        assert!(!c.validation_required());
        c.on_mutation();
        assert!(c.validation_required());
    }

    #[test]
    fn disabled_means_no_brief() {
        let mut c = RepairController::new(RepairPolicy::disabled());
        let d = c.observe_validation(fail(E0308));
        assert!(matches!(d, RepairDecision::Disabled));
    }

    #[test]
    fn focused_command_suggested_for_test_failure() {
        let mut c = RepairController::new(RepairPolicy::default());
        let stderr = "---- cli::json_format stdout ----\nthread 'cli::json_format' panicked at src/cli.rs:3:1:\nassertion `left == right` failed\n  left: 1\n right: 2\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n";
        let d = c.observe_validation(fail(stderr));
        if let RepairDecision::Repair { brief } = d {
            assert!(brief
                .focused_commands
                .iter()
                .any(|c| c.contains("cargo test cli::json_format")));
        } else {
            panic!("expected Repair");
        }
    }

    #[test]
    fn repeated_clears_on_success() {
        let mut c = RepairController::new(RepairPolicy::default());
        c.observe_validation(fail(E0308));
        c.observe_validation(pass());
        assert_eq!(c.state.repaired_failures.len(), 1);
    }
}
