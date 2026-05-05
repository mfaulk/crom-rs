use std::sync::{Arc, Mutex};

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
