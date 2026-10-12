//! Diagnostic fingerprinting and repeated-failure detection (M16E).
//!
//! A fingerprint is a stable identity for a failure so that the *same* failure
//! across repair attempts hashes identically even when temp paths, line
//! numbers in unrelated noise, ANSI codes, or timestamps differ.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};

use super::model::{CommandDiagnostic, DiagnosticFingerprint, DiagnosticKind, DiagnosticSummary};
use super::text::{normalize_command, normalize_message, relativize};

/// Build a fingerprint from the command, its cwd, and the distilled summary.
pub fn fingerprint(
    command: &str,
    cwd: &Path,
    summary: &DiagnosticSummary,
    primary_code: Option<&str>,
    primary_message: &str,
    raw_output: &str,
) -> DiagnosticFingerprint {
    let normalized_command = normalize_command(command);

    // The primary file is relativized to cwd so a different temp root does not
    // change the fingerprint.
    let primary_file = summary.primary_file.as_ref().map(|p| relativize(p, cwd));

    // The hashed material has two parts joined by a separator:
    // - a *normalized* part (paths/line-numbers/whitespace canonicalized) so
    //   the same failure hashes stably across temp dirs and shifting lines;
    // - a *raw* part (test name + assertion values) that is deliberately NOT
    //   number-normalized, so two assertions that differ only in numeric values
    //   (e.g. `left: 1` vs `left: 3`) do NOT collapse to one fingerprint and
    //   thus are not mistaken for a repeated identical failure.
    let mut normalized_part = String::new();
    let mut raw_part = String::new();

    if let Some(ft) = summary.failing_tests.first() {
        raw_part.push_str(&ft.name);
        raw_part.push('\n');
        if let (Some(l), Some(r)) = (&ft.assertion_left, &ft.assertion_right) {
            raw_part.push_str(l);
            raw_part.push_str(" == ");
            raw_part.push_str(r);
            raw_part.push('\n');
        }
    }
    normalized_part.push_str(primary_message);

    // When no diagnostic could be parsed, the primary message is just a generic
    // per-command fallback, which would collapse every unrecognized failure of
    // the same command to one fingerprint. Fold a bounded, normalized excerpt of
    // the raw output in so genuinely different failures differ.
    if matches!(summary.primary_kind, DiagnosticKind::Unknown) {
        normalized_part.push('\n');
        normalized_part.push_str(&super::text::truncate_bytes(raw_output, 2000));
    }

    let combined = format!("{}\x1f{}", normalize_message(&normalized_part), raw_part);
    let normalized_message_hash = hash_message(&combined);

    DiagnosticFingerprint {
        normalized_command,
        primary_kind: summary.primary_kind,
        primary_code: primary_code.map(|s| s.to_string()),
        primary_file,
        normalized_message_hash,
    }
}

fn hash_message(normalized: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hex::encode(hasher.finalize())
}

/// Status returned when recording a failure into [`FailureHistory`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatStatus {
    NewFailure,
    Repeated { count: u32 },
}

impl RepeatStatus {
    pub fn is_repeat(&self) -> bool {
        matches!(self, RepeatStatus::Repeated { .. })
    }

    pub fn count(&self) -> u32 {
        match self {
            RepeatStatus::NewFailure => 1,
            RepeatStatus::Repeated { count } => *count,
        }
    }
}

/// Accumulates observed failures and detects repeated identical ones.
#[derive(Debug, Clone, Default)]
pub struct FailureHistory {
    pub failures: Vec<CommandDiagnostic>,
    pub repeats_by_fingerprint: HashMap<DiagnosticFingerprint, u32>,
}

