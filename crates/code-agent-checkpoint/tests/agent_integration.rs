//! Integration tests that drive the agent loop through a scripted model
//! provider and verify checkpointing behavior end-to-end.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_agent_checkpoint::{
    AgentCheckpointHandle, BashCheckpointPolicy, CheckpointFailureMode, CheckpointOptions,
    CheckpointStore, CreateCheckpointRequest, LocalCheckpointStore, RewindRequest,
};
use code_agent_core::{
    query, AgentEvent, AgentOptions, CheckpointHandle, ModelEvent, ModelProvider, ModelRequest,
    PermissionCallback, PermissionDecision, PermissionKind, PermissionMode, PermissionRequest,
    ProviderError, RunId, SessionId, Tool, ToolCall, ToolContext, ToolError, ToolRegistry,
    ToolResult,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::json;
use tempfile::TempDir;

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

/// A trivial Write-like tool: writes content to a workspace-relative path.
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
        let path = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidInput("file_path missing".into()))?;
        let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
        let p = ctx.cwd.join(path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::write(&p, content).map_err(ToolError::Io)?;
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

fn write_call(id: &str, file_path: &str, content: &str) -> ModelEvent {
    ModelEvent::ToolCall(ToolCall {
        id: id.into(),
        name: "Write".into(),
        input: json!({"file_path": file_path, "content": content}),
    })
}

#[tokio::test]
// Agent run with checkpointing: a Write to a NEW file gets snapshotted
// (MissingBeforeChange), and rewind deletes the created file.
async fn write_new_file_via_agent_loop_then_rewind_deletes() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let session_id = SessionId::new();
    let run_id = RunId::new();
    let store = Arc::new(LocalCheckpointStore::new(cwd.clone()));
    let checkpoint_id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            cwd: cwd.clone(),
            description: Some("test".into()),
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();

    let opts = CheckpointOptions {
        enabled: true,
        failure_mode: CheckpointFailureMode::Strict,
        create_on_user_message: true,
        limits: Default::default(),
        bash_policy: BashCheckpointPolicy::AllowWithWarning,
    };
    let handle: Arc<dyn CheckpointHandle> = Arc::new(AgentCheckpointHandle::new(
        store.clone(),
        session_id.clone(),
        run_id,
        cwd.clone(),
        checkpoint_id.clone(),
        opts,
    ));

    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::MessageStart,
            write_call("c1", "out.txt", "hello"),
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
    ]));

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

    let agent = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .checkpoint_handle(handle)
        .build()
        .unwrap();

    let mut stream = agent.run("write a file");
    let mut saw_snapshot = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::CheckpointEvent(p) = &ev {
            if p.event == "snapshot_captured" {
                saw_snapshot = true;
                let kind = p
                    .data
                    .get("capture")
                    .and_then(|v| v.get("kind"))
                    .and_then(|v| v.as_str());
                assert_eq!(kind, Some("missing_before_change"));
            }
        }
    }
    assert!(saw_snapshot, "expected snapshot_captured event");
    assert!(
        cwd.join("out.txt").exists(),
        "file should have been written"
    );

    // Rewind: file should be deleted.
    let report = store
        .rewind(RewindRequest {
            checkpoint_id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.deleted_files.len(), 1);
    assert!(!cwd.join("out.txt").exists());
}

/// A failing capture-failure-mode handle that reports deny in Strict mode.
struct AlwaysFailingCheckpointHandle;

#[async_trait]
impl CheckpointHandle for AlwaysFailingCheckpointHandle {
    async fn capture_for_tool(
        &self,
        _session_id: &SessionId,
        _run_id: &RunId,
        _tool_name: &str,
        _tool_input: &serde_json::Value,
    ) -> code_agent_core::CheckpointCaptureResult {
        code_agent_core::CheckpointCaptureResult {
            events: Vec::new(),
            deny: Some("simulated capture failure".into()),
        }
    }
    async fn before_bash(
        &self,
        _session_id: &SessionId,
        _run_id: &RunId,
        _command: &str,
        _explicit: bool,
    ) -> code_agent_core::CheckpointBashAction {
        Default::default()
    }
    fn checkpoint_id(&self) -> Option<String> {
        Some("ckpt_fake".into())
    }
}

