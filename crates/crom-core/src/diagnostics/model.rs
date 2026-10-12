//! Structured data model for command-failure diagnostics (M16A).
//!
//! These types capture the result of running a validation command (a compile,
//! a test run, a lint) in a structured, serializable form so the agent loop,
//! repair controller, session store, and eval artifacts can all reason about
//! failures without re-parsing raw text. Every enum carries an explicit
//! `Unknown` variant: parsing never fabricates a classification it could not
//! derive from the output.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A fully parsed diagnostic for a single command invocation.
///
/// Raw `stdout`/`stderr` are kept here as *excerpts* (capped) so the struct is
/// safe to embed in artifacts; the un-truncated logs are preserved separately
/// by the artifact writer / Bash tool path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDiagnostic {
    pub command: String,
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub status: CommandStatus,
    pub duration_ms: Option<u64>,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub parsed: Vec<ParsedDiagnostic>,
    pub summary: DiagnosticSummary,
    pub fingerprint: DiagnosticFingerprint,
}

impl CommandDiagnostic {
    /// Whether this command represents a failure (anything that is not a clean
    /// successful exit).
    pub fn is_failure(&self) -> bool {
        !matches!(self.status, CommandStatus::Success)
    }
}

/// How a command terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Success,
    ExitFailure,
    Timeout,
    Signaled,
    SpawnFailure,
}

/// A single diagnostic record extracted from command output (one compiler
/// error, one failing test, one lint, ...).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedDiagnostic {
    pub source: DiagnosticSource,
    pub severity: DiagnosticSeverity,
    pub kind: DiagnosticKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rendered_excerpt: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related: Vec<RelatedDiagnostic>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_fixes: Vec<SuggestedFix>,
}

impl ParsedDiagnostic {
    /// A minimal diagnostic carrying just a kind + message; spans and extras
    /// default to empty. Used by parsers and tests as a building block.
    pub fn new(
        source: DiagnosticSource,
        severity: DiagnosticSeverity,
        kind: DiagnosticKind,
        message: impl Into<String>,
    ) -> Self {
        Self {
            source,
            severity,
            kind,
            code: None,
            message: message.into(),
            file: None,
            line: None,
            column: None,
            end_line: None,
            end_column: None,
            rendered_excerpt: None,
            related: Vec::new(),
            suggested_fixes: Vec::new(),
        }
    }

    /// Whether this diagnostic is error-level (counts toward the primary
    /// failure). Warnings and info do not.
    pub fn is_error(&self) -> bool {
        matches!(
            self.severity,
            DiagnosticSeverity::Error | DiagnosticSeverity::Failure | DiagnosticSeverity::Timeout
        )
    }
}

/// A note/help/related-span attached to a primary diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelatedDiagnostic {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<u32>,
}

/// A machine-applicable fix suggestion (e.g. rustc's `help` with a
/// replacement span).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuggestedFix {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applicability: Option<String>,
}

/// Which tool produced a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSource {
    Rustc,
    Cargo,
    CargoTest,
    Clippy,
    Rustfmt,
    TestHarness,
    Panic,
    Shell,
    Unknown,
}

/// Severity of a single diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Failure,
    Timeout,
    Info,
}

/// Coarse classification of a failure, used to drive repair hints and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    SyntaxError,
    TypeMismatch,
    MissingImport,
    MissingCrate,
    MissingTraitImpl,
    TraitBound,
    BorrowChecker,
    Lifetime,
    MoveError,
    MutabilityError,
    VisibilityError,
    FeatureGate,
    MissingFeature,
    UnresolvedName,
    DuplicateDefinition,
    ClippyLint,
    Formatting,
    TestAssertionFailure,
    TestPanic,
    SnapshotMismatch,
    GoldenOutputMismatch,
    CliExitMismatch,
    Timeout,
    CommandNotFound,
    PermissionDenied,
    Unknown,
}

