//! Drivers that translate user planning/act intent into agent runs.
//!
//! [`generate_plan`] builds a planning-mode agent: tools are restricted to
//! the read-only allow list plus the internal `SubmitPlan` tool, the model
//! is asked to draft a plan, and the resulting [`PlanDocument`] is
//! persisted. [`act_plan`] takes an approved plan, restricts the tool
//! registry to what the plan permits, registers `PlanUpdate`, and runs the
//! agent under a plan-scope-aware permission callback.

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::agent::AgentBuilder;
use crate::errors::AgentError;
use crate::events::AgentEvent;
use crate::options::AgentOptions;
use crate::permission::{AlwaysAllow, PermissionCallback, PermissionDecision, PermissionRequest};
use crate::plan::artifact::{
    new_plan_id, PlanCreatedBy, PlanDocument, PlanExecutionStatus, PlanExecutionSummary, PlanId,
    PlanStatus, PlanStepStatus,
};
use crate::plan::deviation::{
    classify_tool_call, default_severity_for, PlanDeviation, PlanScopeDecision,
};
use crate::plan::errors::PlanError;
use crate::plan::options::{PlanDeviationPolicy, PlanningMode, PlanningOptions};
use crate::plan::store::PlanStore;
use crate::plan::tools::{
    parse_submit_plan_input, render_plan_pretty, PlanUpdateHandle, PlanUpdateTool,
    SubmitPlanHandle, SubmitPlanTool, PLAN_UPDATE_TOOL_NAME, SUBMIT_PLAN_TOOL_NAME,
};
use crate::policy::{PolicyDecisionSource, PolicyOutcome};
use crate::provider::ModelProvider;
use crate::tool::ToolRegistry;
use crate::usage::TokenUsage;

/// Inputs for [`generate_plan`].
pub struct PlanRequest {
    pub prompt: String,
    pub options: AgentOptions,
    pub planning: PlanningOptions,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolRegistry,
    pub plan_store: Option<Arc<dyn PlanStore>>,
    pub parent_plan_id: Option<PlanId>,
    pub created_by: PlanCreatedBy,
}

/// Outputs of [`generate_plan`]: the persisted plan document, the events
/// the agent emitted, and a hint about how the plan was produced.
#[derive(Debug)]
pub struct PlanGenerationOutcome {
    pub plan: PlanDocument,
    pub events: Vec<AgentEvent>,
    pub fallback_used: bool,
}

/// Generate a structured plan from a user prompt. The returned plan is
/// persisted to the supplied store (when present) and is in
/// `PendingApproval` status.
pub async fn generate_plan(req: PlanRequest) -> Result<PlanGenerationOutcome, PlanError> {
    if !req.planning.enabled {
        return Err(PlanError::Other("planning is disabled".into()));
    }

    let mut options = req.options.clone();
    apply_planning_tool_restrictions(&mut options, &req.planning);

    // Build a fresh draft plan; the SubmitPlan tool will fill it in.
    let mut draft = PlanDocument::new_draft(new_plan_id(), "", &req.prompt);
    draft.session_id = options.session_id.clone();
    draft.parent_plan_id = req.parent_plan_id.clone();
    draft.created_by = req.created_by;
    draft.model = Some(options.model.clone());

    // SubmitPlan tool: only available during planning. Register it on a
    // copy of the registry so the rest of the world does not gain access.
    let mut tools = req.tools.clone();
    let submit_handle = SubmitPlanHandle::new();
    let submit_tool =
        SubmitPlanTool::new(draft.clone(), submit_handle.clone(), req.planning.clone());
    tools.register(Arc::new(submit_tool));

    // Filter the tool registry to read-only + SubmitPlan.
    let allowed = planning_allowed_tool_names(&req.planning, &options);
    options.allowed_tools = allowed.clone();

    // Augment system prompt with planning instructions.
    let original_system = options.system_prompt.clone();
    options.system_prompt = Some(planning_system_prompt(&req.planning, &original_system));

    // Force a small turn cap for planning unless the user already lowered it.
    if options.max_turns.is_none() {
        options.max_turns = Some(6);
    }

    let agent = AgentBuilder::new()
        .options(options.clone())
        .provider(req.provider.clone())
        .tools(tools.clone())
        .build()
        .map_err(|e: AgentError| PlanError::Other(e.to_string()))?;

    let stream = agent.run(req.prompt.clone());
    let mut collected: Vec<AgentEvent> = Vec::new();
    let mut last_assistant_text = String::new();
    futures::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        if let AgentEvent::AssistantMessage(msg) = &event {
            for blk in &msg.content {
                if let crate::conversation::AssistantContent::Text { text } = blk {
                    last_assistant_text.push_str(text);
                }
            }
        }
        collected.push(event);
    }

    // Take the SubmitPlan handle; if the model didn't call it, parse the
    // assistant text as a textual plan fallback.
    let (mut result, fallback_used) = match submit_handle.take().await {
        Some(r) => (r, false),
        None => {
            let r = parse_submit_plan_input(&last_assistant_text, draft.clone());
            (r, true)
        }
    };
    result.plan.status = PlanStatus::PendingApproval;
    result.plan.session_id = req.options.session_id.clone();
    result.plan.parent_plan_id = req.parent_plan_id.clone();
    result.plan.model = Some(req.options.model.clone());

    if let Some(store) = req.plan_store {
        store.create(result.plan.clone()).await?;
    }

    Ok(PlanGenerationOutcome {
        plan: result.plan,
        events: collected,
        fallback_used,
    })
}

