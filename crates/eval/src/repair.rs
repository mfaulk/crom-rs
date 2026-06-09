//! Repair-loop integration for the eval harness (M16L/M16M).
//!
//! This wires the structured diagnostics + bounded repair controller from
//! `code_agent_core` into the eval runner's retry loop. After a failed
//! validation it parses the failing command into a [`CommandDiagnostic`],
//! drives a [`RepairController`] to produce a targeted repair brief used as the
//! next-attempt feedback, records a per-attempt trace, and computes repair
//! metrics for the result schema and reports.
//!
//! Everything here is offline and deterministic — no providers, no network.

use std::path::Path;

use serde::{Deserialize, Serialize};

use code_agent_core::diagnostics::{
    classify_command, focused_validation_plan, parse_command_diagnostic, CommandDiagnostic,
    CommandDiagnosticInput, CommandIntent, DiagnosticKind,
};
use code_agent_core::repair::{
    RepairAttemptRecord, RepairController, RepairDecision, RepairPolicy, StopReason,
};
use code_agent_sdk::SdkEvent;

use crate::oracle::OracleReport;

/// Final validation status for the repair trace / metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalValidationStatus {
    Passed,
    Failed,
    Unknown,
}

/// Repair metrics surfaced in the eval result schema (M16M).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairMetrics {
    pub enabled: bool,
    pub repair_attempt_count: u32,
    pub repair_success_count: u32,
    pub compiler_error_recovered: Option<bool>,
    pub test_failure_recovered: Option<bool>,
    pub same_error_repeated: bool,
    pub same_error_repeat_count: u32,
    pub focused_validation_suggested: bool,
    pub focused_validation_used: Option<bool>,
    pub validation_after_last_edit: Option<bool>,
    pub final_answer_without_validation: bool,
    pub diagnostic_parse_success: bool,
    pub diagnostic_primary_kind: Option<DiagnosticKind>,
    pub diagnostic_primary_file: Option<String>,
    pub diagnostic_primary_line: Option<u32>,
    pub repair_attempt_limit_exceeded: bool,
    pub final_validation_status: FinalValidationStatus,
}

impl Default for RepairMetrics {
    fn default() -> Self {
        Self {
            enabled: false,
            repair_attempt_count: 0,
            repair_success_count: 0,
            compiler_error_recovered: None,
            test_failure_recovered: None,
            same_error_repeated: false,
            same_error_repeat_count: 0,
            focused_validation_suggested: false,
            focused_validation_used: None,
            validation_after_last_edit: None,
            final_answer_without_validation: false,
            diagnostic_parse_success: false,
            diagnostic_primary_kind: None,
            diagnostic_primary_file: None,
            diagnostic_primary_line: None,
            repair_attempt_limit_exceeded: false,
            final_validation_status: FinalValidationStatus::Unknown,
        }
    }
}

/// The serializable repair trace written to `repair.json` (M16L).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairTrace {
    pub enabled: bool,
    pub max_attempts: u32,
    pub attempts: Vec<RepairAttemptRecord>,
    pub same_failure_repeats: u32,
    pub validation_after_last_mutation: bool,
    pub final_validation_status: FinalValidationStatus,
}

/// A single recorded validation attempt for `validation-attempts.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationAttempt {
    pub index: u32,
    pub command: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub primary_kind: Option<DiagnosticKind>,
    pub fingerprint: Option<String>,
}

/// Build a [`CommandDiagnostic`] from the first failing command in an oracle
/// report. Returns `None` when every command passed (or none ran).
pub fn diagnostic_from_report(report: &OracleReport, cwd: &Path) -> Option<CommandDiagnostic> {
    let failing = report.commands.iter().find(|c| !c.success)?;
    Some(parse_command_diagnostic(CommandDiagnosticInput {
        command: failing.command.clone(),
        cwd: cwd.to_path_buf(),
        exit_code: failing.exit_code,
        timed_out: failing.timed_out,
        stdout: failing.stdout.clone(),
        stderr: failing.stderr.clone(),
        duration_ms: Some(failing.duration_ms),
    }))
}

