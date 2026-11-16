//! Schema-versioned manifest extensions.
//!
//! M14 manifests are schema v1; M15 added v2 (capabilities, behavior rules,
//! hidden verification, quarantine, visibility, prompt variants, path policy);
//! M17 added v3 (content version, capability level, task kind, provenance,
//! limits, network, toolchain, reference, expected outcome). Every later-version
//! field has a default, so older manifests parse unchanged, but v3-only fields
//! are rejected in v1/v2 manifests (see `Manifest::from_toml_validated`).
//!
//! See `docs/eval-schema.md` for the documented surface.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::task::{AssertionSpec, OracleKind, OracleSpec};

/// Schema versions this build understands.
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1, 2, 3];

/// Default schema version applied when a manifest omits `schema_version`.
/// M14 manifests have no version field, so they default to v1.
pub const DEFAULT_SCHEMA_VERSION: u32 = 1;

pub(crate) fn default_schema_version() -> u32 {
    DEFAULT_SCHEMA_VERSION
}

/// Validate a declared schema version against this build.
///
/// `allow_future` is a library-level escape hatch (no CLI flag exposes it):
/// when set, an unknown *higher* version is accepted rather than rejected.
pub fn validate_schema_version(version: u32, allow_future: bool) -> Result<(), String> {
    if SUPPORTED_SCHEMA_VERSIONS.contains(&version) {
        return Ok(());
    }
    let max = SUPPORTED_SCHEMA_VERSIONS.iter().copied().max().unwrap_or(0);
    if version > max && allow_future {
        return Ok(());
    }
    Err(format!(
        "unsupported schema_version {version}; this build supports {SUPPORTED_SCHEMA_VERSIONS:?}"
    ))
}

/// Capability level (M17 capability ladder). `L0` is runtime/harness
/// correctness; `L1`..`L5` are increasingly demanding coding capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Level {
    L0,
    L1,
    L2,
    L3,
    L4,
    L5,
}

impl Level {
    pub fn as_str(&self) -> &'static str {
        match self {
            Level::L0 => "L0",
            Level::L1 => "L1",
            Level::L2 => "L2",
            Level::L3 => "L3",
            Level::L4 => "L4",
            Level::L5 => "L5",
        }
    }
}

/// What a task measures. Only `capability` tasks belong in capability pass
/// rates; `harness` tasks validate the runtime and scripted providers and
/// succeed by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    Harness,
    Capability,
    Security,
}

impl TaskKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskKind::Harness => "harness",
            TaskKind::Capability => "capability",
            TaskKind::Security => "security",
        }
    }
}

/// Where a task came from and under what terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Provenance {
    pub author: Option<String>,
    /// e.g. `original`, an upstream repo/issue URL, or a benchmark name.
    pub source: Option<String>,
    /// SPDX expression for the fixture/prompt content.
    pub license: Option<String>,
    /// ISO date (YYYY-MM-DD) the task was authored.
    pub created: Option<String>,
    /// Upstream repository a real-repo fixture was taken from.
    pub upstream: Option<String>,
    /// Upstream commit the fixture snapshots (the fix commit's parent).
    pub base_commit: Option<String>,
    /// Upstream commit whose tests are the hidden oracle.
    pub fix_commit: Option<String>,
}

/// Declared per-task resource limits. `wall_secs` and `max_turns` override the
/// top-level `timeout_secs` / `max_turns`; the other fields are carried on the
/// task but not yet enforced by the runner.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskLimits {
    pub wall_secs: Option<u64>,
    pub cpu: Option<f64>,
    pub memory_mb: Option<u64>,
    pub max_turns: Option<usize>,
    pub max_tool_calls: Option<usize>,
    pub max_cost_usd: Option<f64>,
}

impl TaskLimits {
    pub fn is_empty(&self) -> bool {
        self == &TaskLimits::default()
    }

    /// Reject zero/negative limits; a zero limit is always a typo.
    pub fn validate(&self) -> Result<(), String> {
        for (name, v) in [("wall_secs", self.wall_secs), ("memory_mb", self.memory_mb)] {
            if v == Some(0) {
                return Err(format!("limits.{name} must be > 0"));
            }
        }
        for (name, v) in [
            ("max_turns", self.max_turns),
            ("max_tool_calls", self.max_tool_calls),
        ] {
            if v == Some(0) {
                return Err(format!("limits.{name} must be > 0"));
            }
        }
        if self.cpu.is_some_and(|c| !(c.is_finite() && c > 0.0)) {
            return Err("limits.cpu must be > 0".to_string());
        }
        if self
            .max_cost_usd
            .is_some_and(|c| !(c.is_finite() && c >= 0.0))
        {
            return Err("limits.max_cost_usd must be >= 0".to_string());
        }
        Ok(())
    }
}

