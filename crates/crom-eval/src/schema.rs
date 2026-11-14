//! Schema-versioned manifest extensions (M15).
//!
//! M14 task manifests are treated as **schema version 1**. M15 introduces
//! version 2, which adds capability labels, behavior rules, hidden
//! verification, quarantine, visibility, prompt variants, and explicit
//! required/forbidden path policy. Version 1 manifests continue to parse and
//! run unchanged: every v2 field has a default, and the version defaults to 1
//! when omitted.
//!
//! See `docs/eval-schema.md` for the documented surface.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::task::{AssertionSpec, OracleKind, OracleSpec};

/// Schema versions this build understands. Manifests declaring a version
/// outside this set are rejected unless an explicit compatibility flag is set.
pub const SUPPORTED_SCHEMA_VERSIONS: &[u32] = &[1, 2];

/// Default schema version applied when a manifest omits `schema_version`.
/// M14 manifests have no version field, so they default to v1.
pub const DEFAULT_SCHEMA_VERSION: u32 = 1;

pub(crate) fn default_schema_version() -> u32 {
    DEFAULT_SCHEMA_VERSION
}

/// Validate a declared schema version against this build.
///
/// `allow_future` corresponds to the `--allow-future-schema` compatibility
/// flag; when set, an unknown *higher* version is accepted with a warning
/// rather than rejected.
pub fn validate_schema_version(version: u32, allow_future: bool) -> Result<(), String> {
    if SUPPORTED_SCHEMA_VERSIONS.contains(&version) {
        return Ok(());
    }
    let max = SUPPORTED_SCHEMA_VERSIONS.iter().copied().max().unwrap_or(0);
    if version > max && allow_future {
        return Ok(());
    }
    Err(format!(
        "unsupported schema_version {version}; this build supports {SUPPORTED_SCHEMA_VERSIONS:?}\
         (pass --allow-future-schema to attempt a newer manifest)"
    ))
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

    #[test]
    fn schema_version_validation() {
        assert!(validate_schema_version(1, false).is_ok());
        assert!(validate_schema_version(2, false).is_ok());
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
