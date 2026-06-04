//! M2 session tests: continue, fork, show, export.

use std::sync::Arc;

use async_trait::async_trait;
use code_agent_core::agent::SessionSink;
use code_agent_core::{
    AgentEvent, AgentOptions, ModelEvent, ModelProvider, ModelRequest, PermissionMode,
    ProviderError, SessionId, ToolRegistry,
};
use code_agent_session::{
    append_user_prompt, export_session, fork_session, replay_conversation, summarize_session,
    ExportFormat, FsSessionStore, SessionPayload, SessionSinkAdapter, SessionStore,
};
use futures::stream::{self, BoxStream};
use tempfile::TempDir;

#[derive(Clone)]
struct TextProvider {
    text: String,
}

#[async_trait]
impl ModelProvider for TextProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let evs = vec![
            ModelEvent::TextDelta {
                text: self.text.clone(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ];
        Ok(Box::pin(stream::iter(evs.into_iter().map(Ok))))
    }
}

async fn run_one(
    cwd: &std::path::Path,
    session_id: SessionId,
    sink: Arc<dyn SessionSink>,
    text: &str,
) {
    use futures::StreamExt;
    let provider = Arc::new(TextProvider {
        text: text.to_string(),
    });
    let agent = code_agent_core::AgentBuilder::new()
        .options(AgentOptions {
            cwd: cwd.to_path_buf(),
            session_id: Some(session_id),
            permission_mode: PermissionMode::Default,
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .session_sink(sink)
        .build()
        .unwrap();
    let mut s = agent.run("hi");
    while s.next().await.is_some() {}
}

#[tokio::test]
// Forking copies prior records into a new session id, plus a fork-metadata record.
async fn fork_session_copies_history() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id1 = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id1, "first")
        .await
        .unwrap();
    let sink: Arc<dyn SessionSink> = Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));
    run_one(&cwd, id1.clone(), sink, "done").await;

    let new_id = fork_session(store.as_ref(), &cwd, &id1).await.unwrap();
    assert_ne!(new_id.as_str(), id1.as_str());
    let recs = store.load(&new_id).await.unwrap();
    let has_fork_marker = recs.iter().any(|r| {
        matches!(
            &r.payload,
            SessionPayload::ForkMetadata { source_session_id } if source_session_id == id1.as_str()
        )
    });
    assert!(has_fork_marker, "fork metadata should reference source");
    let conv = replay_conversation(&recs);
    assert!(!conv.messages.is_empty(), "fork copied prior history");
}

#[tokio::test]
// Continue picks up the latest session for cwd and reads its history.
async fn continue_uses_latest_session() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id1 = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id1, "p1").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let id2 = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id2, "p2").await.unwrap();
    let latest = store.latest_for_cwd(&cwd).await.unwrap().unwrap();
    assert_eq!(latest.as_str(), id2.as_str());
}

#[tokio::test]
// Sessions show summarises a session's records.
async fn sessions_show_summary() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id, "go").await.unwrap();
    let sink: Arc<dyn SessionSink> = Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));
    run_one(&cwd, id.clone(), sink, "done").await;

    let summary = summarize_session(store.as_ref(), &id).await.unwrap();
    assert_eq!(summary.session_id, id.as_str());
    assert_eq!(summary.user_prompts, 1);
    assert!(summary.assistant_messages >= 1);
}

#[tokio::test]
// Sessions export jsonl emits one JSON object per line.
async fn sessions_export_jsonl() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id, "p1").await.unwrap();
    let s = export_session(store.as_ref(), &id, ExportFormat::Jsonl)
        .await
        .unwrap();
    let lines: Vec<&str> = s.lines().collect();
    assert!(!lines.is_empty());
    for line in lines {
        let _: serde_json::Value = serde_json::from_str(line).expect("each line is JSON");
    }
}

