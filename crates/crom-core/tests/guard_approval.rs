//! Crom's Guard approval retry (#362) against a minimal fake Guard over
//! Streamable HTTP: one tool, `refund.issue`, that answers
//! `approval_required` unless the call carries `_meta.approval_id = "ap_ok"`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use crom_core::events::{SecurityDecision, SecurityOutcome};
use crom_core::guard::{GuardApprovalHook, GuardApprovalRequest, GuardApprovalStep, GuardApprover};
use crom_core::{
    AgentBuilder, AgentDefinition, AgentEvent, AgentOptions, AgentRegistry, ModelEvent,
    ModelProvider, ModelRequest, PermissionCallback, PermissionDecision, PermissionMode,
    PermissionRequest, ProviderError, ToolCall, ToolRegistry, ToolRule,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const REFUND: &str = "mcp__guard__refund_issue";

#[derive(Default)]
struct FakeGuard {
    /// `tools/call` params, in arrival order.
    calls: Mutex<Vec<Value>>,
    executed: AtomicUsize,
    /// Delay before answering a call that carries an approval id.
    retry_delay: Option<Duration>,
}

impl FakeGuard {
    fn calls(&self) -> Vec<Value> {
        self.calls.lock().unwrap().clone()
    }

    fn respond(&self, method: &str, params: &Value) -> Value {
        match method {
            "initialize" => json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "crom-guard", "version": "0"}
            }),
            "tools/list" => json!({"tools": [{
                "name": "refund.issue", "description": "refund",
                "inputSchema": {"type": "object"}
            }]}),
            "tools/call" => match params["_meta"]["approval_id"].as_str() {
                None => json!({
                    "content": [{"type": "text", "text": "approval_required"}],
                    "structuredContent": {"status": "approval_required",
                        "role": "supervisor", "action_digest": "ab12cd"},
                    "isError": true,
                    "_meta": {"decision_id": "dec-first"}
                }),
                Some("ap_ok") => {
                    self.executed.fetch_add(1, Ordering::SeqCst);
                    json!({
                        "content": [{"type": "text", "text": "{\"refund\":\"ok\"}"}],
                        "structuredContent": {"refund": "ok"},
                        "isError": false,
                        "_meta": {"decision_id": "dec-retry"}
                    })
                }
                Some(_) => json!({
                    "content": [{"type": "text", "text": "approval_invalid"}],
                    "structuredContent": {"status": "denied", "code": "approval_invalid"},
                    "isError": true,
                    "_meta": {"decision_id": "dec-retry"}
                }),
            },
            _ => json!({}),
        }
    }
}

async fn start(guard: Arc<FakeGuard>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let guard = guard.clone();
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while stream.read_exact(&mut b).await.is_ok() {
                    head.push(b[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&head).to_string();
                let len = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                let _ = stream.read_exact(&mut body).await;
                let Ok(req) = serde_json::from_slice::<Value>(&body) else {
                    return;
                };
                if req.get("id").is_none() {
                    let _ = stream
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .await;
                    return;
                }
                let method = req["method"].as_str().unwrap_or("");
                if method == "tools/call" {
                    guard.calls.lock().unwrap().push(req["params"].clone());
                }
                if method == "tools/call" && req["params"]["_meta"]["approval_id"].is_string() {
                    if let Some(d) = guard.retry_delay {
                        tokio::time::sleep(d).await;
                    }
                }
                let result = guard.respond(method, &req["params"]);
                let body = json!({"jsonrpc": "2.0", "id": req["id"], "result": result}).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}/mcp")
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Approve,
    /// Never answers.
    Hang,
    /// Cancels the run, then approves (a cancel while the prompt was open).
    CancelThenApprove,
}

struct TestApprover {
    mode: Mode,
    seen: Mutex<Vec<GuardApprovalRequest>>,
    cancel: Arc<AtomicBool>,
}

#[async_trait]
impl GuardApprover for TestApprover {
    async fn request_approval(&self, request: GuardApprovalRequest) -> Option<String> {
        self.seen.lock().unwrap().push(request);
        match self.mode {
            Mode::Approve => Some("ap_ok".into()),
            Mode::Hang => std::future::pending().await,
            Mode::CancelThenApprove => {
                self.cancel.store(true, Ordering::SeqCst);
                Some("ap_ok".into())
            }
        }
    }
}

struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

/// Requests with a system prompt (subagents) go to `child`, others to `parent`.
struct Routing {
    parent: Mutex<Vec<Vec<ModelEvent>>>,
    child: Mutex<Vec<Vec<ModelEvent>>>,
}

#[async_trait]
impl ModelProvider for Routing {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let q = if req.system.is_some() {
            &self.child
        } else {
            &self.parent
        };
        let mut q = q.lock().unwrap();
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

fn call_turn(id: &str, name: &str, input: Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".into()),
            usage: None,
        },
    ]
}

