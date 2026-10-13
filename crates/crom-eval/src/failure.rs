//! Failure taxonomy (M15).
//!
//! M14's primary score is binary `resolved`. M15 adds a richer failure
//! taxonomy so that *why* a task failed is queryable in reports, trends, and
//! the regression index. Each failed run maps to exactly one primary
//! [`FailureClass`] plus any number of secondary labels.

use serde::{Deserialize, Serialize};

/// The primary reason a task did not resolve. `None` means the task resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureClass {
    /// The agent produced no changes and did not engage with the task.
    DidNotAttempt,
    /// The agent edited the wrong file(s); required paths untouched.
    WrongFile,
    /// The workspace does not compile after the run.
    CompileError,
    /// Public verification failed.
    PublicTestFailure,
    /// Hidden verification failed (public may have passed).
    HiddenTestFailure,
    /// Public passed but hidden failed — classic overfit to visible tests.
    OverfitPublicTests,
    /// The patch is excessively large for the task.
    ExcessivePatch,
    /// The agent attempted an unsafe/destructive command.
    UnsafeCommand,
    /// The agent looped on the same tool/command without progress.
    ToolLoop,
    /// The agent lost the thread / ran out of useful context.
    ContextFailure,
    /// A planning task failed to plan.
    PlanningFailure,
    /// The run exceeded its time budget.
    Timeout,
    /// The model provider errored or was unreachable.
    ProviderError,
    /// The harness itself errored (artifact, workspace, internal).
    HarnessError,
    /// The agent modified a forbidden path.
    ForbiddenPathChanged,
    /// A required path was not created/modified.
    RequiredPathMissing,
    /// Verification was non-deterministic across repeats.
    VerificationFlake,
    /// The isolation backend failed (engine missing, container error).
    IsolationError,
    /// An adapter failed to load/materialize/normalize a task.
    AdapterError,
    /// The task's oracle patch failed to apply or did not pass checks.
    OracleValidationFailed,
    /// The task manifest/fixture is invalid.
    TaskInvalid,
    // --- M16 repair-loop failure classes ---
    /// A compile error survived all repair attempts.
    UnrecoveredCompileError,
    /// A test failure survived all repair attempts.
    UnrecoveredTestFailure,
    /// The same failure fingerprint repeated past the policy limit.
    RepeatedSameFailure,
    /// The agent finalized after a mutation without validating it.
    UnvalidatedCompletion,
    /// The bounded repair loop exhausted its attempt budget.
    RepairAttemptLimitExceeded,
    /// Command output could not be parsed into a structured diagnostic.
    DiagnosticParseFailure,
}

impl FailureClass {
    /// Stable string key (matches the serde representation).
    pub fn key(&self) -> &'static str {
        use FailureClass::*;
        match self {
            DidNotAttempt => "did_not_attempt",
            WrongFile => "wrong_file",
            CompileError => "compile_error",
            PublicTestFailure => "public_test_failure",
            HiddenTestFailure => "hidden_test_failure",
            OverfitPublicTests => "overfit_public_tests",
            ExcessivePatch => "excessive_patch",
            UnsafeCommand => "unsafe_command",
            ToolLoop => "tool_loop",
            ContextFailure => "context_failure",
            PlanningFailure => "planning_failure",
            Timeout => "timeout",
            ProviderError => "provider_error",
            HarnessError => "harness_error",
            ForbiddenPathChanged => "forbidden_path_changed",
            RequiredPathMissing => "required_path_missing",
            VerificationFlake => "verification_flake",
            IsolationError => "isolation_error",
            AdapterError => "adapter_error",
            OracleValidationFailed => "oracle_validation_failed",
            TaskInvalid => "task_invalid",
            UnrecoveredCompileError => "unrecovered_compile_error",
            UnrecoveredTestFailure => "unrecovered_test_failure",
            RepeatedSameFailure => "repeated_same_failure",
            UnvalidatedCompletion => "unvalidated_completion",
            RepairAttemptLimitExceeded => "repair_attempt_limit_exceeded",
            DiagnosticParseFailure => "diagnostic_parse_failure",
        }
    }
}

