//! Configuration knobs for planning + act mode.

use serde::{Deserialize, Serialize};

use crate::permission::PermissionMode;
use crate::plan::artifact::{
    MAX_PLAN_BYTES_DEFAULT, MAX_PLAN_REVISIONS_DEFAULT, MAX_PLAN_STEPS_DEFAULT,
};

/// What the agent is allowed to do during plan generation.
///
/// `NoTools` is the strictest — only the internal `SubmitPlan` tool is
/// available; the model writes the entire plan from prior context.
/// `ReadOnlyWorkspace` additionally lets the model use Read/Glob/Grep so it
/// can ground the plan in actual files. `ReadOnlyWithApprovedExternal`
/// further opts in to allow-listed read-only MCP tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanningMode {
    NoTools,
    #[default]
    ReadOnlyWorkspace,
    ReadOnlyWithApprovedExternal,
}

impl PlanningMode {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "no_tools" | "no-tools" | "notools" => Self::NoTools,
            "read_only" | "read-only" | "read_only_workspace" | "read-only-workspace" => {
                Self::ReadOnlyWorkspace
            }
            "read_only_external"
            | "read-only-external"
            | "read_only_with_approved_external"
            | "read-only-with-approved-external" => Self::ReadOnlyWithApprovedExternal,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::NoTools => "no_tools",
            Self::ReadOnlyWorkspace => "read_only_workspace",
            Self::ReadOnlyWithApprovedExternal => "read_only_with_approved_external",
        }
    }

    /// Built-in tool names that are *candidate* allowed during planning. The
    /// runtime still applies project-level allow/disallow rules on top.
    pub fn allowed_planning_tools(self) -> &'static [&'static str] {
        match self {
            Self::NoTools => &[],
            Self::ReadOnlyWorkspace | Self::ReadOnlyWithApprovedExternal => {
                &["Read", "Glob", "Grep"]
            }
        }
    }

    pub fn allows_external(self) -> bool {
        matches!(self, Self::ReadOnlyWithApprovedExternal)
    }
}

/// Behavior when an act-mode tool call doesn't match the approved plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PlanDeviationPolicy {
    Deny,
    #[default]
    Ask,
    RequireReplan,
    AllowWithWarning,
}

impl PlanDeviationPolicy {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "deny" => Self::Deny,
            "ask" => Self::Ask,
            "require_replan" | "require-replan" | "replan" => Self::RequireReplan,
            "allow_with_warning" | "allow-with-warning" | "warn" => Self::AllowWithWarning,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::RequireReplan => "require_replan",
            Self::AllowWithWarning => "allow_with_warning",
        }
    }
}

/// Settings for planning + act mode. Constructed from CLI flags or
/// `.agent/settings.json` planning blocks.
#[derive(Debug, Clone)]
pub struct PlanningOptions {
    pub enabled: bool,
    pub planning_mode: PlanningMode,
    pub require_approval_before_act: bool,
    pub enforce_plan_scope: bool,
    pub allow_read_only_tools_during_planning: bool,
    pub allow_external_tools_during_planning: bool,
    pub max_plan_steps: usize,
    pub max_plan_revisions: usize,
    pub max_plan_bytes: usize,
    pub default_act_permission_mode: PermissionMode,
    pub deviation_policy: PlanDeviationPolicy,
    pub persist_plans: bool,
    /// Allow exploratory Read/Glob/Grep in act mode even when not pre-listed
    /// in the plan. Defaults to true so the executor isn't forced to refuse
    /// every "let me look at this file first" probe.
    pub exploratory_reads_during_act: bool,
    /// When true, only commands matching the approved plan's `command_regex`
    /// (or exact command) are allowed in act mode.
    pub strict_commands: bool,
}

impl Default for PlanningOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            planning_mode: PlanningMode::ReadOnlyWorkspace,
            require_approval_before_act: true,
            enforce_plan_scope: true,
            allow_read_only_tools_during_planning: true,
            allow_external_tools_during_planning: false,
            max_plan_steps: MAX_PLAN_STEPS_DEFAULT,
            max_plan_revisions: MAX_PLAN_REVISIONS_DEFAULT,
            max_plan_bytes: MAX_PLAN_BYTES_DEFAULT,
            default_act_permission_mode: PermissionMode::Default,
            deviation_policy: PlanDeviationPolicy::Ask,
            persist_plans: true,
            exploratory_reads_during_act: true,
            strict_commands: true,
        }
    }
}

impl PlanningOptions {
    /// Returns true iff the runtime should restrict the tool registry to
    /// the planning-mode allow list when generating a plan.
    pub fn restricts_tools_during_planning(&self) -> bool {
        // NoTools always restricts; ReadOnly modes still restrict to read-only
        // catalogs so the model cannot smuggle in writes during planning.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // PlanningMode lenient parsing covers expected synonyms.
    fn planning_mode_parse() {
        assert_eq!(
            PlanningMode::from_str_lenient("no-tools").unwrap(),
            PlanningMode::NoTools
        );
        assert_eq!(
            PlanningMode::from_str_lenient("read-only").unwrap(),
            PlanningMode::ReadOnlyWorkspace
        );
        assert_eq!(
            PlanningMode::from_str_lenient("read-only-external").unwrap(),
            PlanningMode::ReadOnlyWithApprovedExternal
        );
        assert!(PlanningMode::from_str_lenient("garbage").is_none());
    }

    #[test]
    // Default planning options match the M11 spec defaults.
    fn defaults_match_spec() {
        let o = PlanningOptions::default();
        assert!(o.enabled);
        assert!(o.require_approval_before_act);
        assert!(o.enforce_plan_scope);
        assert_eq!(o.max_plan_steps, MAX_PLAN_STEPS_DEFAULT);
        assert_eq!(o.deviation_policy, PlanDeviationPolicy::Ask);
        assert_eq!(o.planning_mode, PlanningMode::ReadOnlyWorkspace);
    }
}