/// Stateful driver used by the runner's retry loop.
///
/// `next_feedback` is called after each failed attempt's validation to obtain
/// the brief text for the *next* attempt (or `None` to stop). Diagnostics,
/// attempt records, and validation attempts accumulate for artifact emission.
pub struct RepairSession {
    controller: RepairController,
    policy: RepairPolicy,
    pub diagnostics: Vec<CommandDiagnostic>,
    pub attempt_records: Vec<RepairAttemptRecord>,
    pub validation_attempts: Vec<ValidationAttempt>,
    attempt_index: u32,
    limit_exceeded: bool,
    focused_suggested: bool,
}

/// Outcome of asking the session for the next attempt's feedback.
pub struct FeedbackOutcome {
    /// Brief text to inject as the next attempt's prompt addendum, if retrying.
    pub feedback: Option<String>,
    /// True when the controller decided to stop (no further attempts).
    pub stop: bool,
}

impl RepairSession {
    pub fn new(policy: RepairPolicy) -> Self {
        Self {
            controller: RepairController::new(policy.clone()),
            policy,
            diagnostics: Vec::new(),
            attempt_records: Vec::new(),
            validation_attempts: Vec::new(),
            attempt_index: 0,
            limit_exceeded: false,
            focused_suggested: false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.policy.enabled
    }

    /// Record a passing validation (clears repair state, marks recovery).
    pub fn record_pass(&mut self, report: &OracleReport, cwd: &Path) {
        // A passing oracle with no command (e.g. a `file_contains` assertion or
        // a `kind = "none"` oracle) is NOT a command validation. Don't fabricate
        // a validation attempt: the final validation status must stay `Unknown`
        // rather than being reported as `Passed`.
        let Some(cmd) = report.commands.iter().map(|c| c.command.clone()).next() else {
            if let Some(last) = self.attempt_records.last_mut() {
                last.recovered = true;
            }
            return;
        };
        self.attempt_index += 1;
        self.validation_attempts.push(ValidationAttempt {
            index: self.attempt_index,
            command: cmd.clone(),
            passed: true,
            exit_code: report.commands.iter().find_map(|c| c.exit_code),
            primary_kind: None,
            fingerprint: None,
        });
        // Build a synthetic success diagnostic so the controller clears state.
        let diag = parse_command_diagnostic(CommandDiagnosticInput {
            command: cmd,
            cwd: cwd.to_path_buf(),
            exit_code: Some(0),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: None,
        });
        let _ = self.controller.observe_validation(diag);
        // Mark the most recent attempt record (if any) as recovered.
        if let Some(last) = self.attempt_records.last_mut() {
            last.recovered = true;
        }
    }

    /// Observe a failed validation and decide the next feedback.
    pub fn record_failure(&mut self, report: &OracleReport, cwd: &Path) -> FeedbackOutcome {
        self.attempt_index += 1;
        let Some(diag) = diagnostic_from_report(report, cwd) else {
            // The failure was not a failing *command* (e.g. a file-content
            // assertion or a behavior rule). There is no command diagnostic to
            // build a repair brief from, so do NOT stop the loop — let the
            // caller fall back to its generic retry feedback. Stopping here
            // would deny non-command failures any retries.
            return FeedbackOutcome {
                feedback: None,
                stop: false,
            };
        };
        self.diagnostics.push(diag.clone());
        self.validation_attempts.push(ValidationAttempt {
            index: self.attempt_index,
            command: diag.command.clone(),
            passed: false,
            exit_code: diag.exit_code,
            primary_kind: Some(diag.summary.primary_kind),
            fingerprint: Some(diag.fingerprint.short_hash()),
        });

        if focused_validation_plan(&diag).is_some() {
            self.focused_suggested = true;
        }

        let decision = self.controller.observe_validation(diag.clone());
        let (brief, stop) = match decision {
            RepairDecision::Repair { brief } => (brief, false),
            RepairDecision::Stop { reason, brief } => {
                if matches!(reason, StopReason::MaxAttemptsExceeded) {
                    self.limit_exceeded = true;
                }
                (brief, true)
            }
            RepairDecision::Disabled => {
                // Repair disabled: still record the diagnostic for metrics, but
                // produce no brief and let the caller use its own feedback.
                return FeedbackOutcome {
                    feedback: None,
                    stop: false,
                };
            }
            RepairDecision::Cleared { .. } => {
                return FeedbackOutcome {
                    feedback: None,
                    stop: true,
                };
            }
        };
        self.attempt_records.push(RepairAttemptRecord {
            index: brief.attempt_index,
            trigger_command: brief.command.clone(),
            failure_kind: diag.summary.primary_kind,
            fingerprint: diag.fingerprint.short_hash(),
            primary_file: diag.summary.primary_file.clone(),
            primary_line: diag.summary.primary_line,
            focused_commands: brief.focused_commands.clone(),
            repeated: brief.repeated,
            recovered: false,
        });
        FeedbackOutcome {
            feedback: Some(brief.render()),
            stop,
        }
    }

    /// Finalize: compute the trace + metrics given the final validation result
    /// and the run's event stream.
    pub fn finish(self, final_passed: bool, events: &[SdkEvent]) -> (RepairTrace, RepairMetrics) {
        let final_status = if self.diagnostics.is_empty() && self.validation_attempts.is_empty() {
            FinalValidationStatus::Unknown
        } else if final_passed {
            FinalValidationStatus::Passed
        } else {
            FinalValidationStatus::Failed
        };

        let same_repeat_count = self.controller.state.max_repeats_observed();
        let same_error_repeated = same_repeat_count >= 2;

        // Per-kind recovery: did we observe a failure of this family and then
        // ultimately pass?
        let had_compile = self
            .diagnostics
            .iter()
            .any(|d| is_compile_kind(d.summary.primary_kind));
        let had_test = self
            .diagnostics
            .iter()
            .any(|d| is_test_kind(d.summary.primary_kind));

        let compiler_error_recovered = if had_compile {
            Some(final_passed)
        } else {
            None
        };
        let test_failure_recovered = if had_test { Some(final_passed) } else { None };

        let first = self.diagnostics.first();
        let metrics = RepairMetrics {
            enabled: self.policy.enabled,
            repair_attempt_count: self.attempt_records.len() as u32,
            repair_success_count: u32::from(final_passed && !self.diagnostics.is_empty()),
            compiler_error_recovered,
            test_failure_recovered,
            same_error_repeated,
            same_error_repeat_count: same_repeat_count,
            focused_validation_suggested: self.focused_suggested,
            focused_validation_used: focused_validation_used(events),
            validation_after_last_edit: validation_after_last_edit(events),
            final_answer_without_validation: final_answer_without_validation(events),
            diagnostic_parse_success: first.map(|d| !d.parsed.is_empty()).unwrap_or(false),
            diagnostic_primary_kind: first.map(|d| d.summary.primary_kind),
            diagnostic_primary_file: first
                .and_then(|d| d.summary.primary_file.as_ref())
                .map(|p| p.display().to_string()),
            diagnostic_primary_line: first.and_then(|d| d.summary.primary_line),
            repair_attempt_limit_exceeded: self.limit_exceeded,
            final_validation_status: final_status,
        };

        let trace = RepairTrace {
            enabled: self.policy.enabled,
            max_attempts: self.policy.max_attempts,
            attempts: self.attempt_records,
            same_failure_repeats: same_repeat_count,
            validation_after_last_mutation: self.controller.state.validation_after_last_mutation,
            final_validation_status: final_status,
        };
        (trace, metrics)
    }
}

fn is_compile_kind(k: DiagnosticKind) -> bool {
    matches!(
        k,
        DiagnosticKind::TypeMismatch
            | DiagnosticKind::MissingImport
            | DiagnosticKind::MissingCrate
            | DiagnosticKind::MissingTraitImpl
            | DiagnosticKind::TraitBound
            | DiagnosticKind::BorrowChecker
            | DiagnosticKind::Lifetime
            | DiagnosticKind::MoveError
            | DiagnosticKind::MutabilityError
            | DiagnosticKind::VisibilityError
            | DiagnosticKind::UnresolvedName
            | DiagnosticKind::DuplicateDefinition
            | DiagnosticKind::SyntaxError
    )
}

fn is_test_kind(k: DiagnosticKind) -> bool {
    matches!(
        k,
        DiagnosticKind::TestAssertionFailure
            | DiagnosticKind::TestPanic
            | DiagnosticKind::SnapshotMismatch
            | DiagnosticKind::GoldenOutputMismatch
            | DiagnosticKind::CliExitMismatch
    )
}

fn bash_command(e: &SdkEvent) -> Option<&str> {
    match e {
        SdkEvent::ToolCallStarted(ev) if ev.tool_name == "Bash" => {
            ev.input.get("command").and_then(|v| v.as_str())
        }
        _ => None,
    }
}

fn is_mutating_tool(e: &SdkEvent) -> bool {
    match e {
        SdkEvent::ToolCallStarted(ev) => {
            matches!(ev.tool_name.as_str(), "Edit" | "Write")
                || bash_command(e)
                    .map(|c| classify_command(c).is_mutation())
                    .unwrap_or(false)
        }
        _ => false,
    }
}

fn is_validation_event(e: &SdkEvent) -> bool {
    bash_command(e)
        .map(|c| classify_command(c).is_validation())
        .unwrap_or(false)
}

/// Whether a validation command ran after the last mutating tool call.
/// `None` when no mutation occurred.
pub fn validation_after_last_edit(events: &[SdkEvent]) -> Option<bool> {
    let last_mut = events.iter().rposition(is_mutating_tool)?;
    Some(
        events
            .iter()
            .enumerate()
            .any(|(i, e)| i > last_mut && is_validation_event(e)),
    )
}

/// True when the agent mutated files but never ran a validation command after
/// the final mutation (the `final_answer_without_post_edit_validation` warning).
pub fn final_answer_without_validation(events: &[SdkEvent]) -> bool {
    matches!(validation_after_last_edit(events), Some(false))
}

/// Whether the agent actually ran a focused validation command (e.g.
/// `cargo test some::specific_test`). `None` when no validation ran at all.
pub fn focused_validation_used(events: &[SdkEvent]) -> Option<bool> {
    let mut saw_validation = false;
    let mut saw_focused = false;
    for e in events {
        if let Some(cmd) = bash_command(e) {
            let class = classify_command(cmd);
            if class.is_validation() {
                saw_validation = true;
            }
            if class.has(CommandIntent::FocusedValidation) {
                saw_focused = true;
            }
        }
    }
    if saw_validation {
        Some(saw_focused)
    } else {
        None
    }
}

/// Render the human-facing `repair.md` trace.
pub fn render_repair_md(trace: &RepairTrace) -> String {
    let mut s = String::from("# Repair trace\n\n");
    if !trace.enabled {
        s.push_str("Repair loop disabled for this run.\n\n");
    }
    if trace.attempts.is_empty() {
        s.push_str("No repair attempts were recorded.\n");
    }
    for a in &trace.attempts {
        s.push_str(&format!("Attempt {}\n", a.index));
        s.push_str(&format!("- Failed command: {}\n", a.trigger_command));
        let loc = match (&a.primary_file, a.primary_line) {
            (Some(f), Some(l)) => format!("{}:{}", f.display(), l),
            (Some(f), None) => f.display().to_string(),
            _ => "unknown".to_string(),
        };
        s.push_str(&format!(
            "- Failure: {} at {}\n",
            a.failure_kind.as_str(),
            loc
        ));
        if !a.focused_commands.is_empty() {
            s.push_str(&format!(
                "- Focused command suggested: {}\n",
                a.focused_commands.join(" ; ")
            ));
        }
        if a.repeated {
            s.push_str("- Repeated: yes\n");
        }
        s.push_str(&format!(
            "- Outcome: {}\n\n",
            if a.recovered {
                "recovered"
            } else {
                "unresolved"
            }
        ));
    }
    s.push_str("Final validation:\n");
    let status = match trace.final_validation_status {
        FinalValidationStatus::Passed => "passed",
        FinalValidationStatus::Failed => "failed",
        FinalValidationStatus::Unknown => "unknown",
    };
    s.push_str(&format!("- {status}\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::CommandResult;
    use code_agent_sdk::event::ToolCallStartedEvent;
    use code_agent_sdk::RunId;

    fn started(tool: &str, input: serde_json::Value) -> SdkEvent {
        SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: RunId::new(),
            tool_name: tool.to_string(),
            tool_use_id: "t".into(),
            input,
        })
    }

    fn report(cmd: &str, success: bool, stderr: &str) -> OracleReport {
        OracleReport {
            commands: vec![CommandResult {
                command: cmd.into(),
                success,
                exit_code: Some(if success { 0 } else { 101 }),
                stdout: String::new(),
                stderr: stderr.into(),
                duration_ms: 5,
                timed_out: false,
                output_truncated: false,
            }],
            assertions: Vec::new(),
            behavior: Vec::new(),
        }
    }

    const E0308: &str = "error[E0308]: mismatched types\n  --> src/config.rs:74:21\n";

    #[test]
    fn diagnostic_from_failing_report() {
        let r = report("cargo test", false, E0308);
        let d = diagnostic_from_report(&r, Path::new("/tmp/x")).unwrap();
        assert_eq!(d.summary.primary_kind, DiagnosticKind::TypeMismatch);
    }

    #[test]
    fn session_records_attempt_and_recovers() {
        let mut s = RepairSession::new(RepairPolicy::default());
        let out = s.record_failure(&report("cargo test", false, E0308), Path::new("/tmp/x"));
        assert!(out.feedback.is_some());
        assert!(!out.stop);
        s.record_pass(&report("cargo test", true, ""), Path::new("/tmp/x"));
        let (trace, metrics) = s.finish(true, &[]);
        assert_eq!(trace.attempts.len(), 1);
        assert!(trace.attempts[0].recovered);
        assert_eq!(metrics.compiler_error_recovered, Some(true));
        assert_eq!(
            metrics.final_validation_status,
            FinalValidationStatus::Passed
        );
        assert_eq!(metrics.repair_success_count, 1);
    }

    #[test]
    fn repeated_failure_stops() {
        let policy = RepairPolicy {
            max_attempts: 10,
            max_same_failure_repeats: 2,
            ..RepairPolicy::default()
        };
        let mut s = RepairSession::new(policy);
        let cwd = Path::new("/tmp/x");
        let o1 = s.record_failure(&report("cargo test", false, E0308), cwd);
        assert!(!o1.stop);
        let o2 = s.record_failure(&report("cargo test", false, E0308), cwd);
        assert!(o2.stop);
        let (_t, m) = s.finish(false, &[]);
        assert!(m.same_error_repeated);
        assert_eq!(m.final_validation_status, FinalValidationStatus::Failed);
    }

    #[test]
    fn validation_after_last_edit_detected() {
        let events = vec![
            started("Edit", serde_json::json!({"file_path": "src/lib.rs"})),
            started("Bash", serde_json::json!({"command": "cargo test"})),
        ];
        assert_eq!(validation_after_last_edit(&events), Some(true));
        assert!(!final_answer_without_validation(&events));
    }

    #[test]
    fn final_answer_without_validation_detected() {
        let events = vec![
            started("Bash", serde_json::json!({"command": "cargo test"})),
            started("Edit", serde_json::json!({"file_path": "src/lib.rs"})),
        ];
        assert_eq!(validation_after_last_edit(&events), Some(false));
        assert!(final_answer_without_validation(&events));
    }

    #[test]
    fn focused_validation_used_detected() {
        let events = vec![started(
            "Bash",
            serde_json::json!({"command": "cargo test parser::foo"}),
        )];
        assert_eq!(focused_validation_used(&events), Some(true));
        let broad = vec![started(
            "Bash",
            serde_json::json!({"command": "cargo test"}),
        )];
        assert_eq!(focused_validation_used(&broad), Some(false));
    }

    #[test]
    fn disabled_session_no_feedback() {
        let mut s = RepairSession::new(RepairPolicy::disabled());
        let out = s.record_failure(&report("cargo test", false, E0308), Path::new("/tmp/x"));
        assert!(out.feedback.is_none());
        let (_t, m) = s.finish(false, &[]);
        assert!(!m.enabled);
        // Diagnostic still parsed for metrics.
        assert!(m.diagnostic_parse_success);
    }
}
