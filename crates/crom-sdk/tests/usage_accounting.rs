#![cfg(feature = "fake-provider")]

use std::sync::Arc;

use crom_core::{ModelEvent, TokenUsage, ToolCall};
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::{
    AgentMode, AgentRequest, AgentRunResult, AgentRunStatus, AgentRuntime, AllowAllApprovals,
    ApprovalPolicy, BuiltinToolSet, InteractivityMode, PatchPolicy, ProviderConfig, SdkEvent,
    SideEffectPolicy, ToolPolicy, WorkspaceSource,
};
use futures::StreamExt;
use tempfile::TempDir;

fn usage(input: u64, output: u64) -> TokenUsage {
    TokenUsage {
        input_tokens: Some(input),
        output_tokens: Some(output),
        cache_creation_input_tokens: Some(0),
        cache_read_input_tokens: Some(input / 2),
        reasoning_tokens: None,
    }
}

fn tool_turn(
    id: &str,
    name: &str,
    input: serde_json::Value,
    u: Option<TokenUsage>,
) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".into()),
            usage: u,
        },
    ]
}

fn text_turn(text: &str, u: Option<TokenUsage>) -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta { text: text.into() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: u,
        },
    ]
}

async fn run(
    script: Vec<Vec<ModelEvent>>,
    mode: AgentMode,
) -> (Vec<SdkEvent>, AgentRunResult, TempDir) {
    let dir = TempDir::new().unwrap();
    let runtime = AgentRuntime::builder()
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
        .approval_callback(Arc::new(AllowAllApprovals))
        .build()
        .await
        .unwrap();
    let mut handle = runtime
        .run(
            AgentRequest::new("do the task")
                .mode(mode)
                .patch_policy(PatchPolicy::ApplyToWorkspace)
                .approval_policy(ApprovalPolicy::CallbackRequired)
                .interactivity(InteractivityMode::Headless)
                .max_turns(6),
        )
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(ev) = handle.events.next().await {
        events.push(ev.unwrap());
    }
    let result = handle.final_result().await.unwrap();
    (events, result, dir)
}

fn model_turns(events: &[SdkEvent]) -> Vec<&crom_sdk::event::ModelTurnCompletedEvent> {
    events
        .iter()
        .filter_map(|e| match e {
            SdkEvent::ModelTurnCompleted(t) => Some(t),
            _ => None,
        })
        .collect()
}

/// A turn whose provider response carries no usage makes the run usage None (not zero) in the result and RunCompleted.
#[tokio::test]
async fn unreported_turn_usage_is_none_end_to_end() {
    let script = vec![
        tool_turn(
            "g1",
            "Glob",
            serde_json::json!({ "pattern": "*" }),
            Some(usage(10, 2)),
        ),
        text_turn("done", None),
    ];
    let (events, result, _dir) = run(script, AgentMode::ModifyWorkspace).await;
    assert!(matches!(result.status, AgentRunStatus::Completed));

    let turns = model_turns(&events);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].usage, Some(usage(10, 2)));
    assert_eq!(turns[1].usage, None);

    assert_eq!(result.usage, None);
    let completed = events
        .iter()
        .find_map(|e| match e {
            SdkEvent::RunCompleted(c) => Some(c),
            _ => None,
        })
        .expect("RunCompleted");
    assert_eq!(completed.usage, None);
    let json = serde_json::to_value(completed).unwrap();
    assert!(
        json["usage"].is_null(),
        "unknown usage must serialize as null: {json}"
    );
}

/// Fully reported turns are summed into the run usage and each emits a ModelTurnCompleted with a stable wire shape.
#[tokio::test]
async fn reported_usage_sums_and_event_snapshot() {
    let script = vec![
        tool_turn(
            "g1",
            "Glob",
            serde_json::json!({ "pattern": "*" }),
            Some(usage(10, 2)),
        ),
        text_turn("done", Some(usage(20, 3))),
    ];
    let (events, result, _dir) = run(script, AgentMode::ModifyWorkspace).await;
    assert_eq!(result.usage, Some(usage(30, 5)));

    let turns = model_turns(&events);
    assert_eq!(turns.len(), 2);
    let mut json = serde_json::to_value(SdkEvent::ModelTurnCompleted(turns[1].clone())).unwrap();
    json["run_id"] = serde_json::Value::Null;
    json["latency_ms"] = serde_json::Value::Null;
    assert_eq!(
        json,
        serde_json::json!({
            "type": "model_turn_completed",
            "run_id": null,
            "turn": 2,
            "provider": "fake",
            "model": result.model,
            "usage": {
                "input_tokens": 20,
                "output_tokens": 3,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 10,
                "reasoning_tokens": null
            },
            "latency_ms": null
        })
    );
}

/// Plan-then-act surfaces planning-phase round trips and the run usage covers both phases.
#[tokio::test]
async fn plan_then_act_counts_planning_turns() {
    let submit_plan_input = serde_json::json!({
        "title": "Write greeting",
        "summary": "Create out.txt with a greeting",
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
    let script = vec![
        tool_turn("plan-1", "SubmitPlan", submit_plan_input, Some(usage(1, 1))),
        text_turn("plan ready", Some(usage(1, 1))),
        tool_turn(
            "act-1",
            "Write",
            serde_json::json!({ "file_path": "out.txt", "content": "hello\n" }),
            Some(usage(1, 1)),
        ),
        text_turn("done", Some(usage(1, 1))),
    ];
    let (events, result, _dir) = run(script, AgentMode::PlanThenAct).await;
    assert!(
        matches!(result.status, AgentRunStatus::Completed),
        "{:?}",
        result.status
    );
    assert_eq!(model_turns(&events).len(), 4);
    let mut expected = usage(4, 4);
    expected.cache_read_input_tokens = Some(0);
    assert_eq!(result.usage, Some(expected));
}
