use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crom_core::events::{EnforcementLayer, SecurityDecision, SecurityOutcome, SecurityPrincipal};
use crom_core::{
    AgentBuilder, AgentError, AgentEvent, AgentOptions, Hook, HookError, HookInput, HookOutput,
    Hooks, ModelEvent, ModelProvider, ModelRequest, PermissionKind, PermissionMode,
    PolicyDecisionSource, PolicyOutcome, ProviderError, SessionId, SessionSink, Tool, ToolCall,
    ToolContext, ToolError, ToolRegistry, ToolResult, ToolRule,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::json;

const SYNTHETIC_ANTHROPIC_KEY: &str = "sk-ant-api03-SYNTHETICabcdefghijklmnopqrstuvwxyz0123456789";
const SYNTHETIC_GITHUB_TOKEN: &str = "ghp_SYNTHETICabcdefghijklmnopqrstuvwxyz01";
const SYNTHETIC_AWS_KEY: &str = "AKIASYNTHETIC0000000";

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

/// Read-only tool that echoes its `text` input back as the result.
struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
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
        input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
        Ok(ToolResult::ok_text(text))
    }
}

/// Mutating tool that records whether it ran.
struct WriteTool {
    ran: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &str {
        "Write"
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
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        *self.ran.lock().unwrap() = true;
        Ok(ToolResult::ok_text("written"))
    }
}

/// MCP-named external tool that records whether it ran.
struct McpWriteTool {
    ran: Arc<Mutex<bool>>,
}

#[async_trait]
impl Tool for McpWriteTool {
    fn name(&self) -> &str {
        "mcp__x__write"
    }
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::External
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        *self.ran.lock().unwrap() = true;
        Ok(ToolResult::ok_text("remote write done"))
    }
}

/// Read-only tool whose in-process path guard always refuses.
struct GuardedReadTool;

#[async_trait]
impl Tool for GuardedReadTool {
    fn name(&self) -> &str {
        "Read"
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
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Err(ToolError::PermissionDenied(
            "path escapes the workspace".into(),
        ))
    }
}

/// Read-only tool that always returns an error result.
struct FailingTool;

#[async_trait]
impl Tool for FailingTool {
    fn name(&self) -> &str {
        "Fail"
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
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::error_text("boom"))
    }
}

struct DenyHook;

#[async_trait]
impl Hook for DenyHook {
    async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
        Ok(HookOutput::Deny {
            reason: "hook says no".into(),
        })
    }
}

struct AllowHook;

#[async_trait]
impl Hook for AllowHook {
    async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
        Ok(HookOutput::Allow {
            updated_input: None,
        })
    }
}

#[derive(Default)]
struct CapturingSink {
    lines: Mutex<Vec<String>>,
}

#[async_trait]
impl SessionSink for CapturingSink {
    async fn record(&self, _sid: &SessionId, event: &AgentEvent) -> Result<(), AgentError> {
        self.lines
            .lock()
            .unwrap()
            .push(serde_json::to_string(event).unwrap());
        Ok(())
    }
}

fn tool_turn(calls: Vec<(&str, &str, serde_json::Value)>) -> Vec<Vec<ModelEvent>> {
    let mut first: Vec<ModelEvent> = calls
        .into_iter()
        .map(|(id, name, input)| {
            ModelEvent::ToolCall(ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            })
        })
        .collect();
    first.push(ModelEvent::MessageStop {
        stop_reason: Some("tool_use".into()),
        usage: None,
    });
    vec![
        first,
        vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]
}

fn registry() -> (ToolRegistry, Arc<Mutex<bool>>) {
    let ran = Arc::new(Mutex::new(false));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    tools.register(Arc::new(WriteTool { ran: ran.clone() }));
    tools.register(Arc::new(GuardedReadTool));
    tools.register(Arc::new(FailingTool));
    tools.register(Arc::new(McpWriteTool { ran: ran.clone() }));
    (tools, ran)
}

fn options() -> AgentOptions {
    AgentOptions {
        max_turns: Some(3),
        allowed_tools: vec![
            "Echo".into(),
            "Write".into(),
            "Read".into(),
            "Fail".into(),
            "mcp__x__write".into(),
        ],
        permission_mode: PermissionMode::AcceptEdits,
        cwd: std::path::PathBuf::from("/work"),
        ..AgentOptions::default()
    }
}

async fn run_with(
    turns: Vec<Vec<ModelEvent>>,
    options: AgentOptions,
    hooks: Hooks,
    sink: Option<Arc<CapturingSink>>,
) -> (Vec<AgentEvent>, Arc<Mutex<bool>>) {
    let (tools, ran) = registry();
    let mut builder = AgentBuilder::new()
        .options(options)
        .provider(Arc::new(ScriptedProvider::new(turns)))
        .tools(tools)
        .hooks(hooks);
    if let Some(s) = sink {
        builder = builder.session_sink(s);
    }
    let mut stream = builder.build().unwrap().run("go");
    let mut events = Vec::new();
    while let Some(e) = stream.next().await {
        events.push(e);
    }
    (events, ran)
}

