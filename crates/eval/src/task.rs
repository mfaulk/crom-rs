//! Task manifest types and discovery.
//!
//! Tasks are described by a `task.toml` manifest plus an adjacent fixture
//! directory. A manifest deserializes into [`Manifest`]; [`Manifest::into_task`]
//! resolves it against its directory to produce an [`EvalTask`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A resolved, ready-to-run evaluation task.
#[derive(Debug, Clone)]
pub struct EvalTask {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    pub prompt: String,
    pub fixture: FixtureSpec,
    pub allowed_tools: Vec<String>,
    pub max_turns: usize,
    pub timeout: Duration,
    pub oracle: OracleSpec,
    pub tags: Vec<String>,
    /// How the agent is asked to operate (modify workspace vs plan-then-act).
    pub mode: TaskMode,
    /// Named suites this task belongs to (e.g. `smoke`, `golden`, `rust-small`).
    pub suites: Vec<String>,
    /// Optional named golden script used by the golden provider mode. Defaults
    /// to the task id when omitted.
    pub golden_script: Option<String>,
    /// Absolute path to the directory containing `task.toml` and the fixture.
    pub dir: PathBuf,
}

impl EvalTask {
    /// The golden script name to look up for this task.
    pub fn golden_script_name(&self) -> &str {
        self.golden_script.as_deref().unwrap_or(&self.id)
    }

    /// Whether this task participates in the named suite.
    pub fn in_suite(&self, suite: &str) -> bool {
        self.suites.iter().any(|s| s == suite)
    }
}

/// How the agent should operate for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMode {
    /// Directly mutate the workspace (Write/Edit/Bash subject to policy).
    #[default]
    ModifyWorkspace,
    /// Produce a plan, then act on it (exercises planning behavior).
    PlanThenAct,
}

/// Fixture sourcing strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureKind {
    /// Copy a directory tree into the isolated workspace.
    #[default]
    CopyDir,
    /// Start from an empty workspace.
    Empty,
}

/// Where a task's fixture comes from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FixtureSpec {
    #[serde(default)]
    pub kind: FixtureKind,
    /// Path to the fixture directory, relative to the task directory.
    #[serde(default)]
    pub path: Option<String>,
}

impl Default for FixtureSpec {
    fn default() -> Self {
        Self {
            kind: FixtureKind::CopyDir,
            path: Some("repo".to_string()),
        }
    }
}

/// Oracle primary check kind. Assertions and behavior checks are evaluated
/// in addition to (and independently of) the primary kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OracleKind {
    /// Run an explicit list of shell commands.
    #[default]
    Command,
    /// Expand to common Rust quality gates.
    Rust,
    /// No primary command/rust gate — only assertions and/or behavior.
    None,
}

/// A complete oracle specification for a task.
///
/// The `kind` selects the primary gate (command list or rust gates). File
/// assertions and behavior checks are always evaluated when present, so a
/// single oracle can combine, e.g., a command gate with a `file_contains`
/// assertion and a `must_emit_plan` behavior check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OracleSpec {
    #[serde(default)]
    pub kind: OracleKind,

    // --- Command oracle ---
    #[serde(default)]
    pub commands: Vec<String>,

    // --- Rust oracle ---
    #[serde(default)]
    pub fmt: bool,
    #[serde(default)]
    pub test: bool,
    #[serde(default)]
    pub clippy: bool,
    #[serde(default)]
    pub doc_tests: bool,

    // --- File assertions ---
    #[serde(default)]
    pub assertions: Vec<AssertionSpec>,

    // --- Behavior checks ---
    #[serde(default)]
    pub behavior: Vec<BehaviorSpec>,

    /// Per-command timeout override (seconds). Falls back to the task timeout.
    #[serde(default)]
    pub command_timeout_secs: Option<u64>,
}

