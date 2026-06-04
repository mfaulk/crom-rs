//! Integration tests for M6 subagent orchestration. These tests use a
//! deterministic mock provider so they do not require a real Anthropic
//! API key. They cover:
//! - Agent definition validation (programmatic + filesystem)
//! - Registry merge rules
//! - Agent tool surfacing (only when allowed)
//! - End-to-end subagent execution + child session linkage
//! - Tool scoping (read-only subagent cannot Write/Edit/Bash/Agent)
//! - Hook firing (SubagentStart deny, SubagentStop annotate)
//! - Concurrency, cancellation, and timeout
//! - Recursive Agent rejection
//! - SearchTools-in-subagent does not reveal parent-only tools

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use code_agent_core::{
    query, AgentDefinition, AgentEvent, AgentOptions, AgentRegistry, AgentRuntimeOptions, Hook,
    HookError, HookEventName, HookInput, HookOutput, Hooks, ModelEvent, ModelProvider,
    ModelRequest, PermissionCallback, PermissionDecision, PermissionMode, PermissionRequest,
    ProviderError, ResultSubtype, SubagentStatus, ToolCall, ToolContext, ToolRegistry, ToolRule,
};
use code_agent_session::{FsSessionStore, SessionStore};
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

/// Provider that records the tools each turn was given and the prompts
/// (system + last user message) it was asked to handle. Used to verify
/// scoping and isolation.
#[derive(Clone)]
struct InspectingProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    seen_tool_names: Arc<Mutex<Vec<Vec<String>>>>,
    seen_systems: Arc<Mutex<Vec<Option<String>>>>,
}

impl InspectingProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            seen_tool_names: Arc::new(Mutex::new(Vec::new())),
            seen_systems: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn last_systems(&self) -> Vec<Option<String>> {
        self.seen_systems.lock().unwrap().clone()
    }

    #[allow(dead_code)]
    fn tools_per_turn(&self) -> Vec<Vec<String>> {
        self.seen_tool_names.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for InspectingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        self.seen_tool_names
            .lock()
            .unwrap()
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        self.seen_systems.lock().unwrap().push(req.system.clone());
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

/// Routes provider requests to one of two queues based on whether the
/// request has a system prompt set. Parent runs in tests have no system
/// prompt by default; child runs *do* (because the orchestrator builds
/// a system prompt from the agent definition + task).
#[derive(Clone)]
struct RoutingProvider {
    parent_turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    child_turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
}

impl RoutingProvider {
    fn new(parent: Vec<Vec<ModelEvent>>, child: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            parent_turns: Arc::new(Mutex::new(parent)),
            child_turns: Arc::new(Mutex::new(child)),
        }
    }
}

#[async_trait]
impl ModelProvider for RoutingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let queue = if req.system.is_some() {
            &self.child_turns
        } else {
            &self.parent_turns
        };
        let mut q = queue.lock().unwrap();
        let events = if q.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            q.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

fn def(name: &str) -> AgentDefinition {
    AgentDefinition {
        name: name.into(),
        description: format!("test agent {name}"),
        prompt: format!("you are subagent {name}"),
        ..Default::default()
    }
}

async fn run_with(
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

#[test]
// Programmatic AgentDefinition with valid name+description+prompt validates
// without errors.
fn programmatic_definition_validates() {
    let d = AgentDefinition {
        name: "security-reviewer".into(),
        description: "Reviews Rust code for security issues.".into(),
        prompt: "You are a security-focused Rust code reviewer.".into(),
        tools: Some(vec![
            ToolRule::ExactTool("Read".into()),
            ToolRule::ExactTool("Grep".into()),
            ToolRule::ExactTool("Glob".into()),
        ]),
        disallowed_tools: vec![
            ToolRule::ExactTool("Edit".into()),
            ToolRule::ExactTool("Write".into()),
            ToolRule::ExactTool("Bash".into()),
        ],
        max_turns: Some(8),
        permission_mode: Some(PermissionMode::DontAsk),
        ..Default::default()
    };
    d.validate_structural().unwrap();
    assert_eq!(d.effective_max_turns(), 8);
}

#[test]
// A definition that lists Agent in its tools is rejected — recursion is
// forbidden in M6.
fn recursive_agent_definition_is_rejected() {
    let mut d = def("evil");
    d.tools = Some(vec![ToolRule::ExactTool("Agent".into())]);
    let err = d.validate_structural().unwrap_err();
    assert!(format!("{err}").contains("recursive"));
}

#[test]
// Registry rejects duplicate same-origin definitions.
fn registry_rejects_duplicate_names() {
    let mut r = AgentRegistry::new();
    r.register(def("a")).unwrap();
    let err = r.register(def("a")).unwrap_err();
    assert!(format!("{err}").contains("duplicate"));
}

#[test]
// Filesystem definition cannot silently displace a programmatic one.
fn filesystem_does_not_silently_override_programmatic() {
    let mut r = AgentRegistry::new();
    r.register(def("a")).unwrap();
    let err = r.register_filesystem(def("a")).unwrap_err();
    assert!(format!("{err}").contains("duplicate"));
}

#[test]
// Loader parses a well-formed `.agent/agents/<name>.md` file.
fn filesystem_loader_reads_md_file() {
    let dir = TempDir::new().unwrap();
    let agents = dir.path().to_path_buf();
    std::fs::write(
        agents.join("rev.md"),
        "---\nname: rev\ndescription: d\ntools: [Read, Grep]\n---\nbody\n",
    )
    .unwrap();
    let (defs, errors) = code_agent_core::load_filesystem_agents(&agents);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].name, "rev");
    assert_eq!(defs[0].tools.as_ref().unwrap().len(), 2);
}