impl DiagnosticKind {
    /// A short, stable, human/machine-readable label (matches the serde name).
    pub fn as_str(&self) -> &'static str {
        match self {
            DiagnosticKind::SyntaxError => "syntax_error",
            DiagnosticKind::TypeMismatch => "type_mismatch",
            DiagnosticKind::MissingImport => "missing_import",
            DiagnosticKind::MissingCrate => "missing_crate",
            DiagnosticKind::MissingTraitImpl => "missing_trait_impl",
            DiagnosticKind::TraitBound => "trait_bound",
            DiagnosticKind::BorrowChecker => "borrow_checker",
            DiagnosticKind::Lifetime => "lifetime",
            DiagnosticKind::MoveError => "move_error",
            DiagnosticKind::MutabilityError => "mutability_error",
            DiagnosticKind::VisibilityError => "visibility_error",
            DiagnosticKind::FeatureGate => "feature_gate",
            DiagnosticKind::MissingFeature => "missing_feature",
            DiagnosticKind::UnresolvedName => "unresolved_name",
            DiagnosticKind::DuplicateDefinition => "duplicate_definition",
            DiagnosticKind::ClippyLint => "clippy_lint",
            DiagnosticKind::Formatting => "formatting",
            DiagnosticKind::TestAssertionFailure => "test_assertion_failure",
            DiagnosticKind::TestPanic => "test_panic",
            DiagnosticKind::SnapshotMismatch => "snapshot_mismatch",
            DiagnosticKind::GoldenOutputMismatch => "golden_output_mismatch",
            DiagnosticKind::CliExitMismatch => "cli_exit_mismatch",
            DiagnosticKind::Timeout => "timeout",
            DiagnosticKind::CommandNotFound => "command_not_found",
            DiagnosticKind::PermissionDenied => "permission_denied",
            DiagnosticKind::Unknown => "unknown",
        }
    }

    /// A short, actionable repair hint for this kind, when one is well known.
    pub fn repair_hint(&self) -> Option<&'static str> {
        let hint = match self {
            DiagnosticKind::TypeMismatch => {
                "Adjust the value or the expected type so they agree; inspect the reported span."
            }
            DiagnosticKind::MissingImport | DiagnosticKind::UnresolvedName => {
                "Add or correct a `use` import, or fix the misspelled path/name."
            }
            DiagnosticKind::MissingCrate => {
                "Add the missing crate to Cargo.toml or correct the crate path."
            }
            DiagnosticKind::TraitBound => {
                "Satisfy the trait bound: add the bound, implement the trait, or change the type."
            }
            DiagnosticKind::MissingTraitImpl => {
                "Implement the missing method/trait, or call an existing one."
            }
            DiagnosticKind::BorrowChecker => {
                "Restructure ownership/borrows; prefer a targeted fix over cloning everything."
            }
            DiagnosticKind::MoveError => {
                "Avoid moving the value, borrow it, or clone only where necessary."
            }
            DiagnosticKind::MutabilityError => {
                "Make the binding/reference mutable where the mutation occurs."
            }
            DiagnosticKind::VisibilityError => {
                "Adjust `pub` visibility or import from an accessible path."
            }
            DiagnosticKind::Formatting => "Run `cargo fmt` to fix formatting.",
            DiagnosticKind::ClippyLint => "Address the clippy lint at the reported span.",
            DiagnosticKind::TestAssertionFailure => {
                "Compare expected vs actual; fix the behavior, not the assertion."
            }
            DiagnosticKind::TestPanic => {
                "Handle the edge case at the panic location (unwrap/index/etc.)."
            }
            DiagnosticKind::CommandNotFound => "Install or correct the command name.",
            _ => return None,
        };
        Some(hint)
    }
}

/// A compact, model-facing summary distilled from the parsed diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticSummary {
    pub primary_kind: DiagnosticKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_line: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failing_tests: Vec<FailingTest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_files: Vec<PathBuf>,
    pub short_message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_hint: Option<String>,
}

impl Default for DiagnosticSummary {
    fn default() -> Self {
        Self {
            primary_kind: DiagnosticKind::Unknown,
            primary_file: None,
            primary_line: None,
            failing_tests: Vec::new(),
            affected_files: Vec::new(),
            short_message: String::new(),
            repair_hint: None,
        }
    }
}

/// A single failing test with its most relevant context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailingTest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion_left: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assertion_right: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_excerpt: String,
}

/// A stable identity for a failure, used to detect repeated identical
/// failures across repair attempts. Two diagnostics with equal fingerprints
/// are treated as "the same failure".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DiagnosticFingerprint {
    pub normalized_command: String,
    pub primary_kind: DiagnosticKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_file: Option<PathBuf>,
    pub normalized_message_hash: String,
}

impl DiagnosticFingerprint {
    /// A short hex digest of the full fingerprint, suitable for artifacts and
    /// terse logging (e.g. `"63f0a1c2"`).
    pub fn short_hash(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.normalized_command.as_bytes());
        hasher.update([0]);
        hasher.update(self.primary_kind.as_str().as_bytes());
        hasher.update([0]);
        hasher.update(self.primary_code.as_deref().unwrap_or("").as_bytes());
        hasher.update([0]);
        hasher.update(
            self.primary_file
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
                .as_bytes(),
        );
        hasher.update([0]);
        hasher.update(self.normalized_message_hash.as_bytes());
        let digest = hasher.finalize();
        hex::encode(&digest[..4])
    }
}

/// Input to the diagnostic parser. Mirrors what the Bash tool already captures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDiagnosticInput {
    pub command: String,
    pub cwd: PathBuf,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: Option<u64>,
}

impl CommandDiagnosticInput {
    pub fn new(command: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            command: command.into(),
            cwd: cwd.into(),
            exit_code: None,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: None,
        }
    }
}