/// Build the planning system prompt that bounds the model's behavior.
pub fn planning_system_prompt(opts: &PlanningOptions, original: &Option<String>) -> String {
    let extra = format!(
        "PLANNING MODE ({mode}). Produce a structured plan by calling the `{name}` tool. \
         Do not modify the workspace. Do not call mutating, execution, or external tools. \
         Each step must list the concrete files, tools, commands, validations, and risks. \
         Steps should be small enough to review. Avoid vague directives like \"fix the bug\". \
         If you need information first, you may call the read-only tools (Read/Glob/Grep). \
         Always finish by calling `{name}` with a complete plan.",
        mode = opts.planning_mode.label(),
        name = SUBMIT_PLAN_TOOL_NAME,
    );
    match original {
        Some(s) => format!("{s}\n\n{extra}"),
        None => extra,
    }
}

/// Tool names allowed during planning given the configured mode and the
/// user's broader allow list. `SubmitPlan` is always included.
pub fn planning_allowed_tool_names(opts: &PlanningOptions, options: &AgentOptions) -> Vec<String> {
    let mut names: Vec<String> = vec![SUBMIT_PLAN_TOOL_NAME.into()];
    if matches!(opts.planning_mode, PlanningMode::NoTools) {
        return names;
    }
    if opts.allow_read_only_tools_during_planning {
        for n in opts.planning_mode.allowed_planning_tools() {
            if !names.iter().any(|x| x == n)
                && (options.allowed_tools.is_empty()
                    || options.allowed_tools.iter().any(|x| x == n))
            {
                names.push((*n).to_string());
            }
        }
    }
    if opts.planning_mode.allows_external() && opts.allow_external_tools_during_planning {
        for n in &options.allowed_tools {
            if n.starts_with("mcp__") && !names.iter().any(|x| x == n) {
                names.push(n.clone());
            }
        }
    }
    names
}

fn apply_planning_tool_restrictions(options: &mut AgentOptions, _planning: &PlanningOptions) {
    // Planning never mutates workspace. We use `Default` permission mode
    // and rely on the restricted tool registry (SubmitPlan + read-only
    // tools) to keep the agent honest. Switching to `Plan` mode would also
    // block our internal SubmitPlan tool because the policy engine treats
    // Plan as an unconditional deny gate.
    options.permission_mode = crate::permission::PermissionMode::Default;
    // Planning runs are short-lived: tighten the turn budget if the user
    // hasn't been explicit so we don't burn a 50-turn agent loop drafting
    // a plan.
    if options.max_turns.is_none() {
        options.max_turns = Some(6);
    }
}

