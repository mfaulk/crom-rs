//! Task manifest types and discovery.
//!
//! Tasks are described by a `task.toml` manifest plus an adjacent fixture
//! directory. A manifest deserializes into [`Manifest`]; [`Manifest::into_task`]
//! resolves it against its directory to produce an [`EvalTask`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::schema::{
    default_schema_version, validate_schema_version, BehaviorRules, Capabilities, HiddenSpec,
    PromptVariants, Quarantine, Visibility,
};

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

    // --- M15 schema v2 additions (all default for v1 manifests) ---
    /// Declared manifest schema version (1 for M14 manifests).
    pub schema_version: u32,
    /// Coarse task category (e.g. `bugfix`, `feature`, `refactor`).
    pub category: Option<String>,
    /// Difficulty label (e.g. `easy`, `medium`, `hard`).
    pub difficulty: Option<String>,
    /// Capability labels describing what the task exercises.
    pub capabilities: Capabilities,
    /// Behavior quality rules (advisory unless `enforce`).
    pub behavior_rules: BehaviorRules,
    /// Quarantine metadata, if the task is quarantined.
    pub quarantine: Option<Quarantine>,
    /// Visibility (public/private).
    pub visibility: Visibility,
    /// Named alternative prompt phrasings.
    pub prompt_variants: PromptVariants,
    /// Hidden verification (applied only after the agent completes).
    pub hidden: Option<HiddenSpec>,
    /// Paths that must exist/be created by a correct solution.
    pub required_paths: Vec<String>,
    /// Paths the agent must not modify (a change is a policy violation).
    pub forbidden_paths: Vec<String>,
    /// Optional oracle patch (relative path) that should solve the task.
    pub oracle_patch: Option<String>,
    /// Whether the task legitimately requires network access during verify.
    pub requires_network: bool,
}

impl EvalTask {
    /// Construct a task with sensible defaults for everything but `id`,
    /// `title`, `prompt`, `oracle`, and `dir`. Used by adapters and tests so
    /// new schema fields don't ripple into every call site.
    pub fn minimal(
        id: impl Into<String>,
        title: impl Into<String>,
        prompt: impl Into<String>,
        oracle: OracleSpec,
        dir: impl Into<PathBuf>,
    ) -> Self {
        EvalTask {
            id: id.into(),
            title: title.into(),
            description: None,
            prompt: prompt.into(),
            fixture: FixtureSpec::default(),
            allowed_tools: Vec::new(),
            max_turns: default_max_turns(),
            timeout: Duration::from_secs(default_timeout_secs()),
            oracle,
            tags: Vec::new(),
            mode: TaskMode::default(),
            suites: Vec::new(),
            golden_script: None,
            dir: dir.into(),
            schema_version: crate::schema::DEFAULT_SCHEMA_VERSION,
            category: None,
            difficulty: None,
            capabilities: Capabilities::default(),
            behavior_rules: BehaviorRules::default(),
            quarantine: None,
            visibility: Visibility::default(),
            prompt_variants: PromptVariants::default(),
            hidden: None,
            required_paths: Vec::new(),
            forbidden_paths: Vec::new(),
            oracle_patch: None,
            requires_network: false,
        }
    }

    /// The golden script name to look up for this task.
    pub fn golden_script_name(&self) -> &str {
        self.golden_script.as_deref().unwrap_or(&self.id)
    }

    /// Whether this task participates in the named suite.
    pub fn in_suite(&self, suite: &str) -> bool {
        self.suites.iter().any(|s| s == suite)
    }

    /// Whether this task is actively quarantined.
    pub fn is_quarantined(&self) -> bool {
        self.quarantine.as_ref().is_some_and(|q| q.is_active())
    }

    /// Whether this task is private.
    pub fn is_private(&self) -> bool {
        self.visibility.is_private()
    }