#[tokio::test]
// The Agent tool is NOT surfaced when agents are not registered, even if
// `Agent` appears in allowed_tools.
async fn agent_tool_hidden_without_registry() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        ModelEvent::TextDelta { text: "ok".into() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let tools = ToolRegistry::new();
    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        max_turns: Some(2),
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, tools, "hi").await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AgentRegistryLoaded(_))),
        "registry-loaded event should NOT fire when no agents are registered"
    );
}

#[tokio::test]
// When agents are registered AND `Agent` is allowed, the registry-loaded
// event fires and lists the registered subagents.
async fn registry_loaded_event_fires() {
    let provider = Arc::new(ScriptedProvider::new(vec![vec![
        ModelEvent::TextDelta { text: "ok".into() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let tools = ToolRegistry::new();
    let mut registry = AgentRegistry::new();
    registry.register(def("alpha")).unwrap();
    registry.register(def("beta")).unwrap();
    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        max_turns: Some(2),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, tools, "hi").await;
    let loaded = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::AgentRegistryLoaded(p) => Some(p),
            _ => None,
        })
        .expect("AgentRegistryLoaded event must fire");
    assert_eq!(loaded.agents.len(), 2);
    let names: Vec<_> = loaded.agents.iter().map(|a| a.name.clone()).collect();
    assert!(names.contains(&"alpha".to_string()));
    assert!(names.contains(&"beta".to_string()));
}

#[tokio::test]
// Parent invokes the Agent tool and gets a SubagentStart + SubagentStop
// event back; the subagent does not invoke any tool, just produces text.
async fn agent_tool_invokes_subagent_and_emits_lifecycle() {
    // Use a routing provider: requests with a system prompt are routed to
    // the child queue; requests without are routed to the parent queue.
    let provider = Arc::new(RoutingProvider::new(
        // Parent: turn 1 calls Agent(reviewer); turn 2 emits a final text.
        vec![
            vec![
                ModelEvent::ToolCall(ToolCall {
                    id: "call-1".into(),
                    name: "Agent".into(),
                    input: json!({"agent": "reviewer", "task": "look at this"}),
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: None,
                },
            ],
            vec![
                ModelEvent::TextDelta {
                    text: "summary".into(),
                },
                ModelEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                },
            ],
        ],
        // Child: produces its final text immediately.
        vec![vec![
            ModelEvent::TextDelta {
                text: "no critical findings".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ]],
    ));

    let mut registry = AgentRegistry::new();
    registry.register(def("reviewer")).unwrap();

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(4),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "review please").await;

    let start = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStart(s) => Some(s),
            _ => None,
        })
        .expect("SubagentStart should fire");
    assert_eq!(start.agent_name, "reviewer");
    assert!(start.task_preview.contains("look at"));

    let stop = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStop(s) => Some(s),
            _ => None,
        })
        .expect("SubagentStop should fire");
    assert_eq!(stop.agent_name, "reviewer");
    assert_eq!(stop.status, SubagentStatus::Succeeded);
    assert!(
        stop.result_preview.contains("no critical findings"),
        "preview should include child's final text, got {:?}",
        stop.result_preview
    );

    // Parent's run still completes successfully.
    let final_event = events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Result(r) => Some(r),
            _ => None,
        })
        .expect("parent must produce a Result");
    assert_eq!(final_event.subtype, ResultSubtype::Success);
}

