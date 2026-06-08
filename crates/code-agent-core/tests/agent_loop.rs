use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use code_agent_core::{
    query, AgentEvent, AgentOptions, AgentResult, ModelEvent, ModelProvider, ModelRequest,
    PermissionCallback, PermissionDecision, PermissionKind, PermissionMode, PermissionRequest,
    ProviderError, ResultSubtype, Tool, ToolContext, ToolError, ToolLoadingMode, ToolRegistry,
    ToolResult, ToolRule,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::json;

#[derive(Clone)]
struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    seen: Arc<Mutex<u32>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            seen: Arc::new(Mutex::new(0)),
        }
    }
    #[allow(dead_code)]
    fn turns_seen(&self) -> u32 {
        *self.seen.lock().unwrap()
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        *self.seen.lock().unwrap() += 1;
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

#[derive(Clone)]
struct CapturingProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    requests: Arc<Mutex<Vec<Vec<String>>>>,
}

impl CapturingProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn request_tool_names(&self) -> Vec<Vec<String>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for CapturingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(
            req.tools
                .iter()
                .map(|tool| tool.name.clone())
                .collect::<Vec<_>>(),
        );
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

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text(input.to_string()))
    }
}

struct NamedTool {
    name: &'static str,
    description: &'static str,
    kind: PermissionKind,
}

#[async_trait]
impl Tool for NamedTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object", "properties": {"query": {"type": "string", "description": "query text"}}})
    }

    fn permission_kind(&self) -> PermissionKind {
        self.kind
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text(format!("{} ok", self.name)))
    }
}

struct WriteSideEffectTool {
    flag: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for WriteSideEffectTool {
    fn name(&self) -> &str {
        "WriteFlag"
    }
    fn description(&self) -> &str {
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
        *self.flag.lock().unwrap() = true;
        Ok(ToolResult::ok_text("ok"))
    }
}

struct ExecTool {
    flag: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for ExecTool {
    fn name(&self) -> &str {
        "Exec"
    }
    fn description(&self) -> &str {
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
        *self.flag.lock().unwrap() = true;
        Ok(ToolResult::ok_text("ran"))
    }
}

struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

struct AlwaysDeny;

#[async_trait]
impl PermissionCallback for AlwaysDeny {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: "test deny".into(),
        }
    }
}

async fn run(
    provider: Arc<dyn ModelProvider>,
    options: AgentOptions,
    tools: ToolRegistry,
    prompt: &str,
) -> Vec<AgentEvent> {
    let mut s = query(prompt, options, provider, tools).await.unwrap();
    let mut events = Vec::new();
    while let Some(e) = s.next().await {
        events.push(e);
    }
    events
}

fn final_result(events: &[AgentEvent]) -> &AgentResult {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Result(r) => Some(r),
            _ => None,
        })
        .expect("no result event")
}

#[tokio::test]
// Agent loop ends with Success when the model emits no tool calls.
async fn loop_completes_on_no_tool_calls() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        ModelEvent::MessageStart,
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let opts = AgentOptions {
        max_turns: Some(2),
        allowed_tools: vec![],
        ..AgentOptions::default()
    };
    let events = run(provider, opts, ToolRegistry::new(), "hi").await;
    let r = final_result(&events);
    assert_eq!(r.subtype, ResultSubtype::Success);
    assert_eq!(r.result.as_deref(), Some("done"));
}

