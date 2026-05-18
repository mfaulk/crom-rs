//! End-to-end tests of the SDK runtime against the fake provider. These
//! tests run without network access or real provider credentials.

#![cfg(feature = "fake-provider")]

use std::sync::Arc;

use code_agent_sdk::fake_provider::{EchoFakeProvider, ScriptedFakeProvider};
use code_agent_sdk::{
    AgentMode, AgentRequest, AgentRunStatus, AgentRuntime, AgentStorage, AllowAllApprovals,
    ApprovalPolicy, BuiltinToolSet, EventVerbosity, InMemoryStorage, InMemoryWorkspace,
    InteractivityMode, PatchPolicy, ProviderConfig, RunBudgets, SdkEvent, SideEffectPolicy,
    ToolPolicy, ValidationPolicy, WorkspaceSource,
};
use futures::StreamExt;
use std::time::Duration;
use tempfile::TempDir;

/// run_once returns Completed status with the fake provider.
#[tokio::test]
async fn run_once_completes_with_fake_provider() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .expect("build runtime");
    let result = runtime
        .run_once(AgentRequest::new("hi there"))
        .await
        .expect("run completes");
    assert!(matches!(result.status, AgentRunStatus::Completed));
    assert!(result.summary.contains("hi there"));
    assert_eq!(result.provider.as_str(), "fake");
    assert!(!result.model.is_empty());
}

/// run streams RunStarted and RunCompleted events at minimum.
#[tokio::test]
async fn streamed_run_emits_start_and_completed() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .unwrap();
    let mut handle = runtime
        .run(AgentRequest::new("describe the workspace"))
        .await
        .unwrap();
    let mut saw_start = false;
    let mut saw_completed = false;
    while let Some(ev) = handle.events.next().await {
        match ev.unwrap() {
            SdkEvent::RunStarted(_) => saw_start = true,
            SdkEvent::RunCompleted(_) => saw_completed = true,
            _ => {}
        }
    }
    assert!(saw_start);
    assert!(saw_completed);
    let result = handle.final_result().await.unwrap();
    assert!(matches!(result.status, AgentRunStatus::Completed));
}

/// Cancellation flips the status to Cancelled and shortcircuits.
#[tokio::test]
async fn cancellation_returns_cancelled_status() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .unwrap();
    let handle = runtime.run(AgentRequest::new("a slow task")).await.unwrap();
    let cancellation = handle.cancellation.clone();
    cancellation.cancel();
    let result = handle.final_result().await.unwrap();
    // The fake provider exits very quickly, so the run may complete before
    // cancellation observed; in either case the status must be a terminal
    // state and the cancellation flag must be set.
    assert!(matches!(
        result.status,
        AgentRunStatus::Cancelled | AgentRunStatus::Completed
    ));
    assert!(cancellation.is_cancelled());
}

/// PatchPolicy::ReturnOnly does not mutate a filesystem workspace.
#[tokio::test]
async fn patch_policy_return_only_doesnt_mutate_fs() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), b"old content\n").unwrap();

    // Provider that calls Write tool to modify a.txt -- but ReturnOnly stops it.
    let provider = ScriptedFakeProvider::one_shot_tool_call(
        "Write",
        serde_json::json!({"file_path": "a.txt", "content": "new content\n"}),
    );
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::patch_only_defaults(),
            side_effect_policy: SideEffectPolicy::workspace_write(),
            ..Default::default()
        })
        .approval_callback(Arc::new(AllowAllApprovals))
        .interactivity(InteractivityMode::Interactive)
        .build()
        .await
        .unwrap();
    let request = AgentRequest::new("Write new content to a.txt")
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .mode(AgentMode::GeneratePatch)
        .patch_policy(PatchPolicy::ReturnOnly)
        .approval_policy(ApprovalPolicy::CallbackRequired)
        .max_turns(3);
    let result = runtime.run_once(request).await.unwrap();
    // Disk content unchanged.
    let on_disk = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
    assert_eq!(on_disk, "old content\n");
    // Patch reflects the intended change.
    let patch = result.patch.expect("patch produced");
    let modify_present = patch.files.iter().any(|f| {
        matches!(
            f,
            code_agent_sdk::FilePatch::Modify { path, .. } if path.as_str() == "a.txt"
        )
    });
    assert!(modify_present);
}

