//! Structured command-failure diagnostics (M16).
//!
//! This module turns raw command output (stdout/stderr/exit-code/timeout) into
//! a structured [`CommandDiagnostic`]: parsed compiler errors, failing tests,
//! a compact model-facing summary, and a stable fingerprint for repeated-
//! failure detection. It is pure logic with no I/O, so it is reused by the
//! Bash tool path, the repair controller, the eval harness, and the
//! `diagnostics` CLI commands.
//!
//! Parsing is defensive: malformed or truncated output never panics; unknown
//! cases are represented as [`DiagnosticKind::Unknown`] rather than guessed.

mod cargo_test;
mod command;
mod compact;
mod fingerprint;
mod model;
mod rust;
mod text;

pub use cargo_test::{looks_like_test_output, TestParse};
pub use command::{
    classify_command, focused_validation_plan, CommandClass, CommandIntent, ValidationPlan,
};
pub use compact::{compact, CompactionConfig};
pub use fingerprint::{fingerprint, FailureHistory, RepeatStatus};
pub use model::{
    CommandDiagnostic, CommandDiagnosticInput, CommandStatus, DiagnosticFingerprint,
    DiagnosticKind, DiagnosticSeverity, DiagnosticSource, DiagnosticSummary, FailingTest,
    ParsedDiagnostic, RelatedDiagnostic, SuggestedFix,
};

/// Parse a command's captured output into a structured diagnostic.
///
/// This is the single public entry point. It:
/// 1. Determines the [`CommandStatus`] from exit code / timeout.
/// 2. Detects command-level failures (timeout, command-not-found, permission).
/// 3. Runs the Rust compiler parser and the cargo-test parser over the
///    combined output.
/// 4. Distills a [`DiagnosticSummary`] (primary kind/file/line, failing tests).
/// 5. Computes a stable [`DiagnosticFingerprint`].
pub fn parse_command_diagnostic(input: CommandDiagnosticInput) -> CommandDiagnostic {
    let CommandDiagnosticInput {
        command,
        cwd,
        exit_code,
        timed_out,
        stdout,
        stderr,
        duration_ms,
    } = input;

    let status = if timed_out {
        CommandStatus::Timeout
    } else {
        match exit_code {
            Some(0) => CommandStatus::Success,
            Some(_) => CommandStatus::ExitFailure,
            None => CommandStatus::Signaled,
        }
    };

    // Combined output for parsing. We parse both streams since cargo writes
    // diagnostics to stderr but JSON to stdout, and tests to stdout.
    let combined = format!("{stdout}\n{stderr}");

    let mut parsed: Vec<ParsedDiagnostic> = Vec::new();
    let mut failing_tests: Vec<FailingTest> = Vec::new();

    // Command-level failures first (these short-circuit content parsing).
    let command_level = detect_command_level_failure(&combined, &command, status);
    if let Some(d) = command_level {
        parsed.push(d);
    } else if status != CommandStatus::Success {
        // Compiler diagnostics.
        let rust_diags = rust::parse(&combined);
        parsed.extend(rust_diags);

        // Test failures (only if it looks like test output, to avoid spurious
        // panic matches in unrelated logs).
        if cargo_test::looks_like_test_output(&combined) {
            let tp = cargo_test::parse(&combined);
            failing_tests = tp.failing_tests;
            // Only add test diagnostics if we didn't already get compiler
            // errors (a failing compile is the real blocker).
            if !parsed.iter().any(|d| d.is_error()) {
                parsed.extend(tp.diagnostics);
            }
        }
    }

    let summary = build_summary(&parsed, &failing_tests, status, &command);

    let primary = parsed
        .iter()
        .find(|d| d.is_error())
        .or_else(|| parsed.first());
    let primary_code = primary.and_then(|d| d.code.as_deref());
    let primary_message = primary
        .map(|d| d.message.as_str())
        .unwrap_or(&summary.short_message);

    let fp = fingerprint::fingerprint(
        &command,
        &cwd,
        &summary,
        primary_code,
        primary_message,
        &combined,
    );

    let cfg = CompactionConfig::default();
    let stdout_excerpt = text::truncate_bytes(&stdout, cfg.max_excerpt_bytes);
    let stderr_excerpt = text::truncate_bytes(&stderr, cfg.max_excerpt_bytes);

    CommandDiagnostic {
        command,
        cwd,
        exit_code,
        status,
        duration_ms,
        stdout_excerpt,
        stderr_excerpt,
        parsed,
        summary,
        fingerprint: fp,
    }
}