fn decisions(events: &[AgentEvent]) -> Vec<SecurityDecision> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::SecurityDecision(d) => Some(d.clone()),
            _ => None,
        })
        .collect()
}

fn decision_for<'a>(ds: &'a [SecurityDecision], id: &str) -> &'a SecurityDecision {
    let matching: Vec<_> = ds.iter().filter(|d| d.tool_use_id == id).collect();
    assert_eq!(matching.len(), 1, "expected exactly one decision for {id}");
    matching[0]
}

#[tokio::test]
// A disallow-rule denial emits a blocked SecurityDecision carrying run id, rule, resource and the policy layer.
async fn disallow_rule_denial_emits_blocked_security_decision() {
    let mut opts = options();
    opts.disallowed_tool_rules = vec![ToolRule::ExactTool("Write".into())];
    let (events, ran) = run_with(
        tool_turn(vec![(
            "w1",
            "Write",
            json!({"file_path": "/work/src/../src/a.rs"}),
        )]),
        opts,
        Hooks::new(),
        None,
    )
    .await;
    assert!(!*ran.lock().unwrap());
    let ds = decisions(&events);
    let d = decision_for(&ds, "w1");
    assert_eq!(d.tool_name, "Write");
    assert_eq!(d.decision, PolicyOutcome::Deny);
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.enforcement, EnforcementLayer::Policy);
    assert_eq!(d.policy_source, Some(PolicyDecisionSource::DisallowRule));
    assert_eq!(d.rule.as_deref(), Some("disallowed_tools[0]: \"Write\""));
    assert_eq!(d.action_class, Some(PermissionKind::Mutating));
    assert_eq!(d.resource.as_deref(), Some("src/a.rs"));
    assert_eq!(d.principal, SecurityPrincipal::Session);
    let result_run_id = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::Result(r) => r.run_id.clone(),
            _ => None,
        })
        .expect("result carries run id");
    assert_eq!(d.run_id, result_run_id);
}

#[tokio::test]
// A PreToolUse hook denial is attributed to the hook enforcement layer.
async fn hook_denial_is_attributed_to_hook_layer() {
    let hooks = Hooks::new().pre_tool_use("*", Arc::new(DenyHook)).unwrap();
    let (events, _) = run_with(
        tool_turn(vec![("e1", "Echo", json!({"text": "hi"}))]),
        options(),
        hooks,
        None,
    )
    .await;
    let ds = decisions(&events);
    let d = decision_for(&ds, "e1");
    assert_eq!(d.enforcement, EnforcementLayer::Hook);
    assert_eq!(d.policy_source, Some(PolicyDecisionSource::Hook));
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.reason.as_deref(), Some("hook says no"));
}

fn assert_blocked_by_external_effect_policy(events: &[AgentEvent], id: &str) {
    let ds = decisions(events);
    let d = decision_for(&ds, id);
    assert_eq!(d.decision, PolicyOutcome::Deny);
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.enforcement, EnforcementLayer::Policy);
    assert_eq!(
        d.policy_source,
        Some(PolicyDecisionSource::ExternalEffectPolicy)
    );
}

#[tokio::test]
// A PreToolUse hook Allow does not run an MCP tool lacking an explicit allow rule; the block is attributed to the external-effect policy.
async fn hook_allow_does_not_run_mcp_tool_without_allow_rule() {
    let hooks = Hooks::new().pre_tool_use("*", Arc::new(AllowHook)).unwrap();
    let (events, ran) = run_with(
        tool_turn(vec![("m1", "mcp__x__write", json!({}))]),
        options(),
        hooks,
        None,
    )
    .await;
    assert!(!*ran.lock().unwrap(), "MCP tool must not execute");
    assert_blocked_by_external_effect_policy(&events, "m1");
}

#[tokio::test]
// dontAsk's tool-name allowlist does not run an MCP tool lacking an explicit allow rule.
async fn dont_ask_name_list_does_not_run_mcp_tool_without_allow_rule() {
    let mut opts = options();
    opts.permission_mode = PermissionMode::DontAsk;
    let (events, ran) = run_with(
        tool_turn(vec![("m1", "mcp__x__write", json!({}))]),
        opts,
        Hooks::new(),
        None,
    )
    .await;
    assert!(!*ran.lock().unwrap(), "MCP tool must not execute");
    assert_blocked_by_external_effect_policy(&events, "m1");
}