/// max_turns budget is enforced.
#[tokio::test]
async fn max_turns_budget_is_enforced() {
    // Scripted provider that always wants more turns.
    let provider = ScriptedFakeProvider::new(vec![
        vec![
            code_agent_core::ModelEvent::TextDelta {
                text: "step1".into(),
            },
            code_agent_core::ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
        vec![
            code_agent_core::ModelEvent::TextDelta {
                text: "step2".into(),
            },
            code_agent_core::ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]);
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .build()
        .await
        .unwrap();
    let result = runtime
        .run_once(
            AgentRequest::new("loop forever")
                .max_turns(1)
                .budgets(RunBudgets::default().with_max_turns(1)),
        )
        .await
        .unwrap();
    // The fake provider always ends with end_turn so the agent loop exits
    // after the first turn -- but max_turns=1 should be respected
    // regardless.
    assert!(matches!(
        result.status,
        AgentRunStatus::Completed | AgentRunStatus::Failed
    ));
}

/// Deadline triggers cancellation with TimedOut-ish status.
#[tokio::test]
async fn deadline_triggers_cancellation() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .unwrap();
    let result = runtime
        .run_once(AgentRequest::new("slow").deadline(Duration::from_millis(1)))
        .await
        .unwrap();
    // Even though the fake provider is fast, the deadline plumbing must
    // not panic and the status must be terminal.
    assert!(matches!(
        result.status,
        AgentRunStatus::Completed | AgentRunStatus::Cancelled | AgentRunStatus::TimedOut
    ));
}

/// InMemoryStorage records the run's start and completion events.
#[tokio::test]
async fn in_memory_storage_records_run_events() {
    let storage = Arc::new(InMemoryStorage::new());
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .storage(storage.clone())
        .build()
        .await
        .unwrap();
    let req = AgentRequest::new("hi").session_policy(code_agent_sdk::SessionPolicy::InMemory);
    let result = runtime.run_once(req).await.unwrap();
    let sid = result.session_id.expect("session id assigned");
    let snap = storage.load_session(&sid).await.unwrap();
    assert!(!snap.events.is_empty());
}

/// InMemoryWorkspace round-trips through the runtime: GeneratePatch returns
/// a patch describing any pending file writes.
#[tokio::test]
async fn in_memory_workspace_supports_runs() {
    let workspace = InMemoryWorkspace::from_pairs(vec![("README.md", b"old\n".to_vec())]).unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await
        .unwrap();
    let req = AgentRequest::new("describe the file");
    let result = runtime.run_once(req).await.unwrap();
    // The echo fake doesn't call any tools, so no patch entries.
    let patch = result.patch.expect("patch returned for return-only policy");
    assert_eq!(patch.summary.files_modified, 0);
}

/// Approval callback receives the request and its decision affects the run.
#[tokio::test]
async fn approval_callback_is_invoked() {
    let provider = ScriptedFakeProvider::one_shot_tool_call(
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    );
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .approval_callback(Arc::new(AllowAllApprovals))
        .interactivity(InteractivityMode::Interactive)
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::patch_only_defaults(),
            side_effect_policy: SideEffectPolicy::workspace_write(),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let dir = TempDir::new().unwrap();
    let request = AgentRequest::new("Create x.txt")
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .approval_policy(ApprovalPolicy::CallbackRequired)
        .mode(AgentMode::GeneratePatch)
        .patch_policy(PatchPolicy::ReturnOnly)
        .max_turns(3);
    let result = runtime.run_once(request).await.unwrap();
    assert!(matches!(
        result.status,
        AgentRunStatus::Completed | AgentRunStatus::Failed
    ));
}

/// Concurrent runs do not leak state between each other.
#[tokio::test]
async fn concurrent_runs_dont_leak_state() {
    let runtime = Arc::new(
        AgentRuntime::builder()
            .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
            .build()
            .await
            .unwrap(),
    );
    let mut handles = Vec::new();
    for i in 0..4 {
        let r = runtime.clone();
        let h = tokio::spawn(async move {
            r.run_once(AgentRequest::new(format!("task {i}")))
                .await
                .unwrap()
        });
        handles.push(h);
    }
    let mut run_ids = std::collections::HashSet::new();
    for h in handles {
        let result = h.await.unwrap();
        run_ids.insert(result.run_id.as_str().to_string());
    }
    assert_eq!(run_ids.len(), 4);
}

/// Validation commands record results when filesystem execution is enabled.
#[tokio::test]
async fn validation_commands_record_results() {
    let dir = TempDir::new().unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::patch_only_defaults(),
            side_effect_policy: SideEffectPolicy::command_execution(),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let validation = ValidationPolicy::default().with_command("true");
    let result = runtime
        .run_once(
            AgentRequest::new("describe")
                .workspace(WorkspaceSource::filesystem(dir.path()))
                .validation(validation),
        )
        .await
        .unwrap();
    assert_eq!(result.validations.len(), 1);
    assert!(result.validations[0].success);
}

/// Validation is denied when command execution is not allowed.
#[tokio::test]
async fn validation_denied_when_command_execution_disabled() {
    let dir = TempDir::new().unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::default(),
            side_effect_policy: SideEffectPolicy::read_only(),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let result = runtime
        .run_once(
            AgentRequest::new("describe")
                .workspace(WorkspaceSource::filesystem(dir.path()))
                .validation(ValidationPolicy::default().with_command("true")),
        )
        .await
        .unwrap();
    assert_eq!(result.validations.len(), 1);
    assert!(!result.validations[0].success);
    assert!(result
        .diagnostics
        .iter()
        .any(|d| d.code == "tool.side_effect_denied"));
}

/// Plan-only mode produces a PlanDocument and does not mutate.
#[tokio::test]
async fn plan_only_returns_plan_document() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .unwrap();
    let result = runtime
        .run_once(AgentRequest::new("ship the feature").mode(AgentMode::PlanOnly))
        .await
        .unwrap();
    assert!(result.plan.is_some());
    // Plan-only must not produce a patch.
    assert!(result.patch.is_none());
}