/// Network access the task's verification needs. `registry` means a package
/// registry (e.g. crates.io) must be reachable and implies `requires_network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    None,
    Registry,
}

/// How a task's reference solution is supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    /// `oracle_patch` applied to the fixture is the reference; it must apply
    /// and pass public + hidden verification.
    OraclePatch,
    /// The unmodified fixture is the reference (`expected_outcome = no_change`
    /// only); the fixture itself must pass public + hidden verification.
    ValidatedOracle,
}

/// Whether a correct solution changes the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectedOutcome {
    #[default]
    Change,
    NoChange,
}

/// Task visibility. Private suites are omitted from default listings and their
/// hidden contents/prompts are redacted in reports unless explicitly opted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    #[default]
    Public,
    Private,
}

impl Visibility {
    pub fn is_private(&self) -> bool {
        matches!(self, Visibility::Private)
    }
}

/// Capability labels describing what a task exercises. Reports aggregate by
/// these so capability-level regressions are visible.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub requires_repo_search: bool,
    pub requires_test_feedback: bool,
    pub requires_multifile_edit: bool,
    pub requires_public_api_preservation: bool,
    pub requires_cli_execution: bool,
    pub requires_async_reasoning: bool,
    pub requires_feature_flags: bool,
    pub negative_noop: bool,
    pub safety_sensitive: bool,
}

impl Capabilities {
    /// The set of enabled capability labels, as stable string keys. Used for
    /// report aggregation and index entries.
    pub fn enabled_labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        let pairs: [(&'static str, bool); 9] = [
            ("requires_repo_search", self.requires_repo_search),
            ("requires_test_feedback", self.requires_test_feedback),
            ("requires_multifile_edit", self.requires_multifile_edit),
            (
                "requires_public_api_preservation",
                self.requires_public_api_preservation,
            ),
            ("requires_cli_execution", self.requires_cli_execution),
            ("requires_async_reasoning", self.requires_async_reasoning),
            ("requires_feature_flags", self.requires_feature_flags),
            ("negative_noop", self.negative_noop),
            ("safety_sensitive", self.safety_sensitive),
        ];
        for (label, on) in pairs {
            if on {
                out.push(label);
            }
        }
        out
    }
}

/// Behavior rules: quality expectations evaluated from the event/patch
/// timeline. These surface as quality signals in reports; they do not override
/// primary correctness unless a task opts into it via `enforce`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorRules {
    pub require_read_before_write: bool,
    pub require_validation_after_edit: bool,
    pub penalize_test_deletion: bool,
    pub penalize_large_blind_edit: bool,
    pub penalize_lockfile_change: bool,
    /// When true, a violated behavior rule downgrades the task to not-resolved.
    /// Defaults to false: behavior rules are advisory quality signals.
    pub enforce: bool,
}

impl BehaviorRules {
    pub fn any_enabled(&self) -> bool {
        self.require_read_before_write
            || self.require_validation_after_edit
            || self.penalize_test_deletion
            || self.penalize_large_blind_edit
            || self.penalize_lockfile_change
    }
}

/// Quarantine metadata. Quarantined tasks are reported separately and do not
/// count as ordinary pass/fail unless the caller explicitly asks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quarantine {
    #[serde(default = "quarantine_default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub reason: Option<String>,
    /// ISO date (YYYY-MM-DD) after which the quarantine should be revisited.
    #[serde(default)]
    pub expires: Option<String>,
}

fn quarantine_default_enabled() -> bool {
    true
}

impl Quarantine {
    pub fn is_active(&self) -> bool {
        self.enabled
    }
}

/// Named prompt variants. The default prompt is always the manifest `prompt`;
/// variants offer alternative phrasings (concise/verbose/bug_report/...).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptVariants {
    #[serde(flatten)]
    pub variants: BTreeMap<String, String>,
}

impl PromptVariants {
    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.variants.get(name).map(String::as_str)
    }

    pub fn names(&self) -> Vec<&str> {
        self.variants.keys().map(String::as_str).collect()
    }

    /// Deterministically pick a variant name given a seed (used by
    /// `--prompt-variant random`). Returns `None` when no variants exist.
    pub fn pick_random(&self, seed: u64) -> Option<&str> {
        if self.variants.is_empty() {
            return None;
        }
        let names = self.names();
        let idx = (seed as usize) % names.len();
        Some(names[idx])
    }
}

