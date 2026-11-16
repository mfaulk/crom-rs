#![cfg(feature = "fake-provider")]

use std::sync::Arc;

use crom_core::{ModelEvent, ModelPricing, TokenUsage, ToolCall};
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::{
    AgentMode, AgentRequest, AgentRunResult, AgentRunStatus, AgentRuntime, AllowAllApprovals,
    BuiltinToolSet, InteractivityMode, PatchPolicy, ProviderConfig, RunBudgets, SideEffectPolicy,
    ToolPolicy, WorkspaceSource,
};
use tempfile::TempDir;

fn usage(input: u64, output: u64) -> Option<TokenUsage> {
    Some(TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        ..Default::default()
    })
}

/// `n` Write turns then a final text turn, each reporting `u`.
fn script(n: usize, u: Option<TokenUsage>) -> Vec<Vec<ModelEvent>> {
    let mut turns: Vec<Vec<ModelEvent>> = (0..n)
        .map(|i| {
            vec![
                ModelEvent::ToolCall(ToolCall {
                    id: format!("w{i}"),
                    name: "Write".into(),
                    input: serde_json::json!({"file_path": format!("f{i}.txt"), "content": "x\n"}),
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: u.clone(),
                },
            ]
        })
        .collect();
    turns.push(vec![
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: u,
        },
    ]);
    turns
}

async fn run(
    script: Vec<Vec<ModelEvent>>,
    budgets: RunBudgets,
    pricing: Option<ModelPricing>,
    mode: AgentMode,
) -> Result<AgentRunResult, crom_sdk::AgentSdkError> {
    let dir = TempDir::new().unwrap();
    let mut builder = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(ScriptedFakeProvider::new(
            script,
        ))))
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::cli_defaults(),
            side_effect_policy: SideEffectPolicy::full(),
            ..Default::default()
        })
        .interactivity(InteractivityMode::Headless)
        .approval_callback(Arc::new(AllowAllApprovals));
    if let Some(p) = pricing {
        builder = builder.pricing(p);
    }
    let runtime = builder.build().await.unwrap();
    runtime
        .run_once(
            AgentRequest::new("write files")
                .mode(mode)
                .patch_policy(PatchPolicy::ApplyToWorkspace)
                .interactivity(InteractivityMode::Headless)
                .max_turns(20)
                .budgets(budgets),
        )
        .await
}

fn codes(r: &AgentRunResult) -> Vec<&str> {
    r.diagnostics.iter().map(|d| d.code.as_str()).collect()
}

// An output-token budget fails the run with a distinct diagnostic after the
// turn that crosses it, and later tool calls never start.
#[tokio::test]
async fn output_token_budget_fails_run() {
    let r = run(
        script(5, usage(10, 40)),
        RunBudgets::default().with_max_output_tokens(100),
        None,
        AgentMode::ModifyWorkspace,
    )
    .await
    .unwrap();
    assert_eq!(r.status, AgentRunStatus::Failed);
    assert!(codes(&r).contains(&"budget.max_output_tokens_exceeded"));
    assert_eq!(r.events_summary.tool_calls, 2);
    assert_eq!(r.usage.unwrap().output_tokens, Some(120));
}

// An input-token budget maps to its own diagnostic code.
#[tokio::test]
async fn input_token_budget_fails_run() {
    let r = run(
        script(5, usage(60, 1)),
        RunBudgets::default().with_max_input_tokens(100),
        None,
        AgentMode::ModifyWorkspace,
    )
    .await
    .unwrap();
    assert_eq!(r.status, AgentRunStatus::Failed);
    assert!(codes(&r).contains(&"budget.max_input_tokens_exceeded"));
}

// A cost budget uses the runtime's pricing to stop the run.
#[tokio::test]
async fn cost_budget_fails_run() {
    let pricing = ModelPricing {
        input_per_million: 1_000.0,
        output_per_million: 0.0,
        cache_creation_per_million: None,
        cache_read_per_million: None,
    };
    // 1000 input tokens/turn at $1000/M = $1/turn.
    let r = run(
        script(5, usage(1_000, 1)),
        RunBudgets::default().with_max_cost_usd(2.5),
        Some(pricing),
        AgentMode::ModifyWorkspace,
    )
    .await
    .unwrap();
    assert_eq!(r.status, AgentRunStatus::Failed);
    assert!(codes(&r).contains(&"budget.max_cost_exceeded"));
    assert_eq!(r.events_summary.tool_calls, 2);
}

// A cost budget without pricing fails closed before any model call.
#[tokio::test]
async fn cost_budget_without_pricing_fails_closed() {
    let r = run(
        script(2, usage(1, 1)),
        RunBudgets::default().with_max_cost_usd(1.0),
        None,
        AgentMode::ModifyWorkspace,
    )
    .await
    .unwrap();
    assert_eq!(r.status, AgentRunStatus::Failed);
    assert!(codes(&r).contains(&"budget.usage_unknown"));
    assert_eq!(r.events_summary.tool_calls, 0);
}

// Plan modes do not meter usage budgets yet, so a request that sets one is
// rejected rather than silently running unmetered.
#[tokio::test]
async fn plan_then_act_rejects_usage_budgets() {
    let r = run(
        script(1, usage(1, 1)),
        RunBudgets::default().with_max_output_tokens(10),
        None,
        AgentMode::PlanThenAct,
    )
    .await
    .unwrap();
    assert_eq!(r.status, AgentRunStatus::Failed);
    assert!(codes(&r).contains(&"budget.unsupported_mode"));
    assert_eq!(r.events_summary.total_events, 0);
}