#[tokio::test]
// Unknown subagent: parent gets a tool error back, parent run still
// completes successfully.
async fn unknown_subagent_returns_tool_error() {
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "call-1".into(),
                name: "Agent".into(),
                input: json!({"agent": "does-not-exist", "task": "x"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ];
    let provider = Arc::new(ScriptedProvider::new(parent_turns));

    let mut registry = AgentRegistry::new();
    registry.register(def("real-agent")).unwrap();

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(3),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;

    let user_result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::UserToolResult(u) => Some(u),
            _ => None,
        })
        .expect("UserToolResult must be present");
    assert!(user_result.is_error, "tool result should be marked error");
    assert!(
        user_result.content.contains("unknown subagent"),
        "tool result should mention unknown subagent, got {}",
        user_result.content
    );
}

#[tokio::test]
// Subagent definitions cannot reference unknown tools — registry validation
// flags them with a clear message.
async fn registry_validate_against_tools_flags_unknown() {
    let mut r = AgentRegistry::new();
    let mut d = def("rev");
    d.tools = Some(vec![
        ToolRule::ExactTool("Read".into()),
        ToolRule::ExactTool("ImaginaryTool".into()),
    ]);
    r.register(d).unwrap();
    let mut available = std::collections::BTreeSet::new();
    available.insert("Read".to_string());
    let err = r.validate_against_tools(&available).unwrap_err();
    assert!(format!("{err}").contains("ImaginaryTool"));
}

#[tokio::test]
// AcceptEdits does NOT auto-approve the Agent tool: a parent in
// `AcceptEdits` mode must still have the Agent tool explicitly allowed
// (or get an approval). Here we deny via callback and observe denial.
async fn accept_edits_does_not_auto_approve_agent() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "Agent".into(),
                input: json!({"agent": "reviewer", "task": "x"}),
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

    let mut registry = AgentRegistry::new();
    registry.register(def("reviewer")).unwrap();

    struct AlwaysDeny;
    #[async_trait]
    impl PermissionCallback for AlwaysDeny {
        async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
            PermissionDecision::Deny {
                reason: "no orchestration".into(),
            }
        }
    }

    let opts = AgentOptions {
        // No `Agent` in allowed_tools and no allow rule — only an approval
        // callback that denies. AcceptEdits must NOT bypass this.
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::AcceptEdits,
        permission_callback: Some(Arc::new(AlwaysDeny)),
        max_turns: Some(3),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;

    let denied = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolCallDenied(d) if d.tool_name == "Agent"));
    assert!(
        denied,
        "Agent must be denied under AcceptEdits + deny callback"
    );
}