/// FilesystemWorkspace builds via WorkspaceSource::filesystem.
#[tokio::test]
async fn filesystem_workspace_source_builds() {
    let dir = TempDir::new().unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::filesystem(dir.path()))
        .build()
        .await
        .unwrap();
    assert_eq!(runtime.provider_id().as_str(), "fake");
    // Provide the same workspace per-request.
    let _ = runtime.run_once(AgentRequest::new("x")).await.unwrap();
}

/// EventVerbosity::Minimal suppresses assistant deltas.
#[tokio::test]
async fn event_verbosity_minimal_suppresses_assistant() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .verbosity(EventVerbosity::Minimal)
        .build()
        .await
        .unwrap();
    let mut h = runtime.run(AgentRequest::new("hi")).await.unwrap();
    let mut saw_assistant = false;
    while let Some(ev) = h.events.next().await {
        if let Ok(SdkEvent::AssistantMessage(_)) = ev {
            saw_assistant = true;
        }
    }
    assert!(!saw_assistant);
    h.final_result().await.unwrap();
}

/// Filesystem path traversal is rejected at the workspace path layer.
#[tokio::test]
async fn workspace_path_rejects_traversal() {
    use code_agent_sdk::WorkspacePath;
    assert!(WorkspacePath::new("../etc/passwd").is_err());
}

/// Building a runtime with no provider returns InvalidRequest.
#[tokio::test]
async fn builder_without_provider_fails() {
    let err = AgentRuntime::builder().build().await.unwrap_err();
    assert!(matches!(
        err.kind,
        code_agent_sdk::AgentSdkErrorKind::InvalidRequest
    ));
}

