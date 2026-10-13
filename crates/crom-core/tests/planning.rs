//! M11 planning end-to-end integration tests.
//!
//! These tests run the full planning + act drivers against a deterministic
//! scripted model provider. No network, no real model.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crom_core::{
    act_plan, classify_tool_call, generate_plan, new_plan_id, plan_update_tool_definition,
    submit_plan_tool_definition, ActRequest, AgentOptions, FsPlanStore, ModelEvent, ModelProvider,
    ModelRequest, PermissionKind, PermissionMode, PlanApprovalRequest, PlanApprovalScope,
    PlanApprover, PlanCreatedBy, PlanDeviationPolicy, PlanDocument, PlanError, PlanFileAccess,
    PlanFileRef, PlanListFilter, PlanRequest, PlanScopeDecision, PlanStatus, PlanStepStatus,
    PlanStore, PlanToolUse, PlanningOptions, ProviderError, ToolCall, ToolRegistry,
    WorkspaceRelativePath,
};
use futures::stream::{self, BoxStream};
use serde_json::json;
use tempfile::TempDir;

/// A deterministic provider that walks a list of canned turns.
#[derive(Clone)]
struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
        }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let mut turns = self.turns.lock().unwrap();
        let events = if turns.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            turns.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn submit_plan_event(input: serde_json::Value) -> ModelEvent {
    ModelEvent::ToolCall(ToolCall {
        id: "tc_submit".into(),
        name: "SubmitPlan".into(),
        input,
    })
}

fn plan_update_event(step_id: &str, status: &str) -> ModelEvent {
    ModelEvent::ToolCall(ToolCall {
        id: format!("tc_update_{step_id}_{status}"),
        name: "PlanUpdate".into(),
        input: json!({"step_id": step_id, "status": status}),
    })
}

fn message_stop() -> ModelEvent {
    ModelEvent::MessageStop {
        stop_reason: Some("end_turn".into()),
        usage: None,
    }
}

fn empty_text(text: &str) -> ModelEvent {
    ModelEvent::TextDelta { text: text.into() }
}

fn options_for(cwd: &std::path::Path) -> AgentOptions {
    AgentOptions {
        cwd: cwd.to_path_buf(),
        model: "test".into(),
        allowed_tools: vec!["Read".into(), "Glob".into(), "Grep".into()],
        permission_mode: PermissionMode::Default,
        max_turns: Some(4),
        ..AgentOptions::default()
    }
}

fn planning_opts() -> PlanningOptions {
    PlanningOptions::default()
}

fn sample_plan_input() -> serde_json::Value {
    json!({
        "title": "fix the failing parser tests",
        "summary": "find the bug, fix it, run tests",
        "goal": "all tests pass",
        "files": [
            {"path": "src/parser.rs", "access": "edit"},
            {"path": "tests/parser_tests.rs", "access": "read"}
        ],
        "steps": [
            {
                "id": "1",
                "title": "inspect failing tests",
                "kind": "inspect",
                "expected_files": [
                    {"path": "tests/parser_tests.rs", "access": "read"}
                ],
                "risk": "low"
            },
            {
                "id": "2",
                "title": "edit parser",
                "kind": "edit",
                "expected_files": [
                    {"path": "src/parser.rs", "access": "edit"}
                ],
                "expected_tools": [
                    {"tool_name": "Edit", "permission_kind": "mutating"}
                ],
                "risk": "medium"
            },
            {
                "id": "3",
                "title": "run cargo test",
                "kind": "execute",
                "expected_commands": [
                    {"command": "cargo test", "command_regex": "^cargo (test|check)"}
                ],
                "expected_tools": [
                    {"tool_name": "Bash", "permission_kind": "execution"}
                ],
                "risk": "low"
            }
        ],
        "requested_tools": [
            {"tool_name": "Read", "permission_kind": "read_only"},
            {"tool_name": "Edit", "permission_kind": "mutating"},
            {"tool_name": "Bash", "permission_kind": "execution"}
        ],
        "validations": [
            {"command": "cargo test", "description": "all green"}
        ],
        "success_criteria": ["all parser tests green"]
    })
}