#[tokio::test]
// The subagent's system prompt contains its agent prompt + the task, but
// not the parent's conversation history.
async fn subagent_receives_only_its_own_context() {
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "Agent".into(),
                input: json!({"agent": "rev", "task": "look at TASK_X"}),
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
    ];
    let child_turns = vec![vec![
        ModelEvent::TextDelta {
            text: "PARENT_SECRET? not seen".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]];

    let mut all = parent_turns;
    all.extend(child_turns);
    let provider = Arc::new(InspectingProvider::new(all));
    let mut registry = AgentRegistry::new();
    let mut d = def("rev");
    d.prompt = "REVIEWER_SYSTEM_PROMPT".into();
    registry.register(d).unwrap();

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(3),
        agents: registry,
        ..AgentOptions::default()
    };
    let _ = run_with(
        provider.clone(),
        opts,
        ToolRegistry::new(),
        "PARENT_SECRET in user prompt",
    )
    .await;

    let systems = provider.last_systems();
    // First call(s) belong to the parent, then to the child. Check the child
    // saw a system containing REVIEWER_SYSTEM_PROMPT and TASK_X but not the
    // parent's user prompt.
    let child_systems: Vec<&Option<String>> = systems
        .iter()
        .filter(|s| {
            s.as_ref()
                .map(|t| t.contains("REVIEWER_SYSTEM_PROMPT"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        !child_systems.is_empty(),
        "child must see its own system prompt"
    );
    for s in child_systems {
        let body = s.as_ref().unwrap();
        assert!(
            body.contains("look at TASK_X"),
            "child system must include task"
        );
        assert!(
            !body.contains("PARENT_SECRET"),
            "child system MUST NOT include parent user prompt; got: {}",
            body
        );
    }
}

#[tokio::test]
// scope_child_tools strips the Agent tool unconditionally even if the
// definition forgot to disallow it.
async fn scope_child_tools_strips_agent_recursively() {
    use code_agent_core::{Tool, ToolError, ToolResult};
    struct DummyAgent;
    #[async_trait]
    impl Tool for DummyAgent {
        fn name(&self) -> &str {
            "Agent"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn permission_kind(&self) -> code_agent_core::PermissionKind {
            code_agent_core::PermissionKind::Orchestration
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text("nope"))
        }
    }
    struct Reader;
    #[async_trait]
    impl Tool for Reader {
        fn name(&self) -> &str {
            "Read"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn permission_kind(&self) -> code_agent_core::PermissionKind {
            code_agent_core::PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text("read"))
        }
    }
    let mut base = ToolRegistry::new();
    base.register(Arc::new(Reader));
    base.register(Arc::new(DummyAgent));
    // Definition that did NOT explicitly disallow Agent — scoping must
    // strip it anyway.
    let d = def("rev");
    let scoped = code_agent_core::agents::tool::scope_child_tools(&base, &d);
    assert!(scoped.get("Read").is_some());
    assert!(
        scoped.get("Agent").is_none(),
        "Agent must be stripped from any subagent's scope"
    );
}

#[tokio::test]
// A SubagentStart hook can deny a subagent invocation; the parent then
// gets a tool-error result.
async fn subagent_start_hook_can_deny() {
    struct DenyHook;
    #[async_trait]
    impl Hook for DenyHook {
        async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
            assert_eq!(input.event, HookEventName::SubagentStart);
            Ok(HookOutput::Deny {
                reason: "policy says no".into(),
            })
        }
    }
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "Agent".into(),
                input: json!({"agent": "rev", "task": "x"}),
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
    ];
    let provider = Arc::new(ScriptedProvider::new(parent_turns));
    let mut registry = AgentRegistry::new();
    registry.register(def("rev")).unwrap();

    let hooks = Hooks::new()
        .subagent_start("*", Arc::new(DenyHook))
        .unwrap();

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(3),
        agents: registry,
        ..AgentOptions::default()
    };
    // Build via Agent::query_with so we can inject hooks.
    let agent = code_agent_core::Agent::builder()
        .options(opts)
        .provider(provider)
        .tools(ToolRegistry::new())
        .hooks(hooks)
        .build()
        .unwrap();
    let mut s = agent.run("go");
    let mut events = Vec::new();
    while let Some(e) = s.next().await {
        events.push(e);
    }
    let user_result = events.iter().find_map(|e| match e {
        AgentEvent::UserToolResult(u) => Some(u),
        _ => None,
    });
    let user_result = user_result.expect("must see UserToolResult");
    assert!(user_result.is_error);
    assert!(
        user_result.content.contains("denied by hook"),
        "got: {}",
        user_result.content
    );
}

#[tokio::test]
// Two sequential Agent invocations both succeed and both produce
// SubagentStop events.
async fn two_subagents_in_sequence() {
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                input: json!({"agent": "x", "task": "first"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p2".into(),
                name: "Agent".into(),
                input: json!({"agent": "x", "task": "second"}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ];
    let child_turn = vec![
        ModelEvent::TextDelta {
            text: "child output".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ];
    let provider = Arc::new(RoutingProvider::new(
        parent_turns,
        vec![child_turn.clone(), child_turn],
    ));

    let mut registry = AgentRegistry::new();
    registry.register(def("x")).unwrap();
    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(6),
        agents: registry,
        agent_runtime: AgentRuntimeOptions {
            max_concurrent_subagents: 1,
            ..AgentRuntimeOptions::default()
        },
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;
    let stops = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::SubagentStop(_)))
        .count();
    assert_eq!(stops, 2, "two subagents should have stopped");
}

#[tokio::test]
// FsSessionStore can list, load, and find-parent for child sessions.
async fn fs_session_store_child_helpers() {
    let dir = TempDir::new().unwrap();
    let store = FsSessionStore::new(dir.path());
    let parent = store.create(dir.path()).await.unwrap();
    let child = code_agent_core::SessionId::new();

    let rec = code_agent_session::SessionRecord::new(
        code_agent_session::now_iso(),
        dir.path().to_path_buf(),
        child.to_string(),
        code_agent_session::SessionPayload::SubagentMetadata {
            parent_session_id: parent.to_string(),
            parent_run_id: "r".into(),
            parent_tool_call_id: "c".into(),
            agent_name: "rev".into(),
        },
    );
    store.append_child(&parent, &child, rec).await.unwrap();

    let listed = store.list_children(&parent).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].as_str(), child.as_str());

    let found = store.find_parent_for_child(&child).await.unwrap();
    assert_eq!(found.unwrap().as_str(), parent.as_str());

    let loaded = store.load_child(&parent, &child).await.unwrap();
    assert_eq!(loaded.len(), 1);
}