/// Inputs for [`act_plan`].
pub struct ActRequest {
    pub plan_id: PlanId,
    pub options: AgentOptions,
    pub planning: PlanningOptions,
    pub provider: Arc<dyn ModelProvider>,
    pub tools: ToolRegistry,
    pub plan_store: Arc<dyn PlanStore>,
    /// Optional override callback when the policy engine asks for approval.
    /// The driver wraps it so plan-scope deviations are routed through
    /// `deviation_callback` first.
    pub user_callback: Option<Arc<dyn PermissionCallback>>,
    pub deviation_callback: Option<Arc<dyn PlanDeviationCallback>>,
    /// Optional prompt to send to the executing agent. When `None`, a
    /// canned "execute the approved plan" prompt is used.
    pub prompt: Option<String>,
}

/// Outcome of [`act_plan`]: the agent events, the updated plan document
/// (already persisted), and the execution summary.
#[derive(Debug)]
pub struct ActOutcome {
    pub plan: PlanDocument,
    pub events: Vec<AgentEvent>,
    pub deviations: Vec<PlanDeviation>,
    pub summary: PlanExecutionSummary,
}

/// Resolve a deviation interactively. The default implementation denies
/// all deviations.
#[async_trait]
pub trait PlanDeviationCallback: Send + Sync {
    async fn resolve(&self, deviation: PlanDeviation) -> super::deviation::PlanDeviationDecision;
}

/// Default: all deviations are denied. Useful for non-interactive runs.
pub struct DenyAllDeviations;

#[async_trait]
impl PlanDeviationCallback for DenyAllDeviations {
    async fn resolve(&self, _: PlanDeviation) -> super::deviation::PlanDeviationDecision {
        super::deviation::PlanDeviationDecision::Deny
    }
}