fn refund_turn() -> Vec<ModelEvent> {
    call_turn(
        "c1",
        REFUND,
        json!({"invoice_id": "acme-inv-001", "amount_cents": 7500}),
    )
}

struct Setup {
    guard: Arc<FakeGuard>,
    approver: Arc<TestApprover>,
    cancel: Arc<AtomicBool>,
    options: AgentOptions,
}

async fn setup(
    mode: Mode,
    retry_delay: Option<Duration>,
    approval_timeout: Option<Duration>,
) -> Setup {
    let guard = Arc::new(FakeGuard {
        retry_delay,
        ..Default::default()
    });
    let url = start(guard.clone()).await;
    let cancel = Arc::new(AtomicBool::new(false));
    let approver = Arc::new(TestApprover {
        mode,
        seen: Mutex::new(Vec::new()),
        cancel: cancel.clone(),
    });
    let server = crom_mcp::StreamableHttpMcpServerConfig {
        url,
        headers: Default::default(),
        startup_timeout: None,
        request_timeout: Some(Duration::from_millis(800)),
        guard: true,
    };
    let options = AgentOptions {
        permission_mode: PermissionMode::Default,
        permission_callback: Some(Arc::new(AlwaysAllow)),
        allowed_tools: vec!["mcp__guard__*".into()],
        allowed_tool_rules: vec![ToolRule::McpServerGlob {
            server: "guard".into(),
            tool_glob: "*".into(),
        }],
        max_turns: Some(4),
        mcp: crom_mcp::McpOptions {
            enabled: true,
            strict: true,
            servers: [(
                "guard".to_string(),
                crom_mcp::McpServerConfig::StreamableHttp(server),
            )]
            .into(),
            ..Default::default()
        },
        guard_approval: Some(GuardApprovalHook {
            approver: approver.clone(),
            timeout: approval_timeout,
        }),
        ..AgentOptions::default()
    };
    Setup {
        guard,
        approver,
        cancel,
        options,
    }
}

async fn run(
    s: &Setup,
    options: AgentOptions,
    parent: Vec<Vec<ModelEvent>>,
    child: Vec<Vec<ModelEvent>>,
) -> Vec<AgentEvent> {
    let provider = Arc::new(Routing {
        parent: Mutex::new(parent),
        child: Mutex::new(child),
    });
    let agent = AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(ToolRegistry::new())
        .cancellation(s.cancel.clone(), Arc::new(tokio::sync::Notify::new()))
        .build()
        .unwrap();
    let events = tokio::time::timeout(
        Duration::from_secs(20),
        agent.run("work the ticket").collect::<Vec<_>>(),
    )
    .await
    .expect("run finished");
    events
}

fn decision(events: &[AgentEvent], id: &str) -> SecurityDecision {
    events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SecurityDecision(d) if d.tool_use_id == id => Some(d.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no decision for {id}"))
}

fn with_meta(calls: &[Value]) -> usize {
    calls
        .iter()
        .filter(|c| c["_meta"]["approval_id"].is_string())
        .count()
}

/// The approver sees the session and tool-call ids; the retry's decision records the first `approval_required` decision id.
#[tokio::test]
async fn retry_records_first_decision_and_request_correlates() {
    let s = setup(Mode::Approve, None, None).await;
    let events = run(&s, s.options.clone(), vec![refund_turn()], vec![]).await;
    let d = decision(&events, "c1");
    assert_eq!(d.outcome, SecurityOutcome::Executed, "{d:?}");
    assert_eq!(d.guard_decision_id.as_deref(), Some("dec-retry"));
    let a = d.guard_approval.expect("approval recorded");
    assert_eq!(a.step, GuardApprovalStep::Retried);
    assert_eq!(
        a.approval_required_decision_id.as_deref(),
        Some("dec-first")
    );
    let seen = s.approver.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].tool_use_id.as_deref(), Some("c1"));
    assert_eq!(seen[0].session_id, d.session_id.as_str());
    assert_eq!(s.guard.calls().len(), 2);
    assert_eq!(s.guard.executed.load(Ordering::SeqCst), 1);
}