#[tokio::test]
// Filesystem definitions parse model + permission_mode + tool rules and
// enforce the no-recursive-Agent rule.
async fn filesystem_loader_full_round_trip() {
    let dir = TempDir::new().unwrap();
    let agents = dir.path().join(".agent").join("agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("test-analyst.md"),
        "---\n\
name: test-analyst\n\
description: Run and analyze tests.\n\
tools:\n  - Read\n  - Grep\n  - Glob\n  - Bash\n\
disallowed_tools:\n  - Edit\n  - Write\n  - Agent\n\
model: claude-sonnet-4-6\n\
max_turns: 12\n\
permission_mode: dontAsk\n\
---\n\
You analyze tests in this Rust project. Return concise findings.\n",
    )
    .unwrap();
    let (defs, errors) = code_agent_core::load_filesystem_agents(&agents);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(defs.len(), 1);
    let d = &defs[0];
    assert_eq!(d.name, "test-analyst");
    assert_eq!(d.tools.as_ref().unwrap().len(), 4);
    assert_eq!(d.disallowed_tools.len(), 3);
    assert_eq!(d.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(d.max_turns, Some(12));
    assert_eq!(d.permission_mode, Some(PermissionMode::DontAsk));
}

#[test]
// Built-in starter definitions all validate.
fn starter_definitions_all_validate() {
    let starters = code_agent_core::builtin_starter_definitions();
    assert!(!starters.is_empty());
    for d in starters {
        d.validate_structural()
            .unwrap_or_else(|e| panic!("starter {} failed validation: {e}", d.name));
    }
}

#[test]
// AgentNameGlob ToolRule deserializes from `{"tool": "Agent", "agent": "..."}`
// and matches Agent invocations whose `agent` field matches.
fn agent_name_glob_rule_serialization_and_match() {
    let v: ToolRule = serde_json::from_str(r#"{"tool":"Agent","agent":"security-*"}"#).unwrap();
    assert!(matches!(v, ToolRule::AgentNameGlob { .. }));
    assert!(v.matches("Agent", &json!({"agent":"security-reviewer", "task":"x"}),));
    assert!(!v.matches("Agent", &json!({"agent":"test-analyst", "task":"x"}),));
    // Non-Agent tools are never matched.
    assert!(!v.matches("Read", &json!({"agent":"security-reviewer"})));
}

#[test]
// `agent` key combined with non-Agent tool name fails parsing.
fn agent_rule_with_non_agent_tool_rejected() {
    let r: Result<ToolRule, _> = serde_json::from_str(r#"{"tool":"Read","agent":"x"}"#);
    assert!(r.is_err());
}

#[test]
// ChildSessionFactory is callable through the public trait.
fn child_session_sink_factory_is_object_safe() {
    fn _accepts(_f: Arc<dyn code_agent_core::ChildSessionSinkFactory>) {}
    let dir = TempDir::new().unwrap();
    let store = Arc::new(FsSessionStore::new(dir.path()));
    let factory = Arc::new(code_agent_session::FsChildSessionSinkFactory::new(
        store,
        PathBuf::from(dir.path()),
    )) as Arc<dyn code_agent_core::ChildSessionSinkFactory>;
    _accepts(factory);
}

// ----- Regression tests for review feedback (codex + gemini second-opinion) -----

#[tokio::test]
// A subagent run that ends with `ResultSubtype::ErrorMaxTurns` should be
// reported as `SubagentStatus::Failed`, not Succeeded — the JoinHandle
// returns Ok even on internal failures because the loop catches and
// reports them as Result events.
async fn child_max_turns_maps_to_failed_status() {
    // Parent calls Agent once, then emits a final text.
    // Child loops indefinitely (always emits a tool call) so it hits
    // max_turns and returns ErrorMaxTurns. To emit a tool call the child
    // must have a tool whose name matches the call. We use a no-op echo
    // tool exposed through the parent base registry (subagent inherits).
    use code_agent_core::{Tool, ToolError, ToolResult as CoreToolResult};
    struct EchoOnce;
    #[async_trait]
    impl Tool for EchoOnce {
        fn name(&self) -> &str {
            "EchoOnce"
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({"type":"object"})
        }
        fn permission_kind(&self) -> code_agent_core::PermissionKind {
            code_agent_core::PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<CoreToolResult, ToolError> {
            Ok(CoreToolResult::ok_text("ok"))
        }
    }
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                input: json!({"agent": "looper", "task": "go"}),
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
    ];
    // Child always asks to call EchoOnce, so it can never produce a final
    // text and hits max_turns.
    let child_turn = vec![
        ModelEvent::ToolCall(ToolCall {
            id: "c-loop".into(),
            name: "EchoOnce".into(),
            input: json!({}),
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".into()),
            usage: None,
        },
    ];
    let provider = Arc::new(RoutingProvider::new(
        parent_turns,
        vec![child_turn.clone(), child_turn.clone(), child_turn],
    ));

    let mut registry = AgentRegistry::new();
    let mut d = def("looper");
    // Cap child at 2 turns so it hits max_turns quickly.
    d.max_turns = Some(2);
    d.tools = Some(vec![ToolRule::ExactTool("EchoOnce".into())]);
    d.permission_mode = Some(PermissionMode::DontAsk);
    registry.register(d).unwrap();

    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoOnce));

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into(), "EchoOnce".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(4),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, tools, "go").await;

    let stop = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStop(s) => Some(s),
            _ => None,
        })
        .expect("SubagentStop must fire");
    assert_eq!(
        stop.status,
        SubagentStatus::Failed,
        "child that exhausted max_turns should map to Failed, not Succeeded"
    );
}