#[tokio::test]
// Sessions export json emits a single JSON array.
async fn sessions_export_json() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id, "p1").await.unwrap();
    let s = export_session(store.as_ref(), &id, ExportFormat::Json)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&s).unwrap();
    assert!(v.is_array());
}

#[tokio::test]
// Hook decisions, policy decisions, and todo updates persist in JSONL via the sink.
async fn hook_and_policy_decisions_persist() {
    use code_agent_core::{Hook, HookError, HookEventName, HookInput, HookOutput, Hooks};
    use std::sync::Arc as A;

    struct AddCtx;
    #[async_trait]
    impl Hook for AddCtx {
        async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
            Ok(HookOutput::AddContext {
                content: "ctx-data".into(),
            })
        }
    }

    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: A<dyn SessionStore> = A::new(FsSessionStore::new(cwd.clone()));
    let id = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &id, "go").await.unwrap();
    let sink: A<dyn SessionSink> = A::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));

    let provider = A::new(TextProvider { text: "ok".into() });
    let hooks = Hooks::new()
        .add(HookEventName::UserPromptSubmit, "*", A::new(AddCtx))
        .unwrap();
    let agent = code_agent_core::AgentBuilder::new()
        .options(AgentOptions {
            cwd: cwd.clone(),
            session_id: Some(id.clone()),
            permission_mode: PermissionMode::Default,
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .session_sink(sink)
        .hooks(hooks)
        .build()
        .unwrap();
    use futures::StreamExt;
    let mut s = agent.run("hi");
    while s.next().await.is_some() {}

    // Inspect the session file: it must include a HookEvaluated event.
    let recs = store.load(&id).await.unwrap();
    let saw_hook = recs.iter().any(|r| {
        matches!(
            &r.payload,
            SessionPayload::Event(AgentEvent::HookEvaluated(_))
        )
    });
    assert!(saw_hook, "HookEvaluated must be persisted");
}

#[tokio::test]
// Regression: loading a non-existent session id should error so the CLI
// resume path can fail fast instead of silently starting fresh.
async fn loading_missing_session_errors() {
    use code_agent_session::SessionError;
    let dir = TempDir::new().unwrap();
    let store = FsSessionStore::new(dir.path().to_path_buf());
    let id = SessionId::from("does-not-exist".to_string());
    let err = store.load(&id).await.unwrap_err();
    assert!(matches!(err, SessionError::NotFound(_)));
}

#[tokio::test]
// Regression (round 4): summarize_session must SUM total_cost_usd across
// every Result event, not overwrite. Resumed/continued/chat sessions emit
// one Result per run; the previous behavior reported only the last run's
// cost.
async fn session_summary_sums_cost_across_runs() {
    use code_agent_core::{AgentResult, ResultSubtype, RunId, TokenUsage};
    use code_agent_session::{now_iso, SessionPayload, SessionRecord};

    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let id = store.create(&cwd).await.unwrap();

    // Two runs at 0.50 USD each → expect 1.00 total.
    for cost in [0.5, 0.5] {
        let res = AgentResult {
            subtype: ResultSubtype::Success,
            result: Some("ok".into()),
            session_id: id.clone(),
            run_id: Some(RunId::new()),
            num_turns: 1,
            usage: Some(TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            total_cost_usd: Some(cost),
        };
        store
            .append(
                &id,
                SessionRecord::new(
                    now_iso(),
                    cwd.clone(),
                    id.to_string(),
                    SessionPayload::Event(AgentEvent::Result(res)),
                ),
            )
            .await
            .unwrap();
    }

    let summary = summarize_session(store.as_ref(), &id).await.unwrap();
    let total = summary.total_cost_usd.expect("cost should be reported");
    assert!(
        (total - 1.0).abs() < 1e-9,
        "cost should sum to 1.0, got {total}"
    );
    // Tokens should also have summed.
    assert_eq!(summary.total_input_tokens, 200);
    assert_eq!(summary.total_output_tokens, 100);
}