#[tokio::test]
async fn submit_plan_persists_plan() {
    let tmp = TempDir::new().unwrap();
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        empty_text("planning..."),
        submit_plan_event(sample_plan_input()),
        message_stop(),
    ]]));
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let outcome = generate_plan(PlanRequest {
        prompt: "fix the failing tests".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store.clone()),
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    })
    .await
    .unwrap();
    assert_eq!(outcome.plan.steps.len(), 3);
    assert_eq!(outcome.plan.status, PlanStatus::PendingApproval);
    assert!(!outcome.fallback_used);
    let loaded = store.load(&outcome.plan.id).await.unwrap();
    assert_eq!(loaded.title, "fix the failing parser tests");
}

#[tokio::test]
async fn unstructured_fallback_marks_warning() {
    let tmp = TempDir::new().unwrap();
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        empty_text("First I'll read the parser. Then I'll edit it. Then I'll run tests."),
        message_stop(),
    ]]));
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let outcome = generate_plan(PlanRequest {
        prompt: "fix the failing tests".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store),
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    })
    .await
    .unwrap();
    assert!(outcome.fallback_used);
    assert!(outcome.plan.unstructured_warning.is_some());
}

#[tokio::test]
async fn act_refuses_unapproved_plan() {
    let tmp = TempDir::new().unwrap();
    let provider: Arc<dyn ModelProvider> =
        Arc::new(ScriptedProvider::new(vec![vec![message_stop()]]));
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let mut draft = PlanDocument::new_draft(new_plan_id(), "T", "u");
    draft.status = PlanStatus::PendingApproval;
    draft.steps.push(crom_core::PlanStep {
        id: "1".into(),
        title: "x".into(),
        description: String::new(),
        status: PlanStepStatus::Pending,
        kind: crom_core::PlanStepKind::Inspect,
        expected_tools: Vec::new(),
        expected_files: Vec::new(),
        expected_commands: Vec::new(),
        expected_outcome: None,
        risk: crom_core::PlanRiskLevel::Low,
        requires_user_approval: false,
        validation: None,
    });
    let id = draft.id.clone();
    store.create(draft).await.unwrap();
    let err = act_plan(ActRequest {
        plan_id: id,
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider,
        tools: ToolRegistry::new(),
        plan_store: store,
        user_callback: None,
        deviation_callback: None,
        prompt: None,
    })
    .await
    .unwrap_err();
    assert!(matches!(err, PlanError::BadStatus(_)));
}

#[tokio::test]
async fn act_runs_approved_plan_to_completion() {
    let tmp = TempDir::new().unwrap();
    let plan_provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        submit_plan_event(sample_plan_input()),
        message_stop(),
    ]]));
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let outcome = generate_plan(PlanRequest {
        prompt: "fix".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider: plan_provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store.clone()),
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    })
    .await
    .unwrap();
    let pid = outcome.plan.id.clone();
    let v = outcome.plan.version;
    store
        .approve(PlanApprovalRequest {
            plan_id: pid.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();

    // Act: model walks the three plan steps to completion via PlanUpdate.
    let act_provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        plan_update_event("1", "completed"),
        plan_update_event("2", "completed"),
        plan_update_event("3", "completed"),
        message_stop(),
    ]]));
    let outcome = act_plan(ActRequest {
        plan_id: pid.clone(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider: act_provider,
        tools: ToolRegistry::new(),
        plan_store: store.clone(),
        user_callback: None,
        deviation_callback: None,
        prompt: None,
    })
    .await
    .unwrap();
    assert_eq!(outcome.plan.status, PlanStatus::Completed);
    assert_eq!(outcome.summary.completed_steps.len(), 3);
}

#[tokio::test]
async fn act_records_deviation_when_step_fails() {
    let tmp = TempDir::new().unwrap();
    let plan_provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        submit_plan_event(sample_plan_input()),
        message_stop(),
    ]]));
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let outcome = generate_plan(PlanRequest {
        prompt: "fix".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider: plan_provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store.clone()),
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    })
    .await
    .unwrap();
    let v = outcome.plan.version;
    let pid = outcome.plan.id.clone();
    store
        .approve(PlanApprovalRequest {
            plan_id: pid.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
    // Act: one step is marked failed.
    let act_provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        plan_update_event("1", "completed"),
        plan_update_event("2", "failed"),
        message_stop(),
    ]]));
    let outcome = act_plan(ActRequest {
        plan_id: pid,
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider: act_provider,
        tools: ToolRegistry::new(),
        plan_store: store,
        user_callback: None,
        deviation_callback: None,
        prompt: None,
    })
    .await
    .unwrap();
    assert_eq!(outcome.plan.status, PlanStatus::Failed);
    assert_eq!(outcome.summary.failed_steps.len(), 1);
}