impl OracleSpec {
    /// Expand the primary kind into a concrete command list. File assertions
    /// and behavior checks are evaluated separately.
    pub fn primary_commands(&self) -> Vec<String> {
        match self.kind {
            OracleKind::Command => self.commands.clone(),
            OracleKind::Rust => {
                let mut cmds = Vec::new();
                if self.fmt {
                    cmds.push("cargo fmt --all --check".to_string());
                }
                if self.test {
                    cmds.push("cargo test --all-targets".to_string());
                }
                if self.clippy {
                    cmds.push("cargo clippy --all-targets -- -D warnings".to_string());
                }
                if self.doc_tests {
                    cmds.push("cargo test --doc".to_string());
                }
                cmds
            }
            OracleKind::None => Vec::new(),
        }
    }

    /// Total number of discrete checks this oracle will evaluate.
    pub fn check_count(&self) -> usize {
        self.primary_commands().len() + self.assertions.len() + self.behavior.len()
    }
}

/// A file-content assertion evaluated against the post-run workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssertionSpec {
    /// File exists and contains `text`.
    FileContains { path: String, text: String },
    /// File exists and does not contain `text`.
    FileNotContains { path: String, text: String },
    /// File exists.
    FileExists { path: String },
    /// File does not exist.
    FileMissing { path: String },
}

impl AssertionSpec {
    pub fn describe(&self) -> String {
        match self {
            Self::FileContains { path, text } => format!("{path} contains {text:?}"),
            Self::FileNotContains { path, text } => format!("{path} does not contain {text:?}"),
            Self::FileExists { path } => format!("{path} exists"),
            Self::FileMissing { path } => format!("{path} is absent"),
        }
    }
}

/// A behavior check evaluated against the structured event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BehaviorSpec {
    /// The agent must emit a plan (a `PlanCreated` event).
    MustEmitPlan,
    /// The agent must call the named tool at least once.
    MustUseTool { tool: String },
    /// The agent must never call the named tool.
    MustNotUseTool { tool: String },
    /// The agent must run a Bash command whose text contains the substring.
    MustRunCommand { command_contains: String },
    /// The agent must never run a Bash command containing the substring.
    MustNotRunCommand { command_contains: String },
}

impl BehaviorSpec {
    pub fn describe(&self) -> String {
        match self {
            Self::MustEmitPlan => "must emit a plan".to_string(),
            Self::MustUseTool { tool } => format!("must use tool {tool}"),
            Self::MustNotUseTool { tool } => format!("must not use tool {tool}"),
            Self::MustRunCommand { command_contains } => {
                format!("must run command containing {command_contains:?}")
            }
            Self::MustNotRunCommand { command_contains } => {
                format!("must not run command containing {command_contains:?}")
            }
        }
    }
}

fn default_max_turns() -> usize {
    20
}

fn default_timeout_secs() -> u64 {
    180
}

/// On-disk task manifest (`task.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Tools the agent is allowed to use. Empty means "all built-in tools".
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub mode: TaskMode,
    #[serde(default)]
    pub suites: Vec<String>,
    #[serde(default)]
    pub golden_script: Option<String>,
    #[serde(default)]
    pub fixture: FixtureSpec,
    #[serde(default)]
    pub oracle: OracleSpec,
}

impl Manifest {
    /// Parse a manifest from TOML text.
    pub fn from_toml(text: &str) -> Result<Self, TaskError> {
        toml::from_str(text).map_err(|e| TaskError::Parse(e.to_string()))
    }

    /// Resolve a manifest against the directory containing it.
    pub fn into_task(self, dir: impl Into<PathBuf>) -> EvalTask {
        EvalTask {
            id: self.id,
            title: self.title,
            description: self.description,
            prompt: self.prompt,
            fixture: self.fixture,
            allowed_tools: self.allowed_tools,
            max_turns: self.max_turns,
            timeout: Duration::from_secs(self.timeout_secs),
            oracle: self.oracle,
            tags: self.tags,
            mode: self.mode,
            suites: self.suites,
            golden_script: self.golden_script,
            dir: dir.into(),
        }
    }
}

/// Load a single task from a `task.toml` file path.
pub fn load_task(manifest_path: &Path) -> Result<EvalTask, TaskError> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| TaskError::Io(format!("{}: {e}", manifest_path.display())))?;
    let manifest = Manifest::from_toml(&text)?;
    let dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(manifest.into_task(dir))
}

