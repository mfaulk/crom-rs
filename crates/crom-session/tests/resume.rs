use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crom_core::agent::SessionSink;
use crom_core::{
    AgentEvent, AgentOptions, ModelEvent, ModelProvider, ModelRequest, PermissionMode,
    ProviderError, SessionId, ToolRegistry,
};
use crom_session::{
    append_user_prompt, replay_conversation, FsSessionStore, SessionSinkAdapter, SessionStore,
};
use futures::stream::{self, BoxStream, StreamExt};
use tempfile::TempDir;

#[derive(Clone)]
struct CapturingProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    captured_messages: Arc<Mutex<Vec<Vec<crom_core::Message>>>>,
}

impl CapturingProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            captured_messages: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelProvider for CapturingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        self.captured_messages.lock().unwrap().push(req.messages);
        let mut t = self.turns.lock().unwrap();
        let events = if t.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            t.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

#[tokio::test]
// Resuming a session injects the prior conversation history into the next request.
async fn resume_includes_prior_history() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();

    // First run: write to session store via sink, then end with text "first done".
    let provider1 = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "first done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let session_id = store.create(&cwd).await.unwrap();
    append_user_prompt(&*store, &cwd, &session_id, "first")
        .await
        .unwrap();
    let sink: Arc<dyn SessionSink> = Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));

    let agent = crom_core::AgentBuilder::new()
        .options(AgentOptions {
            cwd: cwd.clone(),
            session_id: Some(session_id.clone()),
            permission_mode: PermissionMode::Default,
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider1.clone())
        .tools(ToolRegistry::new())
        .session_sink(sink.clone())
        .build()
        .unwrap();
    let mut s = agent.run("first");
    while s.next().await.is_some() {}

    // Second run: load history and pass it to the agent as initial_history.
    let records = store.load(&session_id).await.unwrap();
    let conv = replay_conversation(&records);
    assert!(
        conv.messages
            .iter()
            .any(|m| matches!(m.role, crom_core::MessageRole::Assistant)),
        "replay should include assistant turn from first run"
    );

    let provider2 = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "second done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));

    append_user_prompt(&*store, &cwd, &session_id, "second")
        .await
        .unwrap();

    let agent2 = crom_core::AgentBuilder::new()
        .options(AgentOptions {
            cwd: cwd.clone(),
            session_id: Some(session_id.clone()),
            permission_mode: PermissionMode::Default,
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider2.clone())
        .tools(ToolRegistry::new())
        .session_sink(sink)
        .initial_history(conv.messages)
        .build()
        .unwrap();
    let mut s = agent2.run("second");
    while let Some(_e) = s.next().await {}

    // Inspect what was sent to the second provider.
    let captured = provider2.captured_messages.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let messages = &captured[0];
    assert!(
        messages.len() >= 3,
        "expected prior + new prompt, got {messages:?}"
    );
}

#[tokio::test]
// query() honors a passed-in session id and writes events to the session store.
async fn query_writes_session_events() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store: Arc<dyn SessionStore> = Arc::new(FsSessionStore::new(cwd.clone()));
    let session_id: SessionId = store.create(&cwd).await.unwrap();
    let sink: Arc<dyn SessionSink> = Arc::new(SessionSinkAdapter::new(store.clone(), cwd.clone()));

    let provider = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "hello".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));

    append_user_prompt(&*store, &cwd, &session_id, "hi")
        .await
        .unwrap();

    let agent = crom_core::AgentBuilder::new()
        .options(AgentOptions {
            cwd: cwd.clone(),
            session_id: Some(session_id.clone()),
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
    let mut got_result = false;
    while let Some(e) = s.next().await {
        if matches!(e, AgentEvent::Result(_)) {
            got_result = true;
        }
    }
    assert!(got_result);

    let records = store.load(&session_id).await.unwrap();
    assert!(records.len() >= 2);
}
