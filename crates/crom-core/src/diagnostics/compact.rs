//! Command-output compaction (M16D).
//!
//! Produces a compact, high-signal rendering of a [`CommandDiagnostic`] for the
//! model, while the raw logs are preserved elsewhere (Bash tool result text and
//! eval artifacts). Compaction is deterministic and never drops the primary
//! failure.

use serde::{Deserialize, Serialize};

use super::model::{CommandDiagnostic, DiagnosticSeverity};
use super::text::{last_lines, truncate_bytes};

/// Knobs for compaction. Mirrors the `[diagnostics.compaction]` config section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionConfig {
    pub max_excerpt_bytes: usize,
    pub max_errors: usize,
    pub max_warnings: usize,
    pub max_failing_tests: usize,
    /// "off" | "short" | "full"
    pub include_backtrace: String,
    pub include_successful_tests: bool,
    /// Number of trailing raw output lines to retain.
    pub tail_lines: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            max_excerpt_bytes: 12_000,
            max_errors: 5,
            max_warnings: 5,
            max_failing_tests: 10,
            include_backtrace: "short".to_string(),
            include_successful_tests: false,
            tail_lines: 20,
        }
    }
}

/// Render a compact, model-facing summary of a diagnostic.
pub fn compact(diag: &CommandDiagnostic, cfg: &CompactionConfig) -> String {
    let mut out = String::new();

    out.push_str(&format!("$ {}\n", diag.command));
    let status = match diag.status {
        super::model::CommandStatus::Success => "succeeded".to_string(),
        super::model::CommandStatus::Timeout => "timed out".to_string(),
        super::model::CommandStatus::Signaled => "killed by signal".to_string(),
        super::model::CommandStatus::SpawnFailure => "failed to start".to_string(),
        super::model::CommandStatus::ExitFailure => {
            format!(
                "exited {}",
                diag.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "?".into())
            )
        }
    };
    out.push_str(&format!("[{status}]\n"));

    // Primary line.
    if !diag.summary.short_message.is_empty() {
        out.push_str("\nPrimary failure:\n");
        out.push_str(&diag.summary.short_message);
        out.push('\n');
    }

    // Errors (capped).
    let errors: Vec<_> = diag
        .parsed
        .iter()
        .filter(|d| d.is_error())
        .take(cfg.max_errors)
        .collect();
    if !errors.is_empty() {
        out.push_str("\nDiagnostics:\n");
        for d in &errors {
            let loc = match (&d.file, d.line) {
                (Some(f), Some(l)) => format!(" ({}:{})", f.display(), l),
                (Some(f), None) => format!(" ({})", f.display()),
                _ => String::new(),
            };
            let code = d
                .code
                .as_deref()
                .map(|c| format!("[{c}] "))
                .unwrap_or_default();
            out.push_str(&format!("- {code}{}{loc}\n", first_line(&d.message)));
            // Include rustc help/note text and suggested fixes.
            for r in d.related.iter().take(2) {
                out.push_str(&format!("    {}\n", first_line(&r.message)));
            }
            for fix in d.suggested_fixes.iter().take(1) {
                if let Some(repl) = &fix.replacement {
                    out.push_str(&format!("    suggestion: {}\n", first_line(repl)));
                }
            }
        }
    }

    // Warnings (capped, only when relevant — i.e. there were no errors).
    if errors.is_empty() {
        let warnings: Vec<_> = diag
            .parsed
            .iter()
            .filter(|d| matches!(d.severity, DiagnosticSeverity::Warning))
            .take(cfg.max_warnings)
            .collect();
        if !warnings.is_empty() {
            out.push_str("\nWarnings:\n");
            for d in warnings {
                out.push_str(&format!("- {}\n", first_line(&d.message)));
            }
        }
    }

    // Failing tests (capped).
    if !diag.summary.failing_tests.is_empty() {
        out.push_str("\nFailing tests:\n");
        for ft in diag
            .summary
            .failing_tests
            .iter()
            .take(cfg.max_failing_tests)
        {
            let loc = match (&ft.file, ft.line) {
                (Some(f), Some(l)) => format!(" ({}:{})", f.display(), l),
                _ => String::new(),
            };
            out.push_str(&format!("- {}{loc}\n", ft.name));
            if let (Some(l), Some(r)) = (&ft.assertion_left, &ft.assertion_right) {
                out.push_str(&format!(
                    "    left:  {}\n    right: {}\n",
                    first_line(l),
                    first_line(r)
                ));
            } else if let Some(msg) = &ft.message {
                out.push_str(&format!("    {}\n", first_line(msg)));
            }
        }
        let total = diag.summary.failing_tests.len();
        if total > cfg.max_failing_tests {
            out.push_str(&format!(
                "  …and {} more failing test(s)\n",
                total - cfg.max_failing_tests
            ));
        }
    }

    // Tail of raw output for context, unless it's pure successful-test spam.
    let raw = if !diag.stderr_excerpt.trim().is_empty() {
        &diag.stderr_excerpt
    } else {
        &diag.stdout_excerpt
    };
    let tail = last_lines(raw, cfg.tail_lines);
    if !tail.trim().is_empty() {
        out.push_str("\nOutput tail:\n");
        out.push_str(&tail);
        out.push('\n');
    }

    truncate_bytes(&out, cfg.max_excerpt_bytes)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s).trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{parse_command_diagnostic, CommandDiagnosticInput};
    use std::path::PathBuf;

    #[test]
    fn primary_error_survives_long_log() {
        // A long log dominated by noise, with one real error.
        let mut stderr = String::new();
        for i in 0..2000 {
            stderr.push_str(&format!("   Compiling crate-{i} v0.1.0\n"));
        }
        stderr.push_str("error[E0308]: mismatched types\n  --> src/config.rs:74:21\n");
        let diag = parse_command_diagnostic(CommandDiagnosticInput {
            command: "cargo test".into(),
            cwd: PathBuf::from("/tmp/x"),
            exit_code: Some(101),
            timed_out: false,
            stdout: String::new(),
            stderr,
            duration_ms: Some(10),
        });
        let cfg = CompactionConfig::default();
        let s = compact(&diag, &cfg);
        assert!(s.contains("E0308"), "primary error must survive: {s}");
        assert!(s.contains("config.rs"));
        assert!(s.len() <= cfg.max_excerpt_bytes + 32);
    }

    #[test]
    fn compaction_is_deterministic() {
        let diag = parse_command_diagnostic(CommandDiagnosticInput {
            command: "cargo test".into(),
            cwd: PathBuf::from("/tmp/x"),
            exit_code: Some(101),
            timed_out: false,
            stdout: String::new(),
            stderr: "error[E0432]: unresolved import `foo`\n  --> src/lib.rs:1:5\n".into(),
            duration_ms: Some(10),
        });
        let cfg = CompactionConfig::default();
        assert_eq!(compact(&diag, &cfg), compact(&diag, &cfg));
    }
}
