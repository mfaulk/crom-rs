//! M11 Planning subsystem.
//!
//! Plans are structured artifacts the model drafts during planning mode and
//! the user reviews, approves, revises, or rejects before letting the agent
//! execute. The [`PlanDocument`] captures everything needed to evaluate a
//! plan offline: goal, steps, expected tools/files/commands, validations,
//! risks, and provenance.
//!
//! This module is pure data + storage + scope-classification logic. The
//! agent loop integration (planning generator, act-mode driver, internal
//! `SubmitPlan` / `PlanUpdate` tools) live in submodules below.

mod artifact;
mod deviation;
mod driver;
mod errors;
mod options;
mod scope;
mod store;
mod tools;

pub use artifact::{
    new_plan_id, PlanApproval, PlanApprovalScope, PlanApprover, PlanCheckpointExpectation,
    PlanCommand, PlanContextItem, PlanCreatedBy, PlanDocument, PlanExecutionStatus,
    PlanExecutionSummary, PlanFileAccess, PlanFileRef, PlanId, PlanRisk, PlanRiskLevel, PlanStatus,
    PlanStep, PlanStepKind, PlanStepStatus, PlanSummary, PlanToolUse, PlanValidation,
    WorkspaceRelativePath, MAX_PLAN_BYTES_DEFAULT, MAX_PLAN_REVISIONS_DEFAULT,
    MAX_PLAN_STEPS_DEFAULT,
};
pub use deviation::{
    all_steps_terminal, classify_tool_call, default_severity_for, next_pending_step,
    redact_secrets_in_plan, PlanDeviation, PlanDeviationDecision, PlanDeviationSeverity,
    PlanScopeDecision,
};
pub use driver::{
    act_mode_system_prompt, act_plan, collect_policy_decisions, generate_plan,
    planning_allowed_tool_names, planning_system_prompt, ActOutcome, ActRequest, DenyAllDeviations,
    PlanDeviationCallback, PlanGenerationOutcome, PlanRequest,
};
pub use errors::PlanError;
pub use options::{PlanDeviationPolicy, PlanningMode, PlanningOptions};
pub use scope::{plan_scope_summary, PlanExecutionContext, PlanScopeSummary};
pub use store::{FsPlanStore, PlanApprovalRequest, PlanListFilter, PlanRejectRequest, PlanStore};
pub use tools::{
    parse_submit_plan_input, plan_update_tool_definition, render_plan_pretty,
    submit_plan_tool_definition, PlanUpdateTool, SubmitPlanResult, SubmitPlanTool,
    PLAN_UPDATE_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME,
};