    /// Resolve the prompt for a requested variant. `None` => the default
    /// prompt. `Some("random")` picks deterministically from `seed`. An unknown
    /// variant name falls back to the default prompt.
    pub fn resolved_prompt(&self, variant: Option<&str>, seed: u64) -> String {
        match variant {
            None => self.prompt.clone(),
            Some("default") => self.prompt.clone(),
            Some("random") => self
                .prompt_variants
                .pick_random(seed)
                .and_then(|name| self.prompt_variants.get(name))
                .unwrap_or(&self.prompt)
                .to_string(),
            Some(name) => self
                .prompt_variants
                .get(name)
                .unwrap_or(&self.prompt)
                .to_string(),
        }
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
    /// An oracle with no primary gate (assertions/behavior only).
    pub fn default_none() -> Self {
        OracleSpec {
            kind: OracleKind::None,
            ..Default::default()
        }
    }

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
    /// Manifest schema version. Absent (M14) => v1.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
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
    /// Single-suite alias merged into `suites` (v2 convenience).
    #[serde(default)]
    pub suite: Option<String>,
    #[serde(default)]
    pub golden_script: Option<String>,
    #[serde(default)]
    pub fixture: FixtureSpec,
    #[serde(default)]
    pub oracle: OracleSpec,

    // --- v2 additions ---
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub difficulty: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub behavior: BehaviorRules,
    #[serde(default)]
    pub quarantine: Option<Quarantine>,
    #[serde(default)]
    pub visibility: Visibility,
    #[serde(default)]
    pub prompt_variants: PromptVariants,
    #[serde(default)]
    pub hidden: Option<HiddenSpec>,
    #[serde(default)]
    pub required_paths: Vec<String>,
    #[serde(default)]
    pub forbidden_paths: Vec<String>,
    #[serde(default)]
    pub oracle_patch: Option<String>,
    #[serde(default)]
    pub requires_network: bool,
}

impl Manifest {
    /// Parse a manifest from TOML text (no schema-version gating).
    pub fn from_toml(text: &str) -> Result<Self, TaskError> {
        toml::from_str(text).map_err(|e| TaskError::Parse(e.to_string()))
    }

    /// Parse and validate a manifest, gating on the declared schema version.
    pub fn from_toml_validated(text: &str, allow_future: bool) -> Result<Self, TaskError> {
        let manifest = Self::from_toml(text)?;
        validate_schema_version(manifest.schema_version, allow_future)
            .map_err(TaskError::Schema)?;
        if manifest.id.trim().is_empty() {
            return Err(TaskError::Validation("task id is empty".to_string()));
        }
        if manifest.prompt.trim().is_empty() {
            return Err(TaskError::Validation(format!(
                "task '{}' has an empty prompt",
                manifest.id
            )));
        }
        Ok(manifest)
    }

    /// Resolve a manifest against the directory containing it.
    pub fn into_task(self, dir: impl Into<PathBuf>) -> EvalTask {
        let mut suites = self.suites;
        if let Some(s) = self.suite {
            if !suites.contains(&s) {
                suites.push(s);
            }
        }
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
            suites,
            golden_script: self.golden_script,
            dir: dir.into(),
            schema_version: self.schema_version,
            category: self.category,
            difficulty: self.difficulty,
            capabilities: self.capabilities,
            behavior_rules: self.behavior,
            quarantine: self.quarantine,
            visibility: self.visibility,
            prompt_variants: self.prompt_variants,
            hidden: self.hidden,
            required_paths: self.required_paths,
            forbidden_paths: self.forbidden_paths,
            oracle_patch: self.oracle_patch,
            requires_network: self.requires_network,
        }
    }
}

/// Load a single task from a `task.toml` file path, validating the schema.
pub fn load_task(manifest_path: &Path) -> Result<EvalTask, TaskError> {
    let text = std::fs::read_to_string(manifest_path)
        .map_err(|e| TaskError::Io(format!("{}: {e}", manifest_path.display())))?;
    let manifest = Manifest::from_toml_validated(&text, false)
        .map_err(|e| TaskError::Parse(format!("{}: {e}", manifest_path.display())))?;
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
    #[error("unsupported schema: {0}")]
    Schema(String),
    #[error("task validation error: {0}")]
    Validation(String),
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