/// Execute an approved plan. Refuses to run anything but plans whose
/// status is `Approved`.
pub async fn act_plan(req: ActRequest) -> Result<ActOutcome, PlanError> {
    let mut plan = req.plan_store.load(&req.plan_id).await?;
    if !matches!(plan.status, PlanStatus::Approved) {
        return Err(PlanError::BadStatus(format!(
            "plan {} is not approved (status={:?})",
            plan.id, plan.status
        )));
    }
    let approved_version = plan
        .approval
        .as_ref()
        .map(|a| a.approved_version)
        .unwrap_or(plan.version);
    if approved_version != plan.version {
        return Err(PlanError::VersionMismatch {
            expected: approved_version,
            got: plan.version,
        });
    }

    plan.status = PlanStatus::Executing;
    plan.execution = Some(PlanExecutionSummary {
        started_at: OffsetDateTime::now_utc(),
        completed_at: None,
        status: PlanExecutionStatus::Running,
        completed_steps: Vec::new(),
        failed_steps: Vec::new(),
        deviations: Vec::new(),
        validations: Vec::new(),
        checkpoints: Vec::new(),
        usage: None,
    });
    req.plan_store.save_revision(plan.clone()).await?;

    let mut options = req.options.clone();
    if matches!(
        options.permission_mode,
        crate::permission::PermissionMode::Plan
    ) {
        options.permission_mode = req.planning.default_act_permission_mode;
    }

    // Restrict the active tool registry to:
    // - tools mentioned in the plan
    // - the PlanUpdate internal tool
    // - exploratory read-only tools when configured
    let mut allowed: Vec<String> = vec![PLAN_UPDATE_TOOL_NAME.into()];
    for t in &plan.requested_tools {
        if !allowed.contains(&t.tool_name) {
            allowed.push(t.tool_name.clone());
        }
    }
    for s in &plan.steps {
        for t in &s.expected_tools {
            if !allowed.contains(&t.tool_name) {
                allowed.push(t.tool_name.clone());
            }
        }
    }
    if req.planning.exploratory_reads_during_act {
        for n in ["Read", "Glob", "Grep"].iter() {
            if !allowed.contains(&(*n).to_string()) {
                allowed.push((*n).to_string());
            }
        }
    }
    options.allowed_tools = allowed.clone();

    let mut tools = req.tools.clone();
    let plan_update_handle = PlanUpdateHandle::new();
    let plan_update_tool = PlanUpdateTool::new(plan_update_handle.clone());
    tools.register(Arc::new(plan_update_tool));

    let scope_state = Arc::new(Mutex::new(ScopeState {
        plan: plan.clone(),
        options: req.planning.clone(),
        deviations: Vec::new(),
    }));

    let user_cb: Arc<dyn PermissionCallback> = req
        .user_callback
        .clone()
        .unwrap_or_else(|| Arc::new(AlwaysAllow));
    let dev_cb: Arc<dyn PlanDeviationCallback> = req
        .deviation_callback
        .clone()
        .unwrap_or_else(|| Arc::new(DenyAllDeviations));
    let scope_callback = Arc::new(PlanScopeCallback {
        scope: scope_state.clone(),
        inner: user_cb,
        dev_callback: dev_cb,
    });
    options.permission_callback = Some(scope_callback);

    // Augment system prompt with the act-mode contract.
    let original_system = options.system_prompt.clone();
    options.system_prompt = Some(act_mode_system_prompt(&plan, &original_system));

    let agent = AgentBuilder::new()
        .options(options.clone())
        .provider(req.provider.clone())
        .tools(tools)
        .build()
        .map_err(|e: AgentError| PlanError::Other(e.to_string()))?;

    let prompt = req.prompt.clone().unwrap_or_else(|| {
        format!(
            "Execute the approved plan {} v{} step by step. Use the PlanUpdate tool to mark each \
             step as in_progress and then completed. If you cannot proceed, mark the step blocked \
             or needs_replan. Do not invoke tools that are not listed in the plan unless they are \
             read-only inspections.",
            plan.id, plan.version
        )
    });

    let stream = agent.run(prompt);
    let mut collected: Vec<AgentEvent> = Vec::new();
    let mut total_usage: Option<TokenUsage> = None;
    futures::pin_mut!(stream);
    while let Some(event) = stream.next().await {
        if let AgentEvent::Result(r) = &event {
            if let Some(u) = &r.usage {
                total_usage = Some(u.clone());
            }
        }
        collected.push(event);
    }

    // Drain plan updates and apply to the plan document.
    let updates = plan_update_handle.snapshot().await;
    for u in &updates {
        for s in &mut plan.steps {
            if s.id == u.step_id {
                s.status = u.status;
            }
        }
    }
    let scope_snap = scope_state.lock().await;
    let deviations = scope_snap.deviations.clone();
    drop(scope_snap);

    let completed_steps: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStepStatus::Completed))
        .map(|s| s.id.clone())
        .collect();
    let failed_steps: Vec<String> = plan
        .steps
        .iter()
        .filter(|s| matches!(s.status, PlanStepStatus::Failed))
        .map(|s| s.id.clone())
        .collect();

    let exec_status = if !failed_steps.is_empty() {
        PlanExecutionStatus::Failed
    } else if plan
        .steps
        .iter()
        .any(|s| matches!(s.status, PlanStepStatus::NeedsReplan))
    {
        PlanExecutionStatus::NeedsReplan
    } else if plan.steps.iter().all(|s| {
        matches!(
            s.status,
            PlanStepStatus::Completed | PlanStepStatus::Skipped
        )
    }) {
        PlanExecutionStatus::Completed
    } else {
        PlanExecutionStatus::Failed
    };
    let plan_status = match exec_status {
        PlanExecutionStatus::Completed => PlanStatus::Completed,
        PlanExecutionStatus::Failed => PlanStatus::Failed,
        PlanExecutionStatus::NeedsReplan => PlanStatus::NeedsReplan,
        PlanExecutionStatus::Cancelled => PlanStatus::Failed,
        PlanExecutionStatus::Running => PlanStatus::Executing,
    };

    let summary = PlanExecutionSummary {
        started_at: plan
            .execution
            .as_ref()
            .map(|e| e.started_at)
            .unwrap_or_else(OffsetDateTime::now_utc),
        completed_at: Some(OffsetDateTime::now_utc()),
        status: exec_status,
        completed_steps,
        failed_steps,
        deviations: deviations.clone(),
        validations: Vec::new(),
        checkpoints: Vec::new(),
        usage: total_usage,
    };
    plan.execution = Some(summary.clone());
    plan.status = plan_status;
    plan.updated_at = OffsetDateTime::now_utc();
    req.plan_store.save_revision(plan.clone()).await?;

    Ok(ActOutcome {
        plan,
        events: collected,
        deviations,
        summary,
    })
}