#[tokio::test]
// A PostToolUse hook Deny replaces the tool result with an error and records the call as blocked by the hook layer.
async fn post_tool_hook_deny_withholds_result_and_records_blocked() {
    let hooks = Hooks::new()
        .post_tool_use("Echo", Arc::new(DenyHook))
        .unwrap();
    let (events, _) = run_with(
        tool_turn(vec![("e1", "Echo", json!({"text": "PAYLOAD-XYZ"}))]),
        options(),
        hooks,
        None,
    )
    .await;
    let result = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::UserToolResult(r) if r.tool_use_id == "e1" => Some(r.clone()),
            _ => None,
        })
        .expect("tool result for e1");
    assert!(result.is_error);
    assert!(!result.content.contains("PAYLOAD-XYZ"));
    assert!(result.content.contains("hook says no"));
    let ds = decisions(&events);
    let d = decision_for(&ds, "e1");
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.decision, PolicyOutcome::Deny);
    assert_eq!(d.enforcement, EnforcementLayer::Hook);
    assert_eq!(d.policy_source, Some(PolicyDecisionSource::Hook));
    assert_eq!(d.reason.as_deref(), Some("hook says no"));
}

#[tokio::test]
// A call to a tool outside the allowlist emits a blocked decision with no policy source.
async fn unregistered_tool_emits_blocked_decision() {
    let (events, _) = run_with(
        tool_turn(vec![("x1", "Nope", json!({}))]),
        options(),
        Hooks::new(),
        None,
    )
    .await;
    let ds = decisions(&events);
    let d = decision_for(&ds, "x1");
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.enforcement, EnforcementLayer::Policy);
    assert_eq!(d.policy_source, None);
}

#[tokio::test]
// A tool-level PermissionDenied is recorded as a sandbox-layer block.
async fn tool_permission_denied_is_recorded_as_sandbox_block() {
    let (events, _) = run_with(
        tool_turn(vec![("r1", "Read", json!({"file_path": "/etc/passwd"}))]),
        options(),
        Hooks::new(),
        None,
    )
    .await;
    let ds = decisions(&events);
    let d = decision_for(&ds, "r1");
    assert_eq!(d.enforcement, EnforcementLayer::Sandbox);
    assert_eq!(d.outcome, SecurityOutcome::Blocked);
    assert_eq!(d.decision, PolicyOutcome::Deny);
    assert_eq!(d.resource.as_deref(), Some("/etc/passwd"));
}

#[tokio::test]
// Allowed calls emit one executed/failed decision each, so every started tool call has an audit record.
async fn every_tool_call_has_exactly_one_decision_record() {
    let mut opts = options();
    opts.disallowed_tool_rules = vec![ToolRule::ExactTool("Write".into())];
    let (events, _) = run_with(
        tool_turn(vec![
            ("e1", "Echo", json!({"text": "hi"})),
            ("f1", "Fail", json!({})),
            ("w1", "Write", json!({"file_path": "a.txt"})),
            ("x1", "Nope", json!({})),
        ]),
        opts,
        Hooks::new(),
        None,
    )
    .await;
    let ds = decisions(&events);
    let started: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolCallStarted(s) => Some(s.call.id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(started.len(), 4);
    for id in &started {
        decision_for(&ds, id);
    }
    let e1 = decision_for(&ds, "e1");
    assert_eq!(e1.outcome, SecurityOutcome::Executed);
    assert_eq!(e1.decision, PolicyOutcome::Allow);
    assert_eq!(e1.policy_source, Some(PolicyDecisionSource::PermissionMode));
    assert_eq!(decision_for(&ds, "f1").outcome, SecurityOutcome::Failed);
    assert_eq!(decision_for(&ds, "w1").outcome, SecurityOutcome::Blocked);
}

#[tokio::test]
// Synthetic secrets in tool output, model text and tool input never reach the event stream or the session file.
async fn synthetic_secrets_are_redacted_from_events_and_session_file() {
    let secret_text = format!(
        "key={SYNTHETIC_ANTHROPIC_KEY} gh={SYNTHETIC_GITHUB_TOKEN} aws={SYNTHETIC_AWS_KEY}"
    );
    let mut turns = tool_turn(vec![("e1", "Echo", json!({"text": secret_text}))]);
    turns[1].insert(
        0,
        ModelEvent::TextDelta {
            text: format!("your key is {SYNTHETIC_ANTHROPIC_KEY}"),
        },
    );
    let sink = Arc::new(CapturingSink::default());
    let (events, _) = run_with(turns, options(), Hooks::new(), Some(sink.clone())).await;

    let stream_dump: String = events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    let session_dump = sink.lines.lock().unwrap().join("\n");
    assert!(!session_dump.is_empty());
    for dump in [&stream_dump, &session_dump] {
        for secret in [
            SYNTHETIC_ANTHROPIC_KEY,
            SYNTHETIC_GITHUB_TOKEN,
            SYNTHETIC_AWS_KEY,
        ] {
            assert!(!dump.contains(secret), "secret leaked: {secret}");
        }
        assert!(dump.contains("[REDACTED]"));
    }
    // The tool still produced its result; only the recorded copy is scrubbed.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::UserToolResult(r) if r.tool_use_id == "e1")));
}