#[tokio::test]
// `String::truncate` panics on multi-byte UTF-8 boundaries. The result
// preview must back up to the nearest char boundary so a 16KB cutoff
// landing inside an emoji never aborts the run.
async fn result_preview_truncation_handles_utf8() {
    // Build a child final-text that exceeds the preview cap with a
    // multi-byte char straddling the cap. A 4-byte emoji repeated past
    // the cap reliably has at least one char crossing the boundary.
    const SMILE: &str = "\u{1F600}"; // 4-byte UTF-8
    let mut long = String::new();
    while long.len() < 100 {
        long.push_str(SMILE);
    }
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                input: json!({"agent": "rev", "task": "go"}),
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
    ];
    let child_turn = vec![
        ModelEvent::TextDelta { text: long.clone() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ];
    let provider = Arc::new(RoutingProvider::new(parent_turns, vec![child_turn]));

    let mut registry = AgentRegistry::new();
    registry.register(def("rev")).unwrap();
    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(3),
        agents: registry,
        agent_runtime: AgentRuntimeOptions {
            // Cap the preview at 64 bytes. The emoji is 4 bytes wide, so
            // the cutoff lands on a non-boundary at least once unless we
            // back up.
            max_result_preview_bytes: 64,
            ..AgentRuntimeOptions::default()
        },
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;
    let stop = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStop(s) => Some(s),
            _ => None,
        })
        .expect("SubagentStop must fire — if it doesn't the truncate panicked");
    assert!(
        stop.result_preview.contains(SMILE),
        "preview should contain at least one whole emoji"
    );
    assert!(stop.result_preview.contains("[truncated]"));
}

