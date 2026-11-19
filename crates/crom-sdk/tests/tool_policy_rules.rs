//! Host `ToolPolicy.allowed_tools` / `disallowed_tools` rules must reach the
//! core policy engine (#341, MILESTONE_18 D1).

#![cfg(feature = "fake-provider")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use crom_core::events::{SecurityDecision, SecurityOutcome};
use crom_core::policy::PolicyDecisionSource;
use crom_core::{ModelEvent, TokenUsage, ToolCall, ToolError};
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::policy::ToolRule;
use crom_sdk::AgentSdkErrorKind;
use crom_sdk::{
    AgentMode, AgentRequest, AgentRuntime, AllowAllApprovals, ApprovalPolicy, BuiltinToolSet,
    InteractivityMode, PatchPolicy, PermissionKind, ProviderConfig, SdkEvent, SideEffectPolicy,
    Tool, ToolContext, ToolPolicy, ToolResult, WorkspaceSource,
};
use futures::StreamExt;
use tempfile::TempDir;

struct FlagTool {
    ran: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for FlagTool {
    fn name(&self) -> &str {
        "HostEffect"
    }
    fn description(&self) -> &str {
        "Records that it ran"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::External
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.ran.store(true, Ordering::SeqCst);
        Ok(ToolResult::ok_text("ran"))
    }
}

fn full_policy() -> ToolPolicy {
    ToolPolicy {
        enable_builtin_tools: BuiltinToolSet::cli_defaults(),
        side_effect_policy: SideEffectPolicy::full(),
        ..Default::default()
    }
}

/// Runs one scripted tool call with `AllowAllApprovals` and returns the
/// security decisions emitted for it.
async fn run_tool_call(
    policy: ToolPolicy,
    interactivity: InteractivityMode,
    dir: &TempDir,
    tool: &str,
    input: serde_json::Value,
) -> Vec<SecurityDecision> {
    let provider = ScriptedFakeProvider::one_shot_tool_call(tool, input);
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .approval_callback(Arc::new(AllowAllApprovals))
        .interactivity(interactivity)
        .tool_policy(policy)
        .build()
        .await
        .unwrap();
    let request = AgentRequest::new("do it")
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .approval_policy(ApprovalPolicy::CallbackRequired)
        .mode(AgentMode::ModifyWorkspace)
        .patch_policy(PatchPolicy::ApplyToWorkspace)
        .max_turns(3);
    collect_decisions(&runtime, request).await
}

async fn collect_decisions(runtime: &AgentRuntime, request: AgentRequest) -> Vec<SecurityDecision> {
    let mut handle = runtime.run(request).await.unwrap();
    let mut decisions = Vec::new();
    while let Some(ev) = handle.events.next().await {
        if let SdkEvent::SecurityDecision(d) = ev.unwrap() {
            decisions.push(d);
        }
    }
    let _ = handle.final_result().await.unwrap();
    decisions
}

fn single(decisions: &[SecurityDecision], tool: &str) -> SecurityDecision {
    let matching: Vec<_> = decisions.iter().filter(|d| d.tool_name == tool).collect();
    assert_eq!(
        matching.len(),
        1,
        "expected one decision for {tool}: {decisions:?}"
    );
    matching[0].clone()
}

fn assert_disallowed(d: &SecurityDecision) {
    assert_eq!(
        d.policy_source,
        Some(PolicyDecisionSource::DisallowRule),
        "{d:?}"
    );
    assert_eq!(d.outcome, SecurityOutcome::Blocked, "{d:?}");
}

/// An `ExactTool("Write")` disallow rule blocks Write despite full side effects and an allow-all callback.
#[tokio::test]
async fn disallowed_exact_tool_blocks_write() {
    let dir = TempDir::new().unwrap();
    let policy = ToolPolicy {
        disallowed_tools: vec![ToolRule::ExactTool("Write".into())],
        ..full_policy()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::Interactive,
        &dir,
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    )
    .await;
    assert_disallowed(&single(&decisions, "Write"));
    assert!(!dir.path().join("x.txt").exists());
}

/// Without a disallow rule the same Write call executes (control for the test above).
#[tokio::test]
async fn write_executes_without_disallow_rule() {
    let dir = TempDir::new().unwrap();
    let decisions = run_tool_call(
        full_policy(),
        InteractivityMode::Interactive,
        &dir,
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    )
    .await;
    assert_eq!(
        single(&decisions, "Write").outcome,
        SecurityOutcome::Executed
    );
    assert!(dir.path().join("x.txt").exists());
}

/// A `BashCommandRegex` disallow rule blocks a matching Bash command.
#[tokio::test]
async fn disallowed_bash_regex_blocks_matching_command() {
    let dir = TempDir::new().unwrap();
    let policy = ToolPolicy {
        disallowed_tools: vec![ToolRule::try_bash_command_regex(r"^touch\b").unwrap()],
        ..full_policy()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::Interactive,
        &dir,
        "Bash",
        serde_json::json!({"command": "touch marker"}),
    )
    .await;
    assert_disallowed(&single(&decisions, "Bash"));
    assert!(!dir.path().join("marker").exists());
}

/// A disallow rule naming a host-registered custom tool prevents it from executing.
#[tokio::test]
async fn disallowed_custom_tool_never_executes() {
    let dir = TempDir::new().unwrap();
    let ran = Arc::new(AtomicBool::new(false));
    let policy = ToolPolicy {
        disallowed_tools: vec![ToolRule::ExactTool("HostEffect".into())],
        custom_tools: vec![Arc::new(FlagTool { ran: ran.clone() })],
        ..full_policy()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::Interactive,
        &dir,
        "HostEffect",
        serde_json::json!({}),
    )
    .await;
    assert_disallowed(&single(&decisions, "HostEffect"));
    assert!(!ran.load(Ordering::SeqCst));
}

/// An `allowed_tools` rule auto-allows a matching call even when the run would otherwise deny approvals.
#[tokio::test]
async fn allowed_rule_auto_allows_without_approval() {
    let dir = TempDir::new().unwrap();
    let policy = ToolPolicy {
        allowed_tools: vec![ToolRule::try_file_path_glob("Write", "*.txt").unwrap()],
        ..full_policy()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::HeadlessDeny,
        &dir,
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    )
    .await;
    let d = single(&decisions, "Write");
    assert_eq!(
        d.policy_source,
        Some(PolicyDecisionSource::AllowRule),
        "{d:?}"
    );
    assert_eq!(d.outcome, SecurityOutcome::Executed);
    assert!(dir.path().join("x.txt").exists());
}

/// A call that matches no `allowed_tools` rule still goes through the approval path.
#[tokio::test]
async fn non_matching_call_still_requires_approval() {
    let dir = TempDir::new().unwrap();
    let policy = ToolPolicy {
        allowed_tools: vec![ToolRule::try_file_path_glob("Write", "*.md").unwrap()],
        ..full_policy()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::HeadlessDeny,
        &dir,
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    )
    .await;
    assert_eq!(
        single(&decisions, "Write").outcome,
        SecurityOutcome::Blocked
    );
    assert!(!dir.path().join("x.txt").exists());
}

/// An `allowed_tools` rule cannot enable a tool that the side-effect policy left unregistered.
#[tokio::test]
async fn allowed_rule_does_not_widen_side_effect_policy() {
    let dir = TempDir::new().unwrap();
    let policy = ToolPolicy {
        allowed_tools: vec![ToolRule::ExactTool("Bash".into())],
        enable_builtin_tools: BuiltinToolSet::cli_defaults(),
        side_effect_policy: SideEffectPolicy::read_only(),
        ..Default::default()
    };
    let decisions = run_tool_call(
        policy,
        InteractivityMode::Interactive,
        &dir,
        "Bash",
        serde_json::json!({"command": "touch marker"}),
    )
    .await;
    assert_eq!(single(&decisions, "Bash").outcome, SecurityOutcome::Blocked);
    assert!(!dir.path().join("marker").exists());
}

/// Disallow rules also apply to tool calls made during the plan-then-act flow's act phase.
#[tokio::test]
async fn disallowed_rule_applies_in_act_phase() {
    let stop = |reason: &str| ModelEvent::MessageStop {
        stop_reason: Some(reason.into()),
        usage: Some(TokenUsage::default()),
    };
    let call = |id: &str, name: &str, input: serde_json::Value| {
        ModelEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        })
    };
    let text = |t: &str| ModelEvent::TextDelta { text: t.into() };
    let submit_plan = serde_json::json!({
        "title": "Write greeting",
        "summary": "Create out.txt",
        "goal": "create out.txt",
        "files": [{ "path": "out.txt", "access": "write" }],
        "requested_tools": [{ "tool_name": "Write" }],
        "steps": [{
            "id": "s1",
            "title": "write the file",
            "kind": "edit",
            "expected_tools": [{ "tool_name": "Write" }],
            "expected_files": [{ "path": "out.txt", "access": "write" }],
        }],
    });
    let provider = ScriptedFakeProvider::new(vec![
        vec![call("plan-1", "SubmitPlan", submit_plan), stop("tool_use")],
        vec![text("plan ready"), stop("end_turn")],
        vec![
            call(
                "act-1",
                "Write",
                serde_json::json!({"file_path": "out.txt", "content": "hello\n"}),
            ),
            stop("tool_use"),
        ],
        vec![text("done"), stop("end_turn")],
    ]);
    let dir = TempDir::new().unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .approval_callback(Arc::new(AllowAllApprovals))
        .interactivity(InteractivityMode::Headless)
        .tool_policy(ToolPolicy {
            disallowed_tools: vec![ToolRule::ExactTool("Write".into())],
            ..full_policy()
        })
        .build()
        .await
        .unwrap();
    // Mirrors `plan_then_act_emits_plan_and_act_tool_calls`, which writes
    // out.txt when no disallow rule is set.
    let request = AgentRequest::new("Create out.txt")
        .mode(AgentMode::PlanThenAct)
        .patch_policy(PatchPolicy::ApplyToWorkspace)
        .approval_policy(ApprovalPolicy::CallbackRequired)
        .interactivity(InteractivityMode::Headless)
        .max_turns(6);
    let _ = collect_decisions(&runtime, request).await;
    assert!(!dir.path().join("out.txt").exists());
}

/// A rule that cannot compile is rejected at `build()` instead of silently never matching.
#[tokio::test]
async fn malformed_rule_is_rejected_at_build() {
    for policy in [
        ToolPolicy {
            disallowed_tools: vec![ToolRule::BashCommandRegex("(".into())],
            ..Default::default()
        },
        ToolPolicy {
            allowed_tools: vec![ToolRule::ToolGlob("[".into())],
            ..Default::default()
        },
    ] {
        let Err(err) = AgentRuntime::builder()
            .provider(ProviderConfig::fake(Arc::new(
                ScriptedFakeProvider::text_response("x"),
            )))
            .tool_policy(policy)
            .build()
            .await
        else {
            panic!("malformed rule must fail build");
        };
        assert_eq!(err.kind, AgentSdkErrorKind::InvalidRequest, "{err:?}");
        assert_eq!(
            err.diagnostics[0].code, "policy.invalid_tool_rule",
            "{err:?}"
        );
    }
}