#[tokio::test]
// Agent loop emits ErrorMaxTurns when the model keeps requesting tool calls.
async fn loop_stops_at_max_turns() {
    use code_agent_core::ToolCall;
    let make_turn = || {
        vec![
            ModelEvent::MessageStart,
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "Echo".into(),
                input: json!({"v": 1}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ]
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        make_turn(),
        make_turn(),
        make_turn(),
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let opts = AgentOptions {
        max_turns: Some(2),
        allowed_tools: vec!["Echo".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let events = run(provider.clone(), opts, tools, "hi").await;
    let r = final_result(&events);
    assert_eq!(r.subtype, ResultSubtype::ErrorMaxTurns);
}

fn count_tool_starts(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallStarted(_)))
        .count()
}

fn tool_then_final_provider(tool_turns: usize) -> Arc<ScriptedProvider> {
    use code_agent_core::ToolCall;
    let mut turns: Vec<Vec<ModelEvent>> = Vec::new();
    for i in 0..tool_turns {
        turns.push(vec![
            ModelEvent::MessageStart,
            ModelEvent::ToolCall(ToolCall {
                id: format!("t{i}"),
                name: "Echo".into(),
                input: json!({"v": i}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ]);
    }
    turns.push(vec![
        ModelEvent::MessageStart,
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]);
    Arc::new(ScriptedProvider::new(turns))
}

#[tokio::test]
// A run that uses exactly max_tool_calls and then finishes naturally ends in
// Success with all max calls executed — the cap only blocks the (max+1)-th.
async fn loop_allows_exactly_max_tool_calls() {
    let provider = tool_then_final_provider(2);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let opts = AgentOptions {
        max_turns: Some(8),
        max_tool_calls: Some(2),
        allowed_tools: vec!["Echo".into()],
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let events = run(provider, opts, tools, "hi").await;
    assert_eq!(count_tool_starts(&events), 2);
    assert_eq!(final_result(&events).subtype, ResultSubtype::Success);
}

#[tokio::test]
// A run whose model wants more than max_tool_calls has the (max+1)-th call
// blocked: exactly max calls start and the result is ErrorMaxToolCalls.
async fn loop_blocks_call_exceeding_max_tool_calls() {
    let provider = tool_then_final_provider(5);
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let opts = AgentOptions {
        max_turns: Some(8),
        max_tool_calls: Some(2),
        allowed_tools: vec!["Echo".into()],
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let events = run(provider, opts, tools, "hi").await;
    assert_eq!(count_tool_starts(&events), 2);
    assert_eq!(
        final_result(&events).subtype,
        ResultSubtype::ErrorMaxToolCalls
    );
}

// Regression for #160: dropping the `Agent::run` stream early must stop the
// spawned loop. Before the fix, send sites used `let _ = tx.send(..).await`
// and ignored `SendError`, so the loop kept calling the provider and
// running tools whose output nobody could observe. With the fix, a failed
// send (receiver gone) flips the cancel flag and the loop short-circuits
// before the next provider call.
#[tokio::test]
async fn dropping_stream_stops_loop_before_next_provider_call() {
    use code_agent_core::ToolCall;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    // Provider that counts how many turns it was asked to stream. Each turn
    // requests a tool call, so absent cancellation the loop would keep
    // calling it until max_turns.
    #[derive(Clone)]
    struct CountingProvider {
        turns_started: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelProvider for CountingProvider {
        async fn stream_turn(
            &self,
            _req: ModelRequest,
        ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
            self.turns_started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(stream::iter(
                vec![
                    ModelEvent::MessageStart,
                    ModelEvent::ToolCall(ToolCall {
                        id: "t1".into(),
                        name: "Block".into(),
                        input: json!({}),
                    }),
                    ModelEvent::MessageStop {
                        stop_reason: Some("tool_use".into()),
                        usage: None,
                    },
                ]
                .into_iter()
                .map(Ok),
            )))
        }
    }

    // Tool that signals when it starts and blocks until released, giving the
    // test a deterministic point to drop the stream while a turn is in
    // flight.
    struct BlockingTool {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl Tool for BlockingTool {
        fn name(&self) -> &str {
            "Block"
        }
        fn description(&self) -> &str {
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
            self.entered.notify_one();
            self.release.notified().await;
            Ok(ToolResult::ok_text("unblocked"))
        }
    }

    let turns_started = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(CountingProvider {
        turns_started: turns_started.clone(),
    });
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BlockingTool {
        entered: entered.clone(),
        release: release.clone(),
    }));
    let opts = AgentOptions {
        max_turns: Some(5),
        allowed_tools: vec!["Block".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };

    let mut stream = query("hi", opts, provider, tools).await.unwrap();

    // Pump the stream until the tool reports it has started executing. At
    // this point the first provider turn has fully streamed and the loop is
    // parked inside the tool future.
    let pump = tokio::spawn(async move { while stream.next().await.is_some() {} });
    entered.notified().await;

    // The first (and only) provider turn must have happened.
    assert_eq!(turns_started.load(Ordering::SeqCst), 1);

    // Drop the consumer: abort the pump task so the ReceiverStream (and its
    // channel receiver) is dropped while the tool is still blocked.
    pump.abort();
    let _ = pump.await;

    // Release the tool so the loop resumes. Pushing the tool result now
    // fails to send (receiver gone), which must flip the cancel flag and
    // stop the loop before it requests a second provider turn.
    release.notify_one();

    // Give the spawned loop time to either correctly stop or (on regression)
    // call the provider again.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if turns_started.load(Ordering::SeqCst) > 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert_eq!(
        turns_started.load(Ordering::SeqCst),
        1,
        "loop made a second provider call after the stream was dropped; \
         dropping the stream did not cancel the run"
    );
}

#[tokio::test]
// PermissionMode::Default requires approval for mutating and execution tools.
async fn default_mode_denies_mutating_without_approval() {
    use code_agent_core::ToolCall;
    let flag = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "WriteFlag".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta { text: "fin".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteSideEffectTool { flag: flag.clone() }));
    let opts = AgentOptions {
        max_turns: Some(3),
        allowed_tools: vec!["WriteFlag".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysDeny)),
        ..AgentOptions::default()
    };
    let events = run(provider, opts, tools, "do it").await;
    let r = final_result(&events);
    assert_eq!(r.subtype, ResultSubtype::Success);
    assert!(!*flag.lock().unwrap(), "tool must not have run");
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCallDenied(_))));
}

#[tokio::test]
// PermissionMode::AcceptEdits auto-approves Mutating tools, but Execution still needs approval.
async fn accept_edits_auto_approves_mutating_but_not_exec() {
    use code_agent_core::ToolCall;
    let mut_flag = Arc::new(Mutex::new(false));
    let exec_flag = Arc::new(Mutex::new(false));

    // Turn 1: emit mutating call. Turn 2: emit exec call. Turn 3: final text.
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t1".into(),
                name: "WriteFlag".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "t2".into(),
                name: "Exec".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta { text: "fin".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteSideEffectTool {
        flag: mut_flag.clone(),
    }));
    tools.register(Arc::new(ExecTool {
        flag: exec_flag.clone(),
    }));
    let opts = AgentOptions {
        max_turns: Some(4),
        allowed_tools: vec!["WriteFlag".into(), "Exec".into()],
        permission_mode: PermissionMode::AcceptEdits,
        permission_callback: Some(Arc::new(AlwaysDeny)),
        ..AgentOptions::default()
    };
    let _events = run(provider, opts, tools, "go").await;
    assert!(*mut_flag.lock().unwrap(), "Edits should auto-approve");
    assert!(!*exec_flag.lock().unwrap(), "Bash/Exec needs approval");
}