/// Discover all tasks under a tasks directory.
///
/// Each task lives in its own subdirectory containing a `task.toml`. Results
/// are sorted by task id for deterministic ordering.
pub fn discover_tasks(tasks_dir: &Path) -> Result<Vec<EvalTask>, TaskError> {
    let mut tasks = Vec::new();
    if !tasks_dir.exists() {
        return Err(TaskError::Io(format!(
            "tasks directory not found: {}",
            tasks_dir.display()
        )));
    }
    let entries = std::fs::read_dir(tasks_dir)
        .map_err(|e| TaskError::Io(format!("{}: {e}", tasks_dir.display())))?;
    for entry in entries {
        let entry = entry.map_err(|e| TaskError::Io(e.to_string()))?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest_path = path.join("task.toml");
        if manifest_path.exists() {
            tasks.push(load_task(&manifest_path)?);
        }
    }
    tasks.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(tasks)
}

/// Errors that arise while loading or parsing tasks.
#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("task io error: {0}")]
    Io(String),
    #[error("task manifest parse error: {0}")]
    Parse(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
id = "rust-cli-arg-parsing-bug"
title = "Fix CLI arg parsing bug"
tags = ["rust", "bugfix", "tests", "small"]
prompt = "Fix the bug."
max_turns = 20
timeout_secs = 180
suites = ["rust-small"]

[fixture]
kind = "copy_dir"
path = "repo"

[oracle]
kind = "command"
commands = ["cargo test --all-targets", "cargo clippy --all-targets -- -D warnings"]

[[oracle.assertions]]
kind = "file_contains"
path = "src/lib.rs"
text = "pub fn parse_args"

[[oracle.behavior]]
kind = "must_use_tool"
tool = "Read"

[[oracle.behavior]]
kind = "must_run_command"
command_contains = "cargo test"
"#;

    #[test]
    fn parses_full_manifest() {
        let m = Manifest::from_toml(SAMPLE).expect("parse");
        let task = m.into_task("/tmp/task");
        assert_eq!(task.id, "rust-cli-arg-parsing-bug");
        assert_eq!(task.max_turns, 20);
        assert_eq!(task.timeout, Duration::from_secs(180));
        assert_eq!(task.fixture.kind, FixtureKind::CopyDir);
        assert_eq!(task.oracle.kind, OracleKind::Command);
        assert_eq!(task.oracle.primary_commands().len(), 2);
        assert_eq!(task.oracle.assertions.len(), 1);
        assert_eq!(task.oracle.behavior.len(), 2);
        assert_eq!(task.oracle.check_count(), 5);
        assert!(task.in_suite("rust-small"));
    }

    #[test]
    fn rust_oracle_expands_to_gates() {
        let toml = r#"
id = "x"
title = "x"
prompt = "x"
[oracle]
kind = "rust"
fmt = true
test = true
clippy = true
doc_tests = false
"#;
        let m = Manifest::from_toml(toml).expect("parse");
        let task = m.into_task(".");
        let cmds = task.oracle.primary_commands();
        assert_eq!(cmds.len(), 3);
        assert!(cmds.iter().any(|c| c.contains("fmt")));
        assert!(cmds.iter().any(|c| c.contains("test --all-targets")));
        assert!(cmds.iter().any(|c| c.contains("clippy")));
        assert!(!cmds.iter().any(|c| c.contains("--doc")));
    }

    #[test]
    fn manifest_defaults_apply() {
        let toml = r#"
id = "x"
title = "x"
prompt = "do it"
"#;
        let m = Manifest::from_toml(toml).expect("parse");
        assert_eq!(m.max_turns, 20);
        assert_eq!(m.timeout_secs, 180);
        let task = m.into_task(".");
        assert_eq!(task.fixture.kind, FixtureKind::CopyDir);
        assert_eq!(task.golden_script_name(), "x");
    }

    #[test]
    fn behavior_and_assertion_descriptions() {
        let b = BehaviorSpec::MustRunCommand {
            command_contains: "cargo test".to_string(),
        };
        assert!(b.describe().contains("cargo test"));
        let a = AssertionSpec::FileMissing {
            path: "x".to_string(),
        };
        assert!(a.describe().contains("absent"));
    }
}
