//! Coding-agent capability evaluation harness.
//!
//! `code-agent-eval` runs the agent against controlled coding tasks in
//! isolated workspaces, scores the result, and emits debuggable artifacts.
//! It answers a question that unit tests cannot: given a realistic repository
//! task, can the agent inspect code, plan, edit files, run checks, recover
//! from failures, and produce a correct patch?
//!
//! The harness drives the agent exclusively through the headless
//! [`code_agent_sdk`] runtime — it never shells out to the CLI and never
//! builds a separate mini-agent. Behavior is judged from structured SDK
//! events, not by scraping model prose.
//!
//! # Layout
//!
//! - [`task`] — task manifest types ([`EvalTask`], [`FixtureSpec`],
//!   [`OracleSpec`]) and discovery.
//! - [`fixture`] — isolated workspace setup, git baseline, patch generation.
//! - [`providers`] — fake / golden / live provider execution modes.
//! - [`runner`] — runs one task or a suite end-to-end.
//! - [`oracle`] — command / rust / file-assertion / behavior oracles.
//! - [`score`] — weighted structured scoring.
//! - [`artifacts`] — per-run / per-task artifact bundle.
//! - [`report`] — human-readable reports and run summaries.

pub mod adapter;
pub mod artifacts;
pub mod behavior;
pub mod external;
pub mod failure;
pub mod fixture;
pub mod index;
pub mod isolation;
pub mod multirun;
pub mod oracle;
pub mod providers;
pub mod repair;
pub mod report;
pub mod runner;
pub mod schema;
pub mod score;
pub mod task;
pub mod triage;
pub mod validate;

pub use adapter::{AdapterRegistry, EvalAdapter};
pub use behavior::BehaviorMetrics;
pub use failure::FailureClass;
pub use isolation::{EnvMetadata, IsolationBackend, LocalBackend};
pub use oracle::{AssertionResult, BehaviorResult, CommandResult, OracleReport};
pub use providers::{ProviderMode, ProviderSetup, ProviderUnavailable};
pub use repair::{
    FinalValidationStatus, RepairMetrics, RepairSession, RepairTrace, ValidationAttempt,
};
pub use runner::{EvalResult, EvalStatus, RunConfig};
pub use schema::{
    BehaviorRules, Capabilities, HiddenSpec, PromptVariants, Quarantine, Visibility,
    SUPPORTED_SCHEMA_VERSIONS,
};
pub use score::EvalScore;
pub use task::{
    AssertionSpec, BehaviorSpec, EvalTask, FixtureKind, FixtureSpec, OracleKind, OracleSpec,
    TaskMode,
};

/// Default directory (relative to the eval crate root) that holds task
/// definitions and fixtures.
pub const DEFAULT_TASKS_SUBDIR: &str = "eval/tasks";

/// Root directory under `target/` where eval runs write their artifacts.
pub const RUNS_ROOT: &str = "target/code-agent-eval/runs";

/// Environment variable that opts a process into running live-provider evals.
pub const LIVE_ENV: &str = "CODE_AGENT_EVAL_LIVE";