/// Detect failures that are about the command itself rather than its content:
/// timeout, command-not-found, permission denied.
fn detect_command_level_failure(
    combined: &str,
    _command: &str,
    status: CommandStatus,
) -> Option<ParsedDiagnostic> {
    if status == CommandStatus::Timeout {
        return Some(ParsedDiagnostic::new(
            DiagnosticSource::Shell,
            DiagnosticSeverity::Timeout,
            DiagnosticKind::Timeout,
            "command timed out before completing",
        ));
    }
    let lower = combined.to_ascii_lowercase();
    if lower.contains("command not found")
        || lower.contains("no such file or directory")
            && (lower.contains("exec") || lower.contains(": not found"))
    {
        return Some(ParsedDiagnostic::new(
            DiagnosticSource::Shell,
            DiagnosticSeverity::Error,
            DiagnosticKind::CommandNotFound,
            "command not found",
        ));
    }
    if lower.contains("permission denied") {
        return Some(ParsedDiagnostic::new(
            DiagnosticSource::Shell,
            DiagnosticSeverity::Error,
            DiagnosticKind::PermissionDenied,
            "permission denied",
        ));
    }
    None
}

fn build_summary(
    parsed: &[ParsedDiagnostic],
    failing_tests: &[FailingTest],
    status: CommandStatus,
    command: &str,
) -> DiagnosticSummary {
    let primary = parsed
        .iter()
        .find(|d| d.is_error())
        .or_else(|| parsed.first());

    let primary_kind = primary.map(|d| d.kind).unwrap_or(DiagnosticKind::Unknown);

    // Affected files: dedupe across parsed diagnostics + failing tests.
    let mut affected: Vec<std::path::PathBuf> = Vec::new();
    for d in parsed {
        if let Some(f) = &d.file {
            if !affected.contains(f) {
                affected.push(f.clone());
            }
        }
    }
    for ft in failing_tests {
        if let Some(f) = &ft.file {
            if !affected.contains(f) {
                affected.push(f.clone());
            }
        }
    }

    let short_message = match primary {
        Some(d) => {
            let loc = match (&d.file, d.line) {
                (Some(f), Some(l)) => format!(" in {}:{}", f.display(), l),
                (Some(f), None) => format!(" in {}", f.display()),
                _ => String::new(),
            };
            let code = d
                .code
                .as_deref()
                .map(|c| format!(" [{c}]"))
                .unwrap_or_default();
            format!(
                "{}{}{}: {}",
                d.kind.as_str(),
                code,
                loc,
                d.message.lines().next().unwrap_or(&d.message)
            )
        }
        None if status == CommandStatus::Success => "command succeeded".to_string(),
        None => format!("`{}` failed without a recognized diagnostic", command),
    };

    let repair_hint = primary
        .and_then(|d| d.kind.repair_hint())
        .map(|s| s.to_string());

    DiagnosticSummary {
        primary_kind,
        primary_file: primary.and_then(|d| d.file.clone()),
        primary_line: primary.and_then(|d| d.line),
        failing_tests: failing_tests.to_vec(),
        affected_files: affected,
        short_message,
        repair_hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(cmd: &str, code: Option<i32>, timed_out: bool, stderr: &str) -> CommandDiagnostic {
        parse_command_diagnostic(CommandDiagnosticInput {
            command: cmd.into(),
            cwd: PathBuf::from("/tmp/work"),
            exit_code: code,
            timed_out,
            stdout: String::new(),
            stderr: stderr.into(),
            duration_ms: Some(5),
        })
    }

    #[test]
    fn success_has_no_diagnostics() {
        let d = run("cargo test", Some(0), false, "");
        assert_eq!(d.status, CommandStatus::Success);
        assert!(d.parsed.is_empty());
        assert!(!d.is_failure());
    }

    #[test]
    fn type_mismatch_summary() {
        let d = run(
            "cargo test",
            Some(101),
            false,
            "error[E0308]: mismatched types\n  --> src/config.rs:74:21\n",
        );
        assert_eq!(d.summary.primary_kind, DiagnosticKind::TypeMismatch);
        assert_eq!(d.summary.primary_line, Some(74));
        assert!(d.summary.short_message.contains("config.rs"));
        assert!(d.summary.repair_hint.is_some());
    }

    #[test]
    fn timeout_classified() {
        let d = run("cargo test", None, true, "");
        assert_eq!(d.status, CommandStatus::Timeout);
        assert_eq!(d.summary.primary_kind, DiagnosticKind::Timeout);
    }

    #[test]
    fn command_not_found_classified() {
        let d = run(
            "cargp test",
            Some(127),
            false,
            "bash: cargp: command not found\n",
        );
        assert_eq!(d.summary.primary_kind, DiagnosticKind::CommandNotFound);
    }

    #[test]
    fn test_failure_populates_failing_tests() {
        let stderr = "---- a::b stdout ----\nthread 'a::b' panicked at src/a.rs:3:1:\nassertion `left == right` failed\n  left: 1\n right: 2\n\ntest result: FAILED. 0 passed; 1 failed; 0 ignored\n";
        let d = run("cargo test", Some(101), false, stderr);
        assert_eq!(d.summary.failing_tests.len(), 1);
        assert_eq!(d.summary.failing_tests[0].name, "a::b");
    }

    #[test]
    fn never_panics_on_garbage() {
        let _ = run("cargo test", Some(101), false, "\u{0}\u{1}garbage\x1b[31m");
    }
}
