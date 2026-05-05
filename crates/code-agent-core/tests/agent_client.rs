//! Integration tests for the long-lived `AgentClient` runtime.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_agent_core::{
    AgentClient, AgentEvent, AgentOptions, ApprovalMemoryMode, BlockEnvFileWritesHook, Hook,
    HookError, HookEventName, HookInput, HookOutput, Hooks, ModelEvent, ModelProvider,
    ModelRequest, PermissionMode, ProviderError, ResultSubtype, ToolCall, ToolRegistry, ToolRule,
};
use futures::stream::{self, BoxStream};
use serde_json::json;
use tokio::time::{sleep, Duration};

/// A scripted provider that emits a queue of pre-built turns. Each call to
/// `stream_turn` consumes the next turn. When a per-turn delay is set the
/// provider sleeps before emitting events so tests can reliably observe a
/// "currently running" state.
#[derive(Clone)]
struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    delay_ms: u64,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            delay_ms: 0,
        }
    }

    fn with_delay(turns: Vec<Vec<ModelEvent>>, delay_ms: u64) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            delay_ms,
        }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        if self.delay_ms > 0 {
            sleep(Duration::from_millis(self.delay_ms)).await;
        }
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

fn turn_text(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::MessageStart,
        ModelEvent::TextDelta {
            text: text.to_string(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]
}

#[tokio::test]
// AgentClient::send_user_message receives an event stream and ends with a
// Result event for a single short prompt.
async fn client_runs_single_prompt() {
    let provider = Arc::new(ScriptedProvider::new(vec![turn_text("hello")]));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .build()
        .await
        .unwrap();

    client.send_user_message("hi").await.unwrap();

    let mut got_result = false;
    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::Result(r) = ev {
            assert_eq!(r.subtype, ResultSubtype::Success);
            got_result = true;
            break;
        }
    }
    assert!(got_result);
    let _ = client.shutdown().await;
}

#[tokio::test]
// Sending a second user_message while the first run is busy queues it.
// After the first run completes, the queued message is processed.
async fn client_queues_second_message() {
    let provider = Arc::new(ScriptedProvider::with_delay(
        vec![turn_text("first"), turn_text("second")],
        50,
    ));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .build()
        .await
        .unwrap();

    client.send_user_message("a").await.unwrap();
    // Send the second message while the first is still busy.
    sleep(Duration::from_millis(10)).await;
    client.send_user_message("b").await.unwrap();

    let mut results = 0u32;
    let mut texts: Vec<String> = Vec::new();
    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::Result(r) = ev {
            results += 1;
            if let Some(t) = r.result {
                texts.push(t);
            }
            if results >= 2 {
                break;
            }
        }
    }
    assert_eq!(results, 2, "expected both runs to produce results");
    assert!(texts.iter().any(|t| t.contains("first")));
    assert!(texts.iter().any(|t| t.contains("second")));
    let _ = client.shutdown().await;
}

#[tokio::test]
// Interrupting an in-flight run produces an Interrupted event and a Cancelled Result.
async fn client_interrupts_running_agent() {
    // 200 ms per turn so we have a clear window to send interrupt.
    let provider = Arc::new(ScriptedProvider::with_delay(
        vec![turn_text("delayed")],
        300,
    ));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .build()
        .await
        .unwrap();

    client.send_user_message("hi").await.unwrap();
    sleep(Duration::from_millis(50)).await;
    client.interrupt().await.unwrap();

    let mut saw_interrupt = false;
    let mut final_subtype = None;
    while let Some(ev) = client.next_event().await.unwrap() {
        match ev {
            AgentEvent::Interrupted(_) => saw_interrupt = true,
            AgentEvent::Result(r) => {
                final_subtype = Some(r.subtype);
                break;
            }
            _ => {}
        }
    }
    assert!(saw_interrupt, "expected Interrupted event");
    assert_eq!(final_subtype, Some(ResultSubtype::Cancelled));
    let _ = client.shutdown().await;
}

struct AddContextHook;

#[async_trait]
impl Hook for AddContextHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::UserPromptSubmit {
            return Ok(HookOutput::Continue);
        }
        Ok(HookOutput::AddContext {
            content: "extra-context-marker".into(),
        })
    }
}