#[tokio::test]
// PermissionMode::Plan returns the first turn's text without executing tools.
async fn plan_mode_does_not_execute_tools() {
    use code_agent_core::ToolCall;
    let flag = Arc::new(Mutex::new(false));
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "Plan: do X then Y".into(),
        },
        ModelEvent::ToolCall(ToolCall {
            id: "t1".into(),
            name: "WriteFlag".into(),
            input: json!({}),
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".into()),
            usage: None,
        },
    ]]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(WriteSideEffectTool { flag: flag.clone() }));
    let opts = AgentOptions {
        max_turns: Some(2),
        allowed_tools: vec!["WriteFlag".into()],
        permission_mode: PermissionMode::Plan,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let events = run(provider, opts, tools, "plan").await;
    let r = final_result(&events);
    assert_eq!(r.subtype, ResultSubtype::Success);
    assert!(!*flag.lock().unwrap(), "Plan mode must not run tools");
}

#[tokio::test]
// Search mode sends only loaded tools first, then includes SearchTools matches on the next turn.
async fn search_tools_loads_deferred_tool_for_next_turn() {
    use code_agent_core::ToolCall;
    let provider = Arc::new(CapturingProvider::new(vec![
        vec![
            ModelEvent::MessageStart,
            ModelEvent::ToolCall(ToolCall {
                id: "search1".into(),
                name: "SearchTools".into(),
                input: json!({"query": "github issue", "limit": 1, "include_risky": true}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::MessageStart,
            ModelEvent::ToolCall(ToolCall {
                id: "issue1".into(),
                name: "mcp__github__list_issues".into(),
                input: json!({}),
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
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NamedTool {
        name: "Read",
        description: "Read files",
        kind: PermissionKind::ReadOnly,
    }));
    tools.register(Arc::new(NamedTool {
        name: "mcp__github__list_issues",
        description: "List GitHub issues for a repository",
        kind: PermissionKind::External,
    }));
    let mut opts = AgentOptions {
        max_turns: Some(4),
        allowed_tools: vec!["Read".into(), "SearchTools".into(), "mcp__github__*".into()],
        allowed_tool_rules: vec![ToolRule::try_tool_glob("mcp__github__*").unwrap()],
        ..AgentOptions::default()
    };
    opts.tool_search.mode = ToolLoadingMode::Search;

    let events = run(provider.clone(), opts, tools, "find issue").await;
    let requests = provider.request_tool_names();

    assert!(requests[0].contains(&"Read".to_string()));
    assert!(requests[0].contains(&"SearchTools".to_string()));
    assert!(!requests[0].contains(&"mcp__github__list_issues".to_string()));
    assert!(requests[1].contains(&"mcp__github__list_issues".to_string()));
    assert!(events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolSearchCompleted(_))));
    assert!(events.iter().any(|event| {
        matches!(event, AgentEvent::ToolLoaded(loaded) if loaded.tool_name == "mcp__github__list_issues")
    }));
    assert_eq!(final_result(&events).subtype, ResultSubtype::Success);
}

#[tokio::test]
// Pure eager mode does not inject SearchTools when it is not configured.
async fn eager_mode_does_not_register_search_tools() {
    let provider = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::MessageStart,
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(NamedTool {
        name: "Read",
        description: "Read files",
        kind: PermissionKind::ReadOnly,
    }));
    let mut opts = AgentOptions {
        max_turns: Some(2),
        allowed_tools: vec!["Read".into()],
        ..AgentOptions::default()
    };
    opts.tool_search.mode = ToolLoadingMode::Eager;

    let events = run(provider.clone(), opts, tools, "hi").await;
    let requests = provider.request_tool_names();

    assert_eq!(requests[0], vec!["Read".to_string()]);
    assert!(!events
        .iter()
        .any(|event| matches!(event, AgentEvent::ToolSearchCompleted(_))));
}

/// A tool that blocks on a shared [`tokio::sync::Barrier`] before returning.
/// When `N` of these run *concurrently* every invocation reaches the barrier
/// and all are released together; if they run *sequentially* the first one
/// waits for peers that never arrive and the agent loop hangs (caught by the
/// test's outer timeout). This proves parallelism deterministically without
/// measuring wall-clock time.
struct BarrierTool {
    name: &'static str,
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait]
impl Tool for BarrierTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "rendezvous on a barrier then echo"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: code_agent_core::ToolContext,
    ) -> Result<code_agent_core::ToolResult, ToolError> {
        self.barrier.wait().await;
        Ok(code_agent_core::ToolResult::ok_text("done"))
    }
}

#[tokio::test]
// Multiple tool_use blocks in one model turn run concurrently: two tools that
// rendezvous on a shared barrier only complete if they run at the same time.
// If the loop ran them sequentially the first would block on a barrier its
// peer never reaches, and the run would hang (caught by the outer timeout).
// Both `tool_result` blocks must appear paired to the original `tool_use_id`.
async fn multiple_tool_calls_run_in_parallel() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t1".into(),
                name: "Slow1".into(),
                input: json!({}),
            }),
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t2".into(),
                name: "Slow2".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta { text: "ok".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    // A barrier sized for the two concurrent tool calls. Both must reach it
    // before either returns, so the run only completes under true parallelism.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(BarrierTool {
        name: "Slow1",
        barrier: barrier.clone(),
    }));
    tools.register(Arc::new(BarrierTool {
        name: "Slow2",
        barrier: barrier.clone(),
    }));
    let opts = AgentOptions {
        max_turns: Some(4),
        allowed_tools: vec!["Slow1".into(), "Slow2".into()],
        ..AgentOptions::default()
    };

    // A sequential regression would deadlock on the barrier; bound the whole
    // run so that manifests as a clear failure rather than a hang.
    let events = tokio::time::timeout(Duration::from_secs(10), run(provider, opts, tools, "go"))
        .await
        .expect("tool calls must run in parallel: run did not finish (barrier deadlock)");

    let user_results: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserToolResult(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(
        user_results.len(),
        2,
        "both calls must produce a tool_result"
    );
    // tool_result blocks must come back paired with the original ids.
    let ids: Vec<_> = user_results
        .iter()
        .map(|r| r.tool_use_id.as_str())
        .collect();
    assert!(ids.contains(&"t1"));
    assert!(ids.contains(&"t2"));

    assert_eq!(final_result(&events).subtype, ResultSubtype::Success);
}