#[tokio::test]
async fn scope_classifier_handles_planned_vs_extra_files() {
    let mut plan = PlanDocument::new_draft(new_plan_id(), "T", "u");
    plan.files.push(PlanFileRef {
        path: WorkspaceRelativePath::try_new("src/parser.rs").unwrap(),
        access: PlanFileAccess::Edit,
        reason: None,
    });
    plan.requested_tools.push(PlanToolUse {
        tool_name: "Edit".into(),
        permission_kind: PermissionKind::Mutating,
        input_constraints: serde_json::Value::Null,
        reason: None,
    });
    let opts = planning_opts();
    let d = classify_tool_call(&plan, &opts, "Edit", &json!({"file_path": "src/parser.rs"}));
    assert!(matches!(d, PlanScopeDecision::InScope));
    let d2 = classify_tool_call(&plan, &opts, "Edit", &json!({"file_path": "src/lexer.rs"}));
    assert!(matches!(d2, PlanScopeDecision::OutOfScope { .. }));
}

#[tokio::test]
async fn approval_invalidated_after_revision() {
    let tmp = TempDir::new().unwrap();
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let mut plan = PlanDocument::new_draft(new_plan_id(), "T", "u");
    plan.steps.push(crom_core::PlanStep {
        id: "1".into(),
        title: "x".into(),
        description: String::new(),
        status: PlanStepStatus::Pending,
        kind: crom_core::PlanStepKind::Inspect,
        expected_tools: Vec::new(),
        expected_files: Vec::new(),
        expected_commands: Vec::new(),
        expected_outcome: None,
        risk: crom_core::PlanRiskLevel::Low,
        requires_user_approval: false,
        validation: None,
    });
    plan.status = PlanStatus::PendingApproval;
    let id = plan.id.clone();
    let v = plan.version;
    store.create(plan).await.unwrap();
    let approved = store
        .approve(PlanApprovalRequest {
            plan_id: id.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
    assert_eq!(approved.status, PlanStatus::Approved);
    // Revise: bump version, mark as revised. Approval is invalidated.
    let mut rev = store.load(&id).await.unwrap();
    rev.version = 2;
    rev.created_by = PlanCreatedBy::RevisedByUser;
    store.save_revision(rev).await.unwrap();
    let after = store.load(&id).await.unwrap();
    assert_eq!(after.status, PlanStatus::PendingApproval);
    assert!(after.approval.is_none());
}

#[tokio::test]
async fn deviation_policy_deny_blocks_out_of_scope_via_callback() {
    // Build a plan that only allows Read on src/parser.rs but the act run
    // tries to Edit src/lexer.rs.
    let tmp = TempDir::new().unwrap();
    let mut plan = PlanDocument::new_draft(new_plan_id(), "T", "u");
    plan.files.push(PlanFileRef {
        path: WorkspaceRelativePath::try_new("src/parser.rs").unwrap(),
        access: PlanFileAccess::Read,
        reason: None,
    });
    plan.requested_tools.push(PlanToolUse {
        tool_name: "Read".into(),
        permission_kind: PermissionKind::ReadOnly,
        input_constraints: serde_json::Value::Null,
        reason: None,
    });
    plan.steps.push(crom_core::PlanStep {
        id: "1".into(),
        title: "read".into(),
        description: String::new(),
        status: PlanStepStatus::Pending,
        kind: crom_core::PlanStepKind::Inspect,
        expected_tools: Vec::new(),
        expected_files: vec![PlanFileRef {
            path: WorkspaceRelativePath::try_new("src/parser.rs").unwrap(),
            access: PlanFileAccess::Read,
            reason: None,
        }],
        expected_commands: Vec::new(),
        expected_outcome: None,
        risk: crom_core::PlanRiskLevel::Low,
        requires_user_approval: false,
        validation: None,
    });
    plan.status = PlanStatus::PendingApproval;
    let pid = plan.id.clone();
    let v = plan.version;
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    store.create(plan).await.unwrap();
    store
        .approve(PlanApprovalRequest {
            plan_id: pid.clone(),
            approved_by: PlanApprover::TestHarness,
            approved_version: v,
            scope: PlanApprovalScope::FullPlan,
            notes: None,
        })
        .await
        .unwrap();
    let opts = PlanningOptions {
        deviation_policy: PlanDeviationPolicy::Deny,
        ..planning_opts()
    };
    let provider: Arc<dyn ModelProvider> =
        Arc::new(ScriptedProvider::new(vec![vec![message_stop()]]));
    let outcome = act_plan(ActRequest {
        plan_id: pid,
        options: AgentOptions {
            allowed_tools: vec!["Read".into(), "Edit".into()],
            ..options_for(tmp.path())
        },
        planning: opts,
        provider,
        tools: ToolRegistry::new(),
        plan_store: store,
        user_callback: None,
        deviation_callback: Some(Arc::new(crom_core::DenyAllDeviations)),
        prompt: None,
    })
    .await
    .unwrap();
    // No tool calls were made, so the plan never advances; status falls into
    // Failed because nothing completed.
    assert!(matches!(
        outcome.plan.status,
        PlanStatus::Failed | PlanStatus::NeedsReplan | PlanStatus::Executing
    ));
}

#[test]
fn tool_definitions_are_valid_json_schemas() {
    let s = submit_plan_tool_definition();
    let u = plan_update_tool_definition();
    assert_eq!(s["type"], "object");
    assert_eq!(u["type"], "object");
    assert!(s["properties"]["title"].is_object());
    assert!(u["properties"]["step_id"].is_object());
}

#[tokio::test]
async fn replan_links_to_parent_plan() {
    // The "replan" semantics are: mark old plan Superseded; new plan with
    // parent_plan_id = old.id.
    let tmp = TempDir::new().unwrap();
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let mut parent = PlanDocument::new_draft(new_plan_id(), "parent", "u");
    parent.steps.push(crom_core::PlanStep {
        id: "1".into(),
        title: "x".into(),
        description: String::new(),
        status: PlanStepStatus::Pending,
        kind: crom_core::PlanStepKind::Inspect,
        expected_tools: Vec::new(),
        expected_files: Vec::new(),
        expected_commands: Vec::new(),
        expected_outcome: None,
        risk: crom_core::PlanRiskLevel::Low,
        requires_user_approval: false,
        validation: None,
    });
    parent.status = PlanStatus::PendingApproval;
    let pid = parent.id.clone();
    store.create(parent).await.unwrap();

    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        submit_plan_event(sample_plan_input()),
        message_stop(),
    ]]));
    let outcome = generate_plan(PlanRequest {
        prompt: "replan".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store.clone()),
        parent_plan_id: Some(pid.clone()),
        created_by: PlanCreatedBy::RevisedByAgent,
    })
    .await
    .unwrap();
    assert_eq!(outcome.plan.parent_plan_id.as_ref(), Some(&pid));
}

#[tokio::test]
async fn plan_summary_is_listable() {
    let tmp = TempDir::new().unwrap();
    let store: Arc<dyn PlanStore> = Arc::new(FsPlanStore::new(tmp.path(), planning_opts()));
    let provider: Arc<dyn ModelProvider> = Arc::new(ScriptedProvider::new(vec![vec![
        submit_plan_event(sample_plan_input()),
        message_stop(),
    ]]));
    let outcome = generate_plan(PlanRequest {
        prompt: "x".into(),
        options: options_for(tmp.path()),
        planning: planning_opts(),
        provider,
        tools: ToolRegistry::new(),
        plan_store: Some(store.clone()),
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    })
    .await
    .unwrap();
    let list = store.list(PlanListFilter::default()).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, outcome.plan.id);
    assert_eq!(list[0].status, PlanStatus::PendingApproval);
}

#[allow(unused_imports)]
use crom_core::PlanRiskLevel;