#[tokio::test]
// UserPromptSubmit hook can inject extra context, which appears as part of
// the assistant's downstream behavior. We verify the system prompt sent to
// the provider includes the marker.
async fn user_prompt_hook_adds_context() {
    #[derive(Clone)]
    struct CapturingProvider {
        last_system: Arc<Mutex<Option<String>>>,
    }
    #[async_trait]
    impl ModelProvider for CapturingProvider {
        async fn stream_turn(
            &self,
            req: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
            *self.last_system.lock().unwrap() = req.system.clone();
            Ok(Box::pin(stream::iter(turn_text("ok").into_iter().map(Ok))))
        }
    }

    let last_system = Arc::new(Mutex::new(None));
    let provider = Arc::new(CapturingProvider {
        last_system: last_system.clone(),
    });

    let hooks = Hooks::new().user_prompt_submit(Arc::new(AddContextHook));

    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .hooks(hooks)
        .build()
        .await
        .unwrap();

    client.send_user_message("hi").await.unwrap();

    while let Some(ev) = client.next_event().await.unwrap() {
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }

    let sys = last_system.lock().unwrap().clone().unwrap_or_default();
    assert!(sys.contains("extra-context-marker"));
    let _ = client.shutdown().await;
}

struct DenyHook;

#[async_trait]
impl Hook for DenyHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.tool_name.as_deref() == Some("Bash") {
            Ok(HookOutput::Deny {
                reason: "no bash".into(),
            })
        } else {
            Ok(HookOutput::Continue)
        }
    }
}

#[tokio::test]
// PreToolUse hook denying Bash blocks the call: the tool result shows
// denial and the side-effect tool was not called.
async fn pre_tool_use_hook_denies() {
    use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

    struct EchoTool {
        ran: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Tool for EchoTool {
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
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            *self.ran.lock().unwrap() = true;
            Ok(ToolResult::ok_text("ok"))
        }
    }

    let ran = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Bash".into(),
                input: json!({"command": "echo hi"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        turn_text("done"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool { ran: ran.clone() }));
    let hooks = Hooks::new().pre_tool_use("*", Arc::new(DenyHook));

    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec!["Bash".into()],
            max_turns: Some(3),
            permission_mode: PermissionMode::AcceptEdits,
            approval_memory: ApprovalMemoryMode::None,
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(tools)
        .hooks(hooks)
        .build()
        .await
        .unwrap();
    client.send_user_message("go").await.unwrap();

    let mut saw_denied = false;
    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::ToolCallDenied(_) = ev {
            saw_denied = true;
        }
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }
    assert!(saw_denied);
    assert!(!*ran.lock().unwrap(), "tool must not run");
    let _ = client.shutdown().await;
}

#[tokio::test]
// FilePathGlob disallow rule blocks Edit on `.env` files even with AcceptEdits.
async fn disallow_path_glob_blocks_env_edit() {
    use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

    struct EditTool {
        ran: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Tool for EditTool {
        fn name(&self) -> &'static str {
            "Edit"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::Mutating
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            *self.ran.lock().unwrap() = true;
            Ok(ToolResult::ok_text("edited"))
        }
    }

    let ran = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Edit".into(),
                input: json!({"file_path": ".env", "old_string": "x", "new_string": "y"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        turn_text("done"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EditTool { ran: ran.clone() }));

    let opts = AgentOptions {
        allowed_tools: vec!["Edit".into()],
        max_turns: Some(3),
        permission_mode: PermissionMode::AcceptEdits,
        disallowed_tool_rules: vec![ToolRule::FilePathGlob {
            tool: "Edit".into(),
            pattern: "**/.env*".into(),
        }],
        approval_memory: ApprovalMemoryMode::None,
        ..AgentOptions::default()
    };
    // Also wire the built-in env-block hook to verify hook layering.
    let hooks =
        Hooks::new().pre_tool_use("Edit|Write", Arc::new(BlockEnvFileWritesHook::default()));

    let mut client = AgentClient::builder()
        .options(opts)
        .provider(provider)
        .tools(tools)
        .hooks(hooks)
        .build()
        .await
        .unwrap();
    client.send_user_message("go").await.unwrap();

    let mut saw_denied = false;
    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::ToolCallDenied(_) = ev {
            saw_denied = true;
        }
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }
    assert!(saw_denied);
    assert!(!*ran.lock().unwrap());
    let _ = client.shutdown().await;
}

