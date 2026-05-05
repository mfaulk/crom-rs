//! Verify checkpoint events flow through the agent loop into the session
//! JSONL, and that resuming sees them.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_agent_checkpoint::{
    AgentCheckpointHandle, CheckpointOptions, CheckpointStore, CreateCheckpointRequest,
    LocalCheckpointStore,
};
use code_agent_core::{
    AgentEvent, AgentOptions, CheckpointHandle, ModelEvent, ModelProvider, ModelRequest,
    PermissionCallback, PermissionDecision, PermissionKind, PermissionMode, PermissionRequest,
    ProviderError, RunId, SessionId, Tool, ToolCall, ToolContext, ToolError, ToolRegistry,
    ToolResult,
};
use code_agent_session::{
    append_user_prompt, FsSessionStore, SessionPayload, SessionSinkAdapter, SessionStore,
};
use futures::StreamExt;
use serde_json::json;
use tempfile::TempDir;

#[derive(Clone)]
struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError>
    {
        let mut turns = self.turns.lock().unwrap();
        let events = if turns.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            turns.remove(0)
        };
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }
}

struct WriteTextTool;

#[async_trait]
impl Tool for WriteTextTool {
    fn name(&self) -> &'static str {
        "Write"
    }
    fn description(&self) -> &'static str {
        "write"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::Mutating
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let path = input.get("file_path").and_then(|v| v.as_str()).unwrap();
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        std::fs::write(ctx.cwd.join(path), content).map_err(ToolError::Io)?;
        Ok(ToolResult::ok_text(format!("wrote {path}")))
    }
}

struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

#[tokio::test]
// Checkpoint events emitted during a run land in the session JSONL.
async fn checkpoint_events_persist_to_session() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let session_store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let session_id = session_store.create(&cwd).await.unwrap();
    append_user_prompt(session_store.as_ref(), &cwd, &session_id, "do thing")
        .await
        .unwrap();

    let store = Arc::new(LocalCheckpointStore::new(cwd.clone()));
    let run_id = RunId::new();
    let checkpoint_id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            cwd: cwd.clone(),
            description: Some("user message".into()),
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();

    let opts = CheckpointOptions::default();
    let handle: Arc<dyn CheckpointHandle> = Arc::new(AgentCheckpointHandle::new(
        store.clone(),
        session_id.clone(),
        run_id,
        cwd.clone(),
        checkpoint_id.clone(),
        opts,
    ));

    let provider = Arc::new(ScriptedProvider {
        turns: Arc::new(Mutex::new(vec![
            vec![
                ModelEvent::MessageStart,
                ModelEvent::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "Write".into(),
                    input: json!({"file_path": "x.txt", "content": "hi"}),
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: None,
                },
            ],
            vec![
                ModelEvent::MessageStart,
                ModelEvent::TextDelta {
                    text: "done".into(),
                },
                ModelEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                },
            ],
        ])),
    });

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteTextTool));

    let options = AgentOptions {
        cwd: cwd.clone(),
        max_turns: Some(4),
        allowed_tools: vec!["Write".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        session_id: Some(session_id.clone()),
        ..AgentOptions::default()
    };
    let sink: Arc<dyn code_agent_core::agent::SessionSink> =
        Arc::new(SessionSinkAdapter::new(session_store.clone(), cwd.clone()));
    let agent = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .checkpoint_handle(handle)
        .session_sink(sink)
        .build()
        .unwrap();
    let mut stream = agent.run("do thing");
    while stream.next().await.is_some() {}

    // Now load the session and verify a CheckpointEvent record is present.
    let records = session_store.load(&session_id).await.unwrap();
    let mut saw_checkpoint_event = false;
    for r in &records {
        if let SessionPayload::Event(AgentEvent::CheckpointEvent(p)) = &r.payload {
            if p.event == "snapshot_captured" {
                saw_checkpoint_event = true;
                break;
            }
        }
    }
    assert!(
        saw_checkpoint_event,
        "snapshot_captured event must be persisted to session JSONL"
    );
}

#[tokio::test]
// list_checkpoints reflects checkpoints created across resumes (same session).
async fn resume_can_list_old_checkpoints() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let session_id = SessionId::new();
    let store = LocalCheckpointStore::new(cwd.clone());
    // First "run" creates a checkpoint.
    store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: session_id.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: Some("first".into()),
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    // Simulate a resumed run with a fresh store handle reading the same dir.
    let store2 = LocalCheckpointStore::new(cwd.clone());
    let list = store2.list_checkpoints(&session_id).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].description.as_deref(), Some("first"));
}

#[tokio::test]
// Forks must not share the same checkpoint dir as the source — checkpoints
// are scoped per session, and two sessions must have isolated lineages.
async fn fork_does_not_share_checkpoint_lineage() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store = LocalCheckpointStore::new(cwd.clone());
    let original = SessionId::new();
    store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: original.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: Some("orig".into()),
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    // Forked session id is different; its own list is empty.
    let forked = SessionId::new();
    let list = store.list_checkpoints(&forked).await.unwrap();
    assert!(
        list.is_empty(),
        "forked session must have its own checkpoint lineage"
    );
    // Original session's list still has its checkpoint.
    assert_eq!(store.list_checkpoints(&original).await.unwrap().len(), 1);
}
