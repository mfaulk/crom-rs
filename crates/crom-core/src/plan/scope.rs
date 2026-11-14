//! Helpers exposing the approved plan to the act-mode runtime.

use serde::{Deserialize, Serialize};

use super::artifact::{PlanCommand, PlanDocument, PlanFileRef, PlanId, PlanStep, PlanToolUse};
use super::options::PlanDeviationPolicy;

/// Read-only execution context derived from an approved plan. The act-mode
/// driver uses this to feed the model a concrete summary of what it's
/// allowed to do.
#[derive(Debug, Clone)]
pub struct PlanExecutionContext {
    pub plan_id: PlanId,
    pub plan_version: u32,
    pub enforce_scope: bool,
    pub current_step_id: Option<String>,
    pub approved_tools: Vec<PlanToolUse>,
    pub approved_files: Vec<PlanFileRef>,
    pub approved_commands: Vec<PlanCommand>,
    pub deviation_policy: PlanDeviationPolicy,
}

impl PlanExecutionContext {
    pub fn from_plan(
        plan: &PlanDocument,
        enforce_scope: bool,
        policy: PlanDeviationPolicy,
    ) -> Self {
        let mut approved_tools: Vec<PlanToolUse> = plan.requested_tools.clone();
        let mut approved_files: Vec<PlanFileRef> = plan.files.clone();
        let mut approved_commands: Vec<PlanCommand> = Vec::new();
        for s in &plan.steps {
            for t in &s.expected_tools {
                if !approved_tools.iter().any(|e| e.tool_name == t.tool_name) {
                    approved_tools.push(t.clone());
                }
            }
            for f in &s.expected_files {
                if !approved_files
                    .iter()
                    .any(|e| e.path == f.path && e.access == f.access)
                {
                    approved_files.push(f.clone());
                }
            }
            for c in &s.expected_commands {
                if !approved_commands.iter().any(|e| e.command == c.command) {
                    approved_commands.push(c.clone());
                }
            }
        }
        Self {
            plan_id: plan.id.clone(),
            plan_version: plan.version,
            enforce_scope,
            current_step_id: plan
                .steps
                .iter()
                .find(|s| matches!(s.status, super::artifact::PlanStepStatus::Pending))
                .map(|s| s.id.clone()),
            approved_tools,
            approved_files,
            approved_commands,
            deviation_policy: policy,
        }
    }
}

/// Compact human-readable summary of a plan, used for system prompts and
/// CLI rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanScopeSummary {
    pub plan_id: PlanId,
    pub version: u32,
    pub title: String,
    pub goal: String,
    pub steps: Vec<PlanStep>,
    pub approved_tools: Vec<String>,
    pub approved_files: Vec<String>,
}

pub fn plan_scope_summary(plan: &PlanDocument) -> PlanScopeSummary {
    PlanScopeSummary {
        plan_id: plan.id.clone(),
        version: plan.version,
        title: plan.title.clone(),
        goal: plan.goal.clone(),
        steps: plan.steps.clone(),
        approved_tools: plan
            .requested_tools
            .iter()
            .map(|t| t.tool_name.clone())
            .chain(
                plan.steps
                    .iter()
                    .flat_map(|s| s.expected_tools.iter().map(|t| t.tool_name.clone())),
            )
            .collect(),
        approved_files: plan
            .files
            .iter()
            .map(|f| f.path.as_str().to_string())
            .chain(
                plan.steps
                    .iter()
                    .flat_map(|s| s.expected_files.iter().map(|f| f.path.as_str().to_string())),
            )
            .collect(),
    }
}