/// Hidden verification: checks (and optional fixture overlay) applied only
/// **after** the agent completes. Hidden files are never present in the
/// workspace during the agent run, so the agent cannot overfit to them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HiddenSpec {
    /// Optional directory (relative to the task dir) overlaid into the
    /// workspace after the agent finishes, before hidden verification runs.
    #[serde(default)]
    pub fixture: Option<String>,
    /// The hidden oracle (commands / rust gates / assertions).
    #[serde(default)]
    pub oracle: OracleSpec,
}

impl HiddenSpec {
    pub fn has_checks(&self) -> bool {
        self.oracle.check_count() > 0
    }
}

/// Convenience constructor used by adapters/tests to build a command-only
/// hidden oracle.
pub fn command_oracle(commands: Vec<String>) -> OracleSpec {
    OracleSpec {
        kind: OracleKind::Command,
        commands,
        ..Default::default()
    }
}

/// Convenience constructor for an assertion-only oracle.
pub fn assertion_oracle(assertions: Vec<AssertionSpec>) -> OracleSpec {
    OracleSpec {
        kind: OracleKind::None,
        assertions,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Upstream-derived tasks record the upstream repo and the base/fix commits.
    #[test]
    fn provenance_records_upstream_commits() {
        let p: Provenance = toml::from_str(
            r#"
            source = "upstream"
            license = "MIT OR Apache-2.0"
            upstream = "https://github.com/example/repo"
            base_commit = "0123456789abcdef0123456789abcdef01234567"
            fix_commit = "89abcdef0123456789abcdef0123456789abcdef"
            "#,
        )
        .unwrap();
        assert_eq!(
            p.upstream.as_deref(),
            Some("https://github.com/example/repo")
        );
        assert_eq!(
            p.base_commit.as_deref(),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
        assert!(p.fix_commit.is_some());
        assert!(toml::from_str::<Provenance>("bogus = 1").is_err());
    }

    /// Unsupported-version errors list supported versions and do not reference a nonexistent CLI flag.
    #[test]
    fn unsupported_schema_error_has_no_cli_flag_hint() {
        let err = validate_schema_version(99, false).unwrap_err();
        assert!(err.contains("[1, 2, 3]"), "{err}");
        assert!(!err.contains("--allow-future-schema"), "{err}");
    }

    /// Level/kind/network/reference/outcome enums use their documented TOML spellings.
    #[test]
    fn v3_enums_use_documented_spellings() {
        #[derive(Deserialize)]
        struct W {
            level: Level,
            kind: TaskKind,
            network: NetworkPolicy,
            reference: ReferenceKind,
            expected_outcome: ExpectedOutcome,
        }
        let w: W = toml::from_str(
            r#"
level = "L3"
kind = "capability"
network = "registry"
reference = "validated_oracle"
expected_outcome = "no_change"
"#,
        )
        .unwrap();
        assert_eq!(w.level, Level::L3);
        assert_eq!(w.kind, TaskKind::Capability);
        assert_eq!(w.network, NetworkPolicy::Registry);
        assert_eq!(w.reference, ReferenceKind::ValidatedOracle);
        assert_eq!(w.expected_outcome, ExpectedOutcome::NoChange);
        assert!(toml::from_str::<W>("level = \"L9\"\nkind=\"harness\"\nnetwork=\"none\"\nreference=\"oracle_patch\"\nexpected_outcome=\"change\"").is_err());
    }

    /// Schema versions 1, 2, and 3 are accepted; others only via the future-compat path.
    #[test]
    fn schema_version_validation() {
        assert!(validate_schema_version(1, false).is_ok());
        assert!(validate_schema_version(2, false).is_ok());
        assert!(validate_schema_version(3, false).is_ok());
        assert!(validate_schema_version(99, false).is_err());
        // Future version accepted only with the compat flag.
        assert!(validate_schema_version(99, true).is_ok());
        // A lower-but-unknown version is never silently accepted.
        assert!(validate_schema_version(0, true).is_err());
    }

    #[test]
    fn capability_labels() {
        let caps = Capabilities {
            requires_repo_search: true,
            requires_multifile_edit: true,
            ..Default::default()
        };
        let labels = caps.enabled_labels();
        assert!(labels.contains(&"requires_repo_search"));
        assert!(labels.contains(&"requires_multifile_edit"));
        assert!(!labels.contains(&"safety_sensitive"));
    }

    #[test]
    fn prompt_variant_random_is_deterministic() {
        let mut variants = BTreeMap::new();
        variants.insert("concise".to_string(), "c".to_string());
        variants.insert("verbose".to_string(), "v".to_string());
        let pv = PromptVariants { variants };
        // Same seed -> same pick.
        assert_eq!(pv.pick_random(0), pv.pick_random(0));
        assert_eq!(pv.pick_random(2), pv.pick_random(0)); // wraps modulo len
        assert!(pv.get("concise").is_some());
    }
}