#[tokio::test]
// Stop hook fires on max-turns termination.
async fn stop_hook_fires_on_max_turns() {
    struct CountingStop {
        count: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl Hook for CountingStop {
        async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
            *self.count.lock().unwrap() += 1;
            Ok(HookOutput::Continue)
        }
    }

    let count = Arc::new(Mutex::new(0u32));
    // Provider keeps emitting tool calls forever; agent should hit max_turns.
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t".into(),
                name: "Echo".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t".into(),
                name: "Echo".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
    ]));

    use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};
    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text("ok"))
        }
    }

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let hooks = Hooks::new().stop(Arc::new(CountingStop {
        count: count.clone(),
    }));

    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec!["Echo".into()],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(tools)
        .hooks(hooks)
        .build()
        .await
        .unwrap();
    client.send_user_message("go").await.unwrap();

    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::Result(r) = ev {
            assert_eq!(r.subtype, ResultSubtype::ErrorMaxTurns);
            break;
        }
    }
    assert_eq!(*count.lock().unwrap(), 1, "stop hook fires once");
    let _ = client.shutdown().await;
}

#[tokio::test]
// Regression: AgentClient must emit UserToolResult events to the public
// stream after appending them to internal history (not silently swallow).
async fn client_forwards_user_tool_result_events() {
    use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

    struct EchoTool;
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text("echoed"))
        }
    }

    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Echo".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        turn_text("done"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec!["Echo".into()],
            max_turns: Some(3),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(tools)
        .build()
        .await
        .unwrap();
    client.send_user_message("hi").await.unwrap();

    let mut saw_tool_result = false;
    while let Some(ev) = client.next_event().await.unwrap() {
        if let AgentEvent::UserToolResult(r) = &ev {
            assert!(r.content.contains("echoed"));
            saw_tool_result = true;
        }
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }
    assert!(
        saw_tool_result,
        "AgentClient must forward UserToolResult to the public stream"
    );
    let _ = client.shutdown().await;
}

#[tokio::test]
// Regression: PreToolUse hook returning Ask must override AcceptEdits
// auto-allow on Edit/Write — the user must actually be prompted.
async fn pre_tool_use_ask_overrides_accept_edits() {
    use code_agent_core::{
        PermissionCallback, PermissionDecision, PermissionKind, PermissionRequest, Tool,
        ToolContext, ToolError, ToolResult,
    };

    struct AskHook;
    #[async_trait]
    impl Hook for AskHook {
        async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
            Ok(HookOutput::Ask {
                reason: "needs human review".into(),
            })
        }
    }

    struct CountingDeny {
        count: Arc<Mutex<u32>>,
    }
    #[async_trait]
    impl PermissionCallback for CountingDeny {
        async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
            *self.count.lock().unwrap() += 1;
            PermissionDecision::Deny {
                reason: "deny in test".into(),
            }
        }
    }

    struct EditTool {
        ran: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Tool for EditTool {
        fn name(&self) -> &'static str {
            "Edit"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::Mutating
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            *self.ran.lock().unwrap() = true;
            Ok(ToolResult::ok_text("edited"))
        }
    }

    let count = Arc::new(Mutex::new(0u32));
    let ran = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Edit".into(),
                input: json!({"file_path": "x.rs", "old_string": "a", "new_string": "b"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        turn_text("done"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EditTool { ran: ran.clone() }));
    let hooks = Hooks::new().pre_tool_use("Edit", Arc::new(AskHook));

    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec!["Edit".into()],
            max_turns: Some(3),
            permission_mode: PermissionMode::AcceptEdits,
            permission_callback: Some(Arc::new(CountingDeny {
                count: count.clone(),
            })),
            approval_memory: ApprovalMemoryMode::None,
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(tools)
        .hooks(hooks)
        .build()
        .await
        .unwrap();
    client.send_user_message("go").await.unwrap();
    while let Some(ev) = client.next_event().await.unwrap() {
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }

    assert_eq!(
        *count.lock().unwrap(),
        1,
        "callback must be invoked when hook says Ask, even under AcceptEdits"
    );
    assert!(
        !*ran.lock().unwrap(),
        "tool must not execute when callback denies"
    );
    let _ = client.shutdown().await;
}