/// A `Mutating` tool that records `(name, "start"|"end")` events in a shared
/// log around a cooperative yield point. The yield gives the runtime an
/// opportunity to interleave a concurrently-running peer between this tool's
/// `start` and `end` markers, so if the loop ever (wrongly) parallelized
/// mutating tools the recorded trace would show overlapping start/start
/// events. No wall-clock sleep is needed to detect the race.
struct TracingMutatingTool {
    name: &'static str,
    log: Arc<Mutex<Vec<(String, &'static str)>>>,
}

#[async_trait]
impl Tool for TracingMutatingTool {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "mutating tool that logs start/end"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::Mutating
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push((self.name.to_string(), "start"));
        // Yield several times: if a peer were running concurrently the
        // scheduler would run it here, interleaving its `start` between our
        // `start` and `end`. Under correct serialization nothing else runs.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        self.log
            .lock()
            .unwrap()
            .push((self.name.to_string(), "end"));
        Ok(ToolResult::ok_text("done"))
    }
}

#[tokio::test]
// Mutating tool calls in a single turn must serialize (not race): with two
// 300ms `Mutating` tools the trace must show tool 1 fully complete before
// tool 2 starts. Parallelizing them would interleave start/end and risk
// races on overlapping file state or process state.
async fn mutating_tool_calls_run_sequentially_within_turn() {
    let log = Arc::new(Mutex::new(Vec::<(String, &'static str)>::new()));
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t1".into(),
                name: "Mut1".into(),
                input: json!({}),
            }),
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t2".into(),
                name: "Mut2".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta { text: "ok".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(TracingMutatingTool {
        name: "Mut1",
        log: log.clone(),
    }));
    tools.register(Arc::new(TracingMutatingTool {
        name: "Mut2",
        log: log.clone(),
    }));
    let opts = AgentOptions {
        max_turns: Some(4),
        allowed_tools: vec!["Mut1".into(), "Mut2".into()],
        permission_mode: PermissionMode::AcceptEdits,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };

    let _ = run(provider, opts, tools, "go").await;

    let trace = log.lock().unwrap().clone();
    assert_eq!(
        trace.len(),
        4,
        "expected start/end for each mutating tool: {trace:?}"
    );
    assert_eq!(
        trace[0].1, "start",
        "first event must be a start: {trace:?}"
    );
    assert_eq!(
        trace[1].1, "end",
        "mutating tools must not overlap; expected end before next start: {trace:?}"
    );
    assert_eq!(
        trace[0].0, trace[1].0,
        "the second event must end the same tool that started: {trace:?}"
    );
    assert_ne!(
        trace[0].0, trace[2].0,
        "the second mutating tool must be different from the first: {trace:?}"
    );
}

/// Permission callback that denies any tool call whose tool name matches the
/// configured target. Used to force a specific call to short-circuit while
/// other calls in the same turn run normally.
struct DenyByName {
    deny_tool: &'static str,
}

#[async_trait]
impl PermissionCallback for DenyByName {
    async fn approve(&self, req: PermissionRequest) -> PermissionDecision {
        if req.tool_name == self.deny_tool {
            PermissionDecision::Deny {
                reason: format!("denied {}", self.deny_tool),
            }
        } else {
            PermissionDecision::AllowOnce
        }
    }
}

#[tokio::test]
// When a turn contains a mix of allowed and denied tool calls, the
// `UserToolResult` events (i.e. the `tool_result` blocks sent back to the
// model) must appear in input order. A regression where short-circuited
// denials emit their result immediately while runnable calls emit later
// reverses the order for `[allowed, denied]` turns, which violates the
// Anthropic API's input-order invariant for tool_result blocks.
async fn tool_results_preserve_input_order_when_some_calls_short_circuit() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t0".into(),
                name: "AllowedExec".into(),
                input: json!({}),
            }),
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "t1".into(),
                name: "DeniedExec".into(),
                input: json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta { text: "ok".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let allowed_flag = Arc::new(Mutex::new(false));
    let denied_flag = Arc::new(Mutex::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(ExecToolNamed {
        name: "AllowedExec",
        flag: allowed_flag.clone(),
    }));
    tools.register(Arc::new(ExecToolNamed {
        name: "DeniedExec",
        flag: denied_flag.clone(),
    }));
    let opts = AgentOptions {
        max_turns: Some(4),
        allowed_tools: vec!["AllowedExec".into(), "DeniedExec".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(DenyByName {
            deny_tool: "DeniedExec",
        })),
        ..AgentOptions::default()
    };

    let events = run(provider, opts, tools, "go").await;
    let order: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::UserToolResult(r) => Some(r.tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        order,
        vec!["t0", "t1"],
        "tool_result blocks must be emitted in input order; got {order:?}"
    );
    assert!(*allowed_flag.lock().unwrap(), "allowed tool should run");
    assert!(!*denied_flag.lock().unwrap(), "denied tool must not run");
}

struct ExecToolNamed {
    name: &'static str,
    flag: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for ExecToolNamed {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
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
        *self.flag.lock().unwrap() = true;
        Ok(ToolResult::ok_text("ran"))
    }
}