/// A subagent that hits `approval_required` never reaches the parent's approver and gets the original error.
#[tokio::test]
async fn subagent_never_uses_the_parent_approver() {
    let s = setup(Mode::Approve, None, None).await;
    let mut registry = AgentRegistry::new();
    registry
        .register(AgentDefinition {
            name: "worker".into(),
            description: "refund worker".into(),
            prompt: "you are a worker".into(),
            ..Default::default()
        })
        .unwrap();
    let mut options = s.options.clone();
    options.agents = registry;
    options.allowed_tools.push("Agent".into());
    options
        .allowed_tool_rules
        .push(ToolRule::ExactTool("Agent".into()));
    let parent = vec![call_turn(
        "p1",
        "Agent",
        json!({"agent": "worker", "task": "refund it"}),
    )];
    let events = run(&s, options, parent, vec![refund_turn()]).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SubagentStop(_))),
        "subagent ran"
    );
    let calls = s.guard.calls();
    assert_eq!(calls.len(), 1, "child reached Guard once: {calls:?}");
    assert_eq!(with_meta(&calls), 0);
    assert!(s.approver.seen.lock().unwrap().is_empty());
    assert_eq!(s.guard.executed.load(Ordering::SeqCst), 0);
}

/// A run cancelled while the approval prompt is open sends no retry, even if the approver then approves.
#[tokio::test]
async fn cancel_during_approval_sends_no_retry() {
    let s = setup(Mode::CancelThenApprove, None, None).await;
    let _ = run(&s, s.options.clone(), vec![refund_turn()], vec![]).await;
    assert_eq!(s.approver.seen.lock().unwrap().len(), 1);
    assert_eq!(with_meta(&s.guard.calls()), 0);
    assert_eq!(s.guard.executed.load(Ordering::SeqCst), 0);
}

/// A cancel while the approver hangs ends the wait promptly, with no retry.
#[tokio::test]
async fn cancel_while_approver_hangs_ends_the_wait() {
    let s = setup(Mode::Hang, None, None).await;
    let flag = s.cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        flag.store(true, Ordering::SeqCst);
    });
    let _ = run(&s, s.options.clone(), vec![refund_turn()], vec![]).await;
    assert_eq!(with_meta(&s.guard.calls()), 0);
    assert_eq!(s.guard.executed.load(Ordering::SeqCst), 0);
}

/// An approver that exceeds the approval timeout leaves the call blocked as `approval_required`, recorded `timed_out`.
#[tokio::test]
async fn approval_timeout_leaves_approval_required() {
    let s = setup(Mode::Hang, None, Some(Duration::from_millis(200))).await;
    let events = run(&s, s.options.clone(), vec![refund_turn()], vec![]).await;
    let d = decision(&events, "c1");
    assert_eq!(d.outcome, SecurityOutcome::Blocked, "{d:?}");
    assert_eq!(d.reason.as_deref(), Some("approval_required"));
    assert_eq!(d.guard_approval.unwrap().step, GuardApprovalStep::TimedOut);
    assert_eq!(with_meta(&s.guard.calls()), 0);
}

/// A retry that times out is outcome-unknown (`failed`, `guard_timeout`) and is not retried again.
#[tokio::test]
async fn retry_timeout_is_outcome_unknown_and_not_repeated() {
    let s = setup(Mode::Approve, Some(Duration::from_secs(3)), None).await;
    let events = run(&s, s.options.clone(), vec![refund_turn()], vec![]).await;
    let d = decision(&events, "c1");
    assert_eq!(d.outcome, SecurityOutcome::Failed, "{d:?}");
    assert_eq!(d.reason.as_deref(), Some("guard_timeout"));
    let a = d.guard_approval.unwrap();
    assert_eq!(a.step, GuardApprovalStep::Retried);
    assert_eq!(
        a.approval_required_decision_id.as_deref(),
        Some("dec-first")
    );
    assert_eq!(s.approver.seen.lock().unwrap().len(), 1);
    assert_eq!(s.guard.calls().len(), 2, "one call, one retry");
}