#[tokio::test]
// Regression: dontAsk should permit tools listed in the simple-name
// `allowed_tools` Vec<String>, not just structured `allowed_tool_rules`.
async fn dont_ask_honors_allowed_tools_name_list() {
    use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

    struct EchoTool {
        ran: Arc<Mutex<bool>>,
    }
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn description(&self) -> &'static str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object"})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            *self.ran.lock().unwrap() = true;
            Ok(ToolResult::ok_text("echoed"))
        }
    }

    let ran = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Echo".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        turn_text("done"),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool { ran: ran.clone() }));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec!["Echo".into()],
            allowed_tool_rules: vec![], // intentionally empty
            max_turns: Some(3),
            permission_mode: PermissionMode::DontAsk,
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(tools)
        .build()
        .await
        .unwrap();
    client.send_user_message("go").await.unwrap();
    while let Some(ev) = client.next_event().await.unwrap() {
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }
    assert!(
        *ran.lock().unwrap(),
        "dontAsk must allow tools listed in allowed_tools name list"
    );
    let _ = client.shutdown().await;
}

#[tokio::test]
// Regression: shutting down while a run is active must complete cleanly
// (without hanging waiting for the runtime task to settle).
async fn shutdown_during_active_run_does_not_hang() {
    let provider = Arc::new(ScriptedProvider::with_delay(
        vec![turn_text("delayed")],
        500,
    ));
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .build()
        .await
        .unwrap();
    client.send_user_message("hi").await.unwrap();
    sleep(Duration::from_millis(50)).await;

    // shutdown should return within a reasonable bound; if it hangs the test
    // will time out (tokio test runners ultimately abort).
    let r = tokio::time::timeout(Duration::from_secs(3), client.shutdown()).await;
    assert!(r.is_ok(), "shutdown must not hang during an active run");
}

#[tokio::test]
// Regression: in-session history records the user prompt BEFORE the
// assistant's reply, so the next message in the session sees the
// prompt-then-assistant ordering it expects.
async fn user_prompt_persisted_before_assistant_in_history() {
    // Capturing provider records messages it sees on its second call.
    #[derive(Clone)]
    struct CapturingProvider {
        seen_messages: Arc<Mutex<Vec<Vec<code_agent_core::Message>>>>,
        turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    }
    #[async_trait]
    impl ModelProvider for CapturingProvider {
        async fn stream_turn(
            &self,
            req: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
            self.seen_messages.lock().unwrap().push(req.messages);
            let mut t = self.turns.lock().unwrap();
            let evs = if t.is_empty() {
                vec![ModelEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }]
            } else {
                t.remove(0)
            };
            Ok(Box::pin(stream::iter(evs.into_iter().map(Ok))))
        }
    }

    let captured = Arc::new(Mutex::new(Vec::<Vec<code_agent_core::Message>>::new()));
    let provider = Arc::new(CapturingProvider {
        seen_messages: captured.clone(),
        turns: Arc::new(Mutex::new(vec![turn_text("first"), turn_text("second")])),
    });
    let mut client = AgentClient::builder()
        .options(AgentOptions {
            allowed_tools: vec![],
            max_turns: Some(2),
            ..AgentOptions::default()
        })
        .provider(provider)
        .tools(ToolRegistry::new())
        .build()
        .await
        .unwrap();
    client.send_user_message("first-prompt").await.unwrap();
    while let Some(ev) = client.next_event().await.unwrap() {
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }
    client.send_user_message("second-prompt").await.unwrap();
    while let Some(ev) = client.next_event().await.unwrap() {
        if matches!(ev, AgentEvent::Result(_)) {
            break;
        }
    }

    let second = {
        let captured = captured.lock().unwrap();
        assert_eq!(captured.len(), 2, "two requests");
        captured[1].clone()
    };
    // Order must be: user("first-prompt") → assistant("first") → user("second-prompt").
    use code_agent_core::{ContentBlock, MessageRole};
    let user_first_idx = second.iter().position(|m| {
        matches!(m.role, MessageRole::User)
            && matches!(m.content.first(), Some(ContentBlock::Text { text }) if text == "first-prompt")
    });
    let assistant_idx = second
        .iter()
        .position(|m| matches!(m.role, MessageRole::Assistant));
    let user_second_idx = second.iter().position(|m| {
        matches!(m.role, MessageRole::User)
            && matches!(m.content.first(), Some(ContentBlock::Text { text }) if text == "second-prompt")
    });
    let user_first = user_first_idx.expect("first user prompt missing");
    let assistant = assistant_idx.expect("assistant turn missing");
    let user_second = user_second_idx.expect("second user prompt missing");
    assert!(
        user_first < assistant && assistant < user_second,
        "history ordering should be user → assistant → user, got user_first={user_first} assistant={assistant} user_second={user_second}"
    );
    let _ = client.shutdown().await;
}