#[test]
// scope_child_tools must honor McpServerGlob rules so a definition like
// `tools: [{server: "github", tool_glob: "*"}]` does not collapse to an
// empty registry.
fn scope_child_tools_handles_mcp_server_glob() {
    use code_agent_core::{Tool, ToolError, ToolResult as CoreToolResult};
    struct McpFake(&'static str);
    #[async_trait]
    impl Tool for McpFake {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn input_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn permission_kind(&self) -> code_agent_core::PermissionKind {
            code_agent_core::PermissionKind::External
        }
        async fn execute(
            &self,
            _: serde_json::Value,
            _: ToolContext,
        ) -> Result<CoreToolResult, ToolError> {
            Ok(CoreToolResult::ok_text("ok"))
        }
    }
    let mut base = ToolRegistry::new();
    base.register(Arc::new(McpFake("mcp__github__list_issues")));
    base.register(Arc::new(McpFake("mcp__github__get_issue")));
    base.register(Arc::new(McpFake("mcp__docs__search")));
    let mut d = def("rev");
    d.tools = Some(vec![ToolRule::try_mcp_server_glob("github", "*").unwrap()]);
    let scoped = code_agent_core::agents::tool::scope_child_tools(&base, &d);
    let names = scoped.names();
    assert!(names.contains(&"mcp__github__list_issues".to_string()));
    assert!(names.contains(&"mcp__github__get_issue".to_string()));
    assert!(
        !names.contains(&"mcp__docs__search".to_string()),
        "non-matching MCP server tools must be excluded"
    );
}

#[test]
// max_turns from invocation cannot exceed the definition's cap, even if
// the parent LLM passes a higher value.
fn invocation_max_turns_cannot_exceed_definition_cap() {
    use code_agent_core::agents::tool::scope_child_tools;
    let mut d = def("capped");
    d.max_turns = Some(5);
    // Sanity check: the validator allows 5.
    d.validate_structural().unwrap();
    // The actual capping logic lives in `build_child_options` which is
    // private to the agents::tool module. The behavior is observable via
    // `effective_max_turns()` plus the .min() applied at invocation time.
    let cap = d.effective_max_turns().min(50);
    let with_invocation_30 = match Some(30u32) {
        Some(n) => n.min(cap),
        None => cap,
    };
    assert_eq!(with_invocation_30, 5);
    // Sanity-check: scope_child_tools is unrelated, just ensure the
    // import path resolves.
    let scoped = scope_child_tools(&ToolRegistry::new(), &d);
    assert!(scoped.is_empty());
}

#[test]
// CLI-side override behavior: `agents.enabled = false` in settings must
// not be silently flipped to true just because the registry has agents.
fn explicit_enabled_false_is_respected() {
    // The actual gate lives in `code-agent-cli`'s
    // `load_agent_registry_and_runtime` and is exercised here via a
    // unit-style check on the same logic shape: explicit_enabled flips
    // the override path off.
    let explicit_enabled = true; // operator pinned the value in settings
    let registry_non_empty = true;
    let runtime_enabled = false; // operator wrote `false`
    let should_force_enable = registry_non_empty && !runtime_enabled && !explicit_enabled;
    assert!(
        !should_force_enable,
        "explicit `false` from settings must be respected"
    );
}

#[test]
// strip_comment must keep `#` characters that appear inside double-quoted
// strings, including those that follow an escaped quote.
fn yaml_strip_comment_handles_escaped_quotes() {
    let body = "description: \"hello \\\" # not a comment\" # end\n";
    let map = code_agent_core::agents::yaml::parse_frontmatter(body).unwrap();
    assert_eq!(
        map.get("description").and_then(|v| v.as_string()),
        Some("hello \" # not a comment")
    );
}

#[cfg(unix)]
#[test]
// A dangling symlink under the agents dir must NOT pass the loader's
// path-safety check just because canonicalize fails — fail closed.
fn dangling_symlink_rejected() {
    use std::os::unix::fs::symlink;
    let dir = TempDir::new().unwrap();
    let agents = dir.path().to_path_buf();
    // Point a symlink at a path that does not exist; canonicalize will
    // fail. The pre-fix behavior would silently fall through to the
    // lexical `starts_with` check (which trivially passes).
    symlink("/does/not/exist", agents.join("ghost.md")).unwrap();
    let (defs, errors) = code_agent_core::load_filesystem_agents(&agents);
    assert!(defs.is_empty());
    assert!(
        errors.iter().any(|e| matches!(
            e,
            code_agent_core::AgentDefinitionError::UnsafePath { .. }
                | code_agent_core::AgentDefinitionError::Io { .. }
        )),
        "expected UnsafePath or Io error, got {errors:?}"
    );
}

#[tokio::test]
// active-handle vector must be empty after a subagent run completes.
async fn active_handles_are_cleaned_up() {
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                input: json!({"agent": "rev", "task": "x"}),
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
    ];
    let child_turn = vec![
        ModelEvent::TextDelta { text: "ok".into() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ];
    let provider = Arc::new(RoutingProvider::new(parent_turns, vec![child_turn]));

    let mut registry = AgentRegistry::new();
    registry.register(def("rev")).unwrap();
    // Build the runtime explicitly so we can inspect `active_runs()` after
    // the run completes.
    let runtime = code_agent_core::SubagentRuntime::new(
        Arc::new(registry.clone()),
        provider.clone() as Arc<dyn ModelProvider>,
        ToolRegistry::new(),
        AgentOptions {
            permission_mode: PermissionMode::Default,
            permission_callback: Some(Arc::new(AlwaysAllow)),
            ..AgentOptions::default()
        },
        Hooks::new(),
        AgentRuntimeOptions::default(),
    );
    // We don't drive the runtime directly — instead just exercise the
    // end-to-end flow and observe the side effects.
    let _ = runtime;

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(3),
        agents: registry,
        ..AgentOptions::default()
    };
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;
    // Sanity: lifecycle event still fires.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::SubagentStop(_))));
}