pub fn act_mode_system_prompt(plan: &PlanDocument, original: &Option<String>) -> String {
    let extra = format!(
        "ACT MODE: you are executing the user-approved plan below. Follow the steps in order. \
         Use the `{}` tool between tool calls to record progress. Do not call any tool that is not \
         listed in the approved plan unless it is a read-only inspection (Read/Glob/Grep). If you \
         need to deviate, mark the current step as needs_replan and stop.\n\n--- APPROVED PLAN ---\n{}",
        PLAN_UPDATE_TOOL_NAME,
        render_plan_pretty(plan)
    );
    match original {
        Some(s) => format!("{s}\n\n{extra}"),
        None => extra,
    }
}

/// Internal: PermissionCallback wrapper that classifies tool calls against
/// the approved plan before delegating to the inner callback. Records all
/// deviations in the scope state.
struct ScopeState {
    plan: PlanDocument,
    options: PlanningOptions,
    deviations: Vec<PlanDeviation>,
}

struct PlanScopeCallback {
    scope: Arc<Mutex<ScopeState>>,
    inner: Arc<dyn PermissionCallback>,
    dev_callback: Arc<dyn PlanDeviationCallback>,
}

#[async_trait]
impl PermissionCallback for PlanScopeCallback {
    async fn approve(&self, request: PermissionRequest) -> PermissionDecision {
        let snap = self.scope.lock().await;
        let plan = snap.plan.clone();
        let options = snap.options.clone();
        drop(snap);

        if !options.enforce_plan_scope {
            return self.inner.approve(request).await;
        }
        let decision = classify_tool_call(&plan, &options, &request.tool_name, &request.input);
        match decision {
            PlanScopeDecision::InScope => self.inner.approve(request).await,
            PlanScopeDecision::Ambiguous { .. } | PlanScopeDecision::NotApplicable => {
                self.inner.approve(request).await
            }
            PlanScopeDecision::OutOfScope { reason } => {
                let dev = PlanDeviation {
                    plan_id: plan.id.clone(),
                    step_id: plan
                        .steps
                        .iter()
                        .find(|s| matches!(s.status, PlanStepStatus::Pending))
                        .map(|s| s.id.clone()),
                    tool_name: request.tool_name.clone(),
                    requested_input: request.input.clone(),
                    reason: reason.clone(),
                    severity: default_severity_for(&request.tool_name),
                    occurred_at: OffsetDateTime::now_utc(),
                };
                {
                    let mut s = self.scope.lock().await;
                    s.deviations.push(dev.clone());
                }
                match options.deviation_policy {
                    PlanDeviationPolicy::Deny => PermissionDecision::Deny {
                        reason: format!("plan deviation denied by policy: {reason}"),
                    },
                    PlanDeviationPolicy::AllowWithWarning => {
                        // Still defer to the underlying callback so the user
                        // can refuse on top of the warning.
                        self.inner.approve(request).await
                    }
                    PlanDeviationPolicy::RequireReplan => PermissionDecision::Deny {
                        reason: format!("plan deviation requires re-planning: {reason}"),
                    },
                    PlanDeviationPolicy::Ask => match self.dev_callback.resolve(dev).await {
                        crate::plan::deviation::PlanDeviationDecision::ApproveOnce => {
                            self.inner.approve(request).await
                        }
                        crate::plan::deviation::PlanDeviationDecision::UpdatePlan => {
                            self.inner.approve(request).await
                        }
                        crate::plan::deviation::PlanDeviationDecision::Replan => {
                            PermissionDecision::Deny {
                                reason: "user asked to re-plan after deviation".into(),
                            }
                        }
                        crate::plan::deviation::PlanDeviationDecision::Deny => {
                            PermissionDecision::Deny {
                                reason: format!("user denied plan deviation: {reason}"),
                            }
                        }
                    },
                }
            }
        }
    }
}

/// Convenience: extract the latest PolicyDecision events from a planning
/// run, useful for tests.
pub fn collect_policy_decisions(
    events: &[AgentEvent],
) -> Vec<(String, PolicyOutcome, PolicyDecisionSource)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::PolicyDecision(d) => Some((d.tool_name.clone(), d.outcome, d.source)),
            _ => None,
        })
        .collect()
}