/// Signals used to classify a failure. The runner fills this in from the
/// outcome, oracle reports, and patch.
#[derive(Debug, Clone, Default)]
pub struct FailureSignals {
    pub timed_out: bool,
    pub provider_unavailable: bool,
    pub harness_errored: bool,
    pub empty_patch: bool,
    pub public_passed: bool,
    pub hidden_present: bool,
    pub hidden_passed: bool,
    pub compile_failed: bool,
    pub forbidden_changed: bool,
    pub required_missing: bool,
    pub unsafe_command: bool,
    pub tool_loop: bool,
    pub planning_required: bool,
    pub plan_emitted: bool,
    pub excessive_patch: bool,
}

/// Map a set of signals to a primary failure class plus secondary labels.
/// Returns `None` when the task resolved (no failure).
pub fn classify(sig: &FailureSignals) -> Option<(FailureClass, Vec<FailureClass>)> {
    use FailureClass::*;
    let resolved = sig.public_passed
        && (!sig.hidden_present || sig.hidden_passed)
        && !sig.forbidden_changed
        && !sig.required_missing
        && !sig.timed_out
        && !sig.harness_errored
        && !sig.provider_unavailable;
    if resolved {
        return None;
    }

    let mut secondary = Vec::new();
    if sig.unsafe_command {
        secondary.push(UnsafeCommand);
    }
    if sig.tool_loop {
        secondary.push(ToolLoop);
    }
    if sig.excessive_patch {
        secondary.push(ExcessivePatch);
    }
    if sig.forbidden_changed {
        secondary.push(ForbiddenPathChanged);
    }
    if sig.required_missing {
        secondary.push(RequiredPathMissing);
    }

    // Primary: most specific / most actionable first.
    let primary = if sig.provider_unavailable {
        ProviderError
    } else if sig.harness_errored {
        HarnessError
    } else if sig.timed_out {
        Timeout
    } else if sig.forbidden_changed {
        ForbiddenPathChanged
    } else if sig.planning_required && !sig.plan_emitted {
        PlanningFailure
    } else if sig.compile_failed {
        CompileError
    } else if sig.public_passed && sig.hidden_present && !sig.hidden_passed {
        OverfitPublicTests
    } else if sig.hidden_present && !sig.hidden_passed && !sig.public_passed {
        HiddenTestFailure
    } else if sig.required_missing && sig.empty_patch {
        DidNotAttempt
    } else if sig.required_missing {
        RequiredPathMissing
    } else if sig.empty_patch {
        DidNotAttempt
    } else {
        PublicTestFailure
    };

    // Avoid duplicating the primary in secondary.
    secondary.retain(|c| *c != primary);
    Some((primary, secondary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_has_no_failure() {
        let sig = FailureSignals {
            public_passed: true,
            ..Default::default()
        };
        assert!(classify(&sig).is_none());
    }

    #[test]
    fn overfit_detected() {
        let sig = FailureSignals {
            public_passed: true,
            hidden_present: true,
            hidden_passed: false,
            ..Default::default()
        };
        let (primary, _) = classify(&sig).unwrap();
        assert_eq!(primary, FailureClass::OverfitPublicTests);
    }

    #[test]
    fn did_not_attempt() {
        let sig = FailureSignals {
            empty_patch: true,
            ..Default::default()
        };
        let (primary, _) = classify(&sig).unwrap();
        assert_eq!(primary, FailureClass::DidNotAttempt);
    }

    #[test]
    fn timeout_takes_priority() {
        let sig = FailureSignals {
            timed_out: true,
            empty_patch: true,
            ..Default::default()
        };
        let (primary, _) = classify(&sig).unwrap();
        assert_eq!(primary, FailureClass::Timeout);
    }

    #[test]
    fn forbidden_path_is_secondary_and_primary() {
        let sig = FailureSignals {
            public_passed: true,
            forbidden_changed: true,
            ..Default::default()
        };
        let (primary, _secondary) = classify(&sig).unwrap();
        assert_eq!(primary, FailureClass::ForbiddenPathChanged);
    }
}