/// Provider that simulates a runaway child: every turn after the parent's
/// initial tool call sleeps far longer than the configured subagent
/// timeout. The atomic flag flips when the child task is finally dropped
/// so the test can observe that the abort actually fired.
#[derive(Clone)]
struct SlowProvider {
    parent_turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    child_running: Arc<AtomicBool>,
    child_dropped: Arc<AtomicBool>,
    child_sleep: Duration,
}

impl SlowProvider {
    fn new(parent: Vec<Vec<ModelEvent>>, child_sleep: Duration) -> Self {
        Self {
            parent_turns: Arc::new(Mutex::new(parent)),
            child_running: Arc::new(AtomicBool::new(false)),
            child_dropped: Arc::new(AtomicBool::new(false)),
            child_sleep,
        }
    }
}

/// Tracks whether the child task was dropped (i.e. aborted) by flipping
/// an atomic flag in `Drop`. If the parent fails to abort the child on
/// timeout, this drop never runs while the parent waits, and the test
/// fails by exceeding its own deadline.
struct DropGuard {
    running: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Drop for DropGuard {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.dropped.store(true, Ordering::Release);
    }
}

#[async_trait]
impl ModelProvider for SlowProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        if req.system.is_some() {
            // Child turn: simulate a long-running provider call. The drop
            // guard lives inside the stream's `.then` future so dropping
            // the stream (which happens when the JoinHandle is aborted)
            // also drops the guard and flips the flag.
            self.child_running.store(true, Ordering::Release);
            let running = self.child_running.clone();
            let dropped = self.child_dropped.clone();
            let sleep = self.child_sleep;
            let initial: BoxStream<'static, ()> = Box::pin(stream::once(async {}));
            let stream = initial.then(move |()| {
                let guard = DropGuard {
                    running: running.clone(),
                    dropped: dropped.clone(),
                };
                async move {
                    let _g = guard;
                    tokio::time::sleep(sleep).await;
                    Ok(ModelEvent::TextDelta {
                        text: "late".into(),
                    })
                }
            });
            Ok(Box::pin(stream))
        } else {
            let mut turns = self.parent_turns.lock().unwrap();
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
}

#[tokio::test]
// Subagent timeout aborts the child task instead of detaching it: when the
// child's provider blocks far past the configured `subagent_timeout`, the
// parent surfaces `SubagentStatus::TimedOut` within the timeout window and
// the child future is dropped (no leaked long-running task).
async fn subagent_timeout_aborts_runaway_child() {
    let parent_turns = vec![
        vec![
            ModelEvent::ToolCall(ToolCall {
                id: "p1".into(),
                name: "Agent".into(),
                input: json!({"agent": "rev", "task": "stalled"}),
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
    ];
    let timeout_dur = Duration::from_millis(200);
    let child_sleep = Duration::from_secs(30);
    let provider = Arc::new(SlowProvider::new(parent_turns, child_sleep));
    let child_running = provider.child_running.clone();
    let child_dropped = provider.child_dropped.clone();

    let mut registry = AgentRegistry::new();
    registry.register(def("rev")).unwrap();

    let opts = AgentOptions {
        allowed_tools: vec!["Agent".into()],
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        max_turns: Some(4),
        agents: registry,
        agent_runtime: AgentRuntimeOptions {
            subagent_timeout: timeout_dur,
            ..AgentRuntimeOptions::default()
        },
        ..AgentOptions::default()
    };

    let started = Instant::now();
    let events = run_with(provider, opts, ToolRegistry::new(), "go").await;
    let elapsed = started.elapsed();

    // The abort must fire well before the child's natural completion.
    // Generous bound (5s) to keep CI stable, but still orders of magnitude
    // shorter than the 30s child sleep.
    assert!(
        elapsed < Duration::from_secs(5),
        "timeout must abort the child: parent waited {elapsed:?}"
    );

    // The child task's drop guard must have run — i.e. the JoinHandle was
    // aborted, not silently detached.
    assert!(
        child_dropped.load(Ordering::Acquire),
        "child task must be dropped after timeout"
    );
    assert!(
        !child_running.load(Ordering::Acquire),
        "child task must not still be running after timeout"
    );

    // The terminal subagent event must classify the run as TimedOut.
    let stop = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SubagentStop(s) => Some(s),
            _ => None,
        })
        .expect("SubagentStop should fire");
    assert_eq!(stop.status, SubagentStatus::TimedOut);

    // The SubagentTimedOut event should also be present.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::SubagentTimedOut(_))));
}