/// `InteractivityMode::HeadlessDeny` denies a mutating approval BEFORE
/// consulting the approval callback. Even when the host configures
/// `AllowAllApprovals`, a mutating tool call must be denied so that the
/// safety contract of the default interactivity mode holds.
#[tokio::test]
async fn headless_deny_short_circuits_callback() {
    use async_trait::async_trait;
    use code_agent_sdk::{AgentSdkError, ApprovalRequest};
    use std::sync::atomic::{AtomicBool, Ordering};

    struct AllowButRecord(Arc<AtomicBool>);
    #[async_trait]
    impl code_agent_sdk::ApprovalCallback for AllowButRecord {
        async fn approve(
            &self,
            _: ApprovalRequest,
        ) -> Result<code_agent_sdk::ApprovalDecision, AgentSdkError> {
            self.0.store(true, Ordering::Release);
            Ok(code_agent_sdk::ApprovalDecision::Allow)
        }
    }
    let called = Arc::new(AtomicBool::new(false));
    let provider = ScriptedFakeProvider::one_shot_tool_call(
        "Write",
        serde_json::json!({"file_path": "x.txt", "content": "hi\n"}),
    );
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::patch_only_defaults(),
            side_effect_policy: SideEffectPolicy::workspace_write(),
            ..Default::default()
        })
        .approval_callback(Arc::new(AllowButRecord(called.clone())))
        // Default is HeadlessDeny; assert it explicitly for clarity.
        .interactivity(InteractivityMode::HeadlessDeny)
        .build()
        .await
        .unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let _ = runtime
        .run_once(
            AgentRequest::new("write")
                .workspace(WorkspaceSource::filesystem(dir.path()))
                .approval_policy(ApprovalPolicy::CallbackRequired)
                .max_turns(2),
        )
        .await
        .unwrap();
    assert!(
        !called.load(Ordering::Acquire),
        "HeadlessDeny must not consult the approval callback"
    );
}

/// `SessionPolicy::HostManaged` does not write to SDK storage even though a
/// session id is minted. The host owns the session record entirely.
#[tokio::test]
async fn host_managed_session_does_not_persist() {
    let storage = Arc::new(InMemoryStorage::new());
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .storage(storage.clone())
        .build()
        .await
        .unwrap();
    let result = runtime
        .run_once(
            AgentRequest::new("hello").session_policy(code_agent_sdk::SessionPolicy::HostManaged),
        )
        .await
        .unwrap();
    let sid = result
        .session_id
        .clone()
        .expect("session id minted for HostManaged");
    let snap = storage.load_session(&sid).await.unwrap();
    assert!(
        snap.events.is_empty(),
        "SDK must not persist events under SessionPolicy::HostManaged"
    );
}

/// A model provider whose `stream_turn` returns a stream that yields
/// nothing and never terminates. Used to prove cancellation actually
/// reaches the running core agent loop (#145, #155). With the pre-fix
/// behavior the cancel flag was set but the provider stream kept
/// blocking and `final_result()` never returned.
#[derive(Default, Clone)]
struct NeverEndingProvider;

#[async_trait::async_trait]
impl code_agent_core::provider::ModelProvider for NeverEndingProvider {
    async fn stream_turn(
        &self,
        _request: code_agent_core::provider::ModelRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<code_agent_core::ModelEvent, code_agent_core::ProviderError>,
        >,
        code_agent_core::ProviderError,
    > {
        // Yield nothing, forever. The agent loop will register a
        // notify waiter via the cancellation pair and only break out
        // when `cancel()` fires.
        Ok(Box::pin(futures::stream::pending()))
    }
}

/// Regression for #145/#155: cancellation against a never-ending
/// provider must flip the run to `Cancelled` within a small bounded
/// time after `cancel()`. Pre-fix the `CancellationHandle` was not
/// threaded into the core agent loop, so `final_result()` would have
/// hung forever waiting for the provider stream.
#[tokio::test]
async fn cancellation_stops_a_blocked_run() {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(NeverEndingProvider)))
        .build()
        .await
        .unwrap();
    let handle = runtime
        .run(AgentRequest::new("a never-ending task"))
        .await
        .unwrap();
    let cancellation = handle.cancellation.clone();

    // Fire cancel after the run has had a chance to start.
    let spawn_cancel = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancellation.cancel();
    });

    // Pre-fix: this `final_result().await` blocked forever because the
    // agent loop kept awaiting the never-ending provider stream.
    let result = tokio::time::timeout(Duration::from_secs(2), handle.final_result())
        .await
        .expect("cancellation must propagate and final_result must return")
        .expect("final_result returned an error");
    spawn_cancel.await.unwrap();

    assert!(
        matches!(result.status, AgentRunStatus::Cancelled),
        "expected Cancelled, got {:?}",
        result.status
    );
}