#[tokio::test]
// Strict checkpoint failure must deny the mutating tool call. The file
// must NOT be written.
async fn strict_capture_failure_denies_mutation() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::MessageStart,
            write_call("c1", "out.txt", "should not exist"),
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
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteTextTool));
    let options = AgentOptions {
        cwd: cwd.clone(),
        max_turns: Some(4),
        allowed_tools: vec!["Write".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let handle: Arc<dyn CheckpointHandle> = Arc::new(AlwaysFailingCheckpointHandle);
    let agent = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .checkpoint_handle(handle)
        .build()
        .unwrap();
    let mut stream = agent.run("go");
    let mut saw_denial = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::ToolCallDenied(d) = &ev {
            if d.reason.contains("checkpoint") || d.reason.contains("simulated") {
                saw_denial = true;
            }
        }
    }
    assert!(saw_denial, "expected ToolCallDenied with checkpoint reason");
    assert!(!cwd.join("out.txt").exists(), "file must not be created");
}

#[tokio::test]
// When checkpointing is NOT enabled (no handle), the agent works as before
// and Write executes normally. Regression check: M1/M2 behavior preserved.
async fn no_checkpoint_handle_works_as_m1() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::MessageStart,
            write_call("c1", "out.txt", "hi"),
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
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteTextTool));
    let options = AgentOptions {
        cwd: cwd.clone(),
        max_turns: Some(4),
        allowed_tools: vec!["Write".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let mut s = query(
        "go".to_string(),
        options,
        provider as Arc<dyn ModelProvider>,
        tools,
    )
    .await
    .unwrap();
    while s.next().await.is_some() {}
    assert!(cwd.join("out.txt").exists());
}

#[tokio::test]
// Bash policy: RequireExplicitApproval emits a warning AND, when the
// command was not explicitly approved, denies the call. The deny does NOT
// run the actual tool.
async fn bash_require_explicit_approval_denies_implicit() {
    use code_agent_core::CheckpointBashAction;

    struct Handle;

    #[async_trait]
    impl CheckpointHandle for Handle {
        async fn capture_for_tool(
            &self,
            _: &SessionId,
            _: &RunId,
            _: &str,
            _: &serde_json::Value,
        ) -> code_agent_core::CheckpointCaptureResult {
            Default::default()
        }
        async fn before_bash(
            &self,
            _: &SessionId,
            _: &RunId,
            command: &str,
            explicit: bool,
        ) -> CheckpointBashAction {
            let events = vec![code_agent_core::CheckpointEventPayload {
                event: "bash_warning".into(),
                data: serde_json::json!({"command": command, "explicit": explicit}),
            }];
            CheckpointBashAction {
                events,
                deny: if explicit {
                    None
                } else {
                    Some("must approve".into())
                },
            }
        }
        fn checkpoint_id(&self) -> Option<String> {
            None
        }
    }

    /// Tool that records whether it was called.
    struct BashStub {
        called: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Tool for BashStub {
        fn name(&self) -> &'static str {
            "Bash"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::Execution
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            *self.called.lock().unwrap() = true;
            Ok(ToolResult::ok_text("ran"))
        }
    }

    let called = Arc::new(Mutex::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BashStub {
        called: called.clone(),
    }));

    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::MessageStart,
            ModelEvent::ToolCall(ToolCall {
                id: "b1".into(),
                name: "Bash".into(),
                input: json!({"command": "ls"}),
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
    ]));

    // dontAsk so policy auto-approves Bash without user prompt; this maps
    // to PolicyDecisionSource::PermissionMode (NOT explicit). The handle
    // should then deny.
    let options = AgentOptions {
        cwd: TempDir::new().unwrap().path().to_path_buf(),
        max_turns: Some(4),
        allowed_tools: vec!["Bash".into()],
        permission_mode: PermissionMode::DontAsk,
        ..AgentOptions::default()
    };
    let handle: Arc<dyn CheckpointHandle> = Arc::new(Handle);
    let agent = code_agent_core::AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .checkpoint_handle(handle)
        .build()
        .unwrap();
    let mut stream = agent.run("ls files");
    let mut saw_warning = false;
    let mut saw_deny = false;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::CheckpointEvent(p) if p.event == "bash_warning" => saw_warning = true,
            AgentEvent::ToolCallDenied(_) => saw_deny = true,
            _ => {}
        }
    }
    assert!(saw_warning, "expected bash_warning event");
    assert!(saw_deny, "expected ToolCallDenied");
    assert!(!*called.lock().unwrap(), "Bash must not have run");
}