impl FailureHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a diagnostic, returning whether its fingerprint had been seen
    /// before and how many times it has now occurred.
    pub fn record(&mut self, diagnostic: CommandDiagnostic) -> RepeatStatus {
        let fp = diagnostic.fingerprint.clone();
        let count = self.repeats_by_fingerprint.entry(fp).or_insert(0);
        *count += 1;
        let n = *count;
        self.failures.push(diagnostic);
        if n <= 1 {
            RepeatStatus::NewFailure
        } else {
            RepeatStatus::Repeated { count: n }
        }
    }

    /// How many times a given fingerprint has been recorded.
    pub fn count_for(&self, fp: &DiagnosticFingerprint) -> u32 {
        self.repeats_by_fingerprint.get(fp).copied().unwrap_or(0)
    }

    /// The maximum repeat count across all fingerprints.
    pub fn max_repeats(&self) -> u32 {
        self.repeats_by_fingerprint
            .values()
            .copied()
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::parse_command_diagnostic;
    use crate::diagnostics::CommandDiagnosticInput;
    use std::path::PathBuf;

    fn diag(cwd: &str, stderr: &str) -> CommandDiagnostic {
        parse_command_diagnostic(CommandDiagnosticInput {
            command: "cargo test".into(),
            cwd: PathBuf::from(cwd),
            exit_code: Some(101),
            timed_out: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
            duration_ms: Some(10),
        })
    }

    #[test]
    fn same_error_same_fingerprint_across_tempdirs() {
        let a = diag(
            "/tmp/aaa",
            "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n",
        );
        let b = diag(
            "/tmp/bbb",
            "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n",
        );
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn different_errors_differ() {
        let a = diag(
            "/tmp/x",
            "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n",
        );
        let b = diag(
            "/tmp/x",
            "error[E0432]: unresolved import `foo`\n  --> src/bar.rs:1:5\n",
        );
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn repeat_detection() {
        let mut h = FailureHistory::new();
        let a = diag(
            "/tmp/x",
            "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n",
        );
        let b = diag(
            "/tmp/y",
            "error[E0308]: mismatched types\n  --> src/foo.rs:42:17\n",
        );
        assert_eq!(h.record(a), RepeatStatus::NewFailure);
        assert_eq!(h.record(b), RepeatStatus::Repeated { count: 2 });
        assert_eq!(h.max_repeats(), 2);
    }

    #[test]
    fn distinct_test_failures_differ() {
        let a = diag(
            "/tmp/x",
            "---- a::one stdout ----\nthread 'a::one' panicked at src/a.rs:1:1:\nassertion failed\n",
        );
        let b = diag(
            "/tmp/x",
            "---- a::two stdout ----\nthread 'a::two' panicked at src/a.rs:1:1:\nassertion failed\n",
        );
        assert_ne!(a.fingerprint, b.fingerprint);
    }

    // Two assertion failures in the *same* test/location that differ only in
    // their numeric left/right values must NOT collapse to one fingerprint —
    // otherwise a numerically-progressing fix is mistaken for a repeat.
    #[test]
    fn distinct_numeric_assertions_differ() {
        let a = diag(
            "/tmp/x",
            "---- t::eq stdout ----\nthread 't::eq' panicked at src/a.rs:5:1:\nassertion `left == right` failed\n  left: 1\n right: 2\n",
        );
        let b = diag(
            "/tmp/x",
            "---- t::eq stdout ----\nthread 't::eq' panicked at src/a.rs:5:1:\nassertion `left == right` failed\n  left: 3\n right: 4\n",
        );
        assert_ne!(
            a.fingerprint, b.fingerprint,
            "different assertion values must produce different fingerprints"
        );
    }

    // Two unrecognized (Unknown-kind) failures of the same command with
    // different output must NOT share a fingerprint.
    #[test]
    fn distinct_unknown_outputs_differ() {
        let a = diag("/tmp/x", "custom checker: widget A is broken\n");
        let b = diag("/tmp/x", "custom checker: widget B is broken\n");
        assert_eq!(
            a.summary.primary_kind,
            super::super::DiagnosticKind::Unknown
        );
        assert_ne!(a.fingerprint, b.fingerprint);
    }
}
