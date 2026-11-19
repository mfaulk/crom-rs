//! Crom as a Crom Guard client (#359, MILESTONE_18 §9.5, §12.4): a real
//! `crom-guard-server` in process over the testkit support operations, and a
//! scripted Crom run calling it through `ToolPolicy::guard`.
#![cfg(all(unix, feature = "fake-provider"))]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use crom_core::events::{EnforcementLayer, SecurityDecision, SecurityOutcome};
use crom_core::{ModelEvent, ModelProvider, ModelRequest, ProviderError, ToolCall};
use crom_guard_server::{AdminAddr, ServerHandle};
use crom_guard_testkit::{
    operations, ApiCredential, BusinessApi, Ledger, RunningApi, SupportConfig,
};
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::{
    AgentRequest, AgentRunStatus, AgentRuntime, FileStorage, GuardConnection, ProviderConfig,
    SdkEvent, SecretString, SessionPolicy, ToolPolicy,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");
const ADMIN_TOKEN: &str = "adm-test-7f3e9a1c5b2d4e6f8a0b1c2d3e4f5a6b";
const DOWNSTREAM_SECRET: &str = "sk-business-CANARY-9d2e";
const READ: &str = "mcp__guard__customer_read";

struct Guard {
    dir: tempfile::TempDir,
    mcp_url: String,
    server: Option<ServerHandle>,
    _api: RunningApi,
}

fn write_private(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

impl Guard {
    async fn start() -> Self {
        let api = BusinessApi::new(
            vec![ApiCredential {
                id: "business-api".into(),
                secret: DOWNSTREAM_SECRET.into(),
            }],
            Ledger::in_memory(),
        );
        let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mut registry = crom_guard::OperationRegistry::new();
        for op in operations(&api, &SupportConfig::new(running.base_url())).unwrap() {
            registry.register(Arc::new(op)).unwrap();
        }
        registry.register_delegate().unwrap();

        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("support.cedarschema"), SCHEMA).unwrap();
        std::fs::write(d.join("support.cedar"), POLICIES).unwrap();
        let creds = json!({"credentials": [{
            "id": "business-api",
            "secret": DOWNSTREAM_SECRET,
            "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"]
        }]});
        write_private(&d.join("creds.json"), &creds.to_string());
        write_private(&d.join("admin.token"), &format!("{ADMIN_TOKEN}\n"));
        std::fs::write(
            d.join("guard.toml"),
            r#"
                [agent]
                listen = "127.0.0.1:0"
                [admin]
                listen = "127.0.0.1:0"
                token_file = "admin.token"
                app_id = "support-app"
                [policy]
                namespace = "Support"
                schema = "support.cedarschema"
                policies = "support.cedar"
                [credentials]
                file = "creds.json"
                [audit]
                path = "audit.jsonl"
            "#,
        )
        .unwrap();
        let config = crom_guard_server::Config::load(&d.join("guard.toml")).unwrap();
        let server = crom_guard_server::start(config, registry).await.unwrap();
        Self {
            dir,
            mcp_url: format!("http://{}/mcp", server.agent_addr),
            server: Some(server),
            _api: running,
        }
    }

    /// Issues an acme grant limited to customer `acme-cus-000`; returns the session handle.
    async fn issue_handle(&self) -> String {
        self.issue_handle_with(&["customer.read", "invoice.list", "refund.issue"], 0)
            .await
    }

    async fn issue_handle_with(&self, actions: &[&str], max_depth: u8) -> String {
        let AdminAddr::Tcp(admin) = &self.server.as_ref().unwrap().admin_addr else {
            panic!("tcp admin listener expected");
        };
        let spec = json!({
            "tenant": "acme",
            "delegator": {"id": "acme-agent-operator", "role": "agent-operator",
                          "attrs": {"refund_limit_cents": {"long": 5000}}},
            "run_id": "run-crom",
            "actions": actions,
            "constraints": {
                "customers": {"set": [{"type": "Support::Customer", "id": "acme-cus-000"}]},
                "max_refund_cents": {"at_most": 10000}
            },
            "ttl_seconds": 600,
            "max_depth": max_depth
        });
        let resp = reqwest::Client::new()
            .post(format!("http://{admin}/admin/grants"))
            .bearer_auth(ADMIN_TOKEN)
            .json(&spec)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 201);
        let body: Value = resp.json().await.unwrap();
        body["handle"].as_str().unwrap().to_owned()
    }

    /// Guard's audit log, one JSON object per line.
    fn audit(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("audit.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}

type BeforeTurn = Box<dyn Fn(usize) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Scripted provider that records every request it is sent (the model's
/// whole context) and can run a hook before answering a turn.
struct RecordingProvider {
    inner: ScriptedFakeProvider,
    requests: Arc<Mutex<Vec<String>>>,
    turn: Mutex<usize>,
    before_turn: Option<BeforeTurn>,
}

#[async_trait]
impl ModelProvider for RecordingProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(format!("{request:?}"));
        let turn = {
            let mut t = self.turn.lock().unwrap();
            *t += 1;
            *t
        };
        if let Some(hook) = &self.before_turn {
            hook(turn).await;
        }
        self.inner.stream_turn(request).await
    }
}

fn tool_turn(calls: &[(&str, &str, Value)]) -> Vec<Vec<ModelEvent>> {
    let mut first: Vec<ModelEvent> = calls
        .iter()
        .map(|(id, name, input)| {
            ModelEvent::ToolCall(ToolCall {
                id: (*id).into(),
                name: (*name).into(),
                input: input.clone(),
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

/// Captures every `tracing` event from every crate at TRACE.
fn trace_buffer() -> Arc<Mutex<Vec<u8>>> {
    static BUF: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    BUF.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || BufWriter(writer.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        buf
    })
    .clone()
}

struct BufWriter(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for BufWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RunOutput {
    status: AgentRunStatus,
    decisions: Vec<SecurityDecision>,
    events: Vec<SdkEvent>,
    /// Every Crom output a handle could leak into, as text.
    outputs: String,
}

async fn run_crom(
    guard: &Guard,
    handle: &str,
    turns: Vec<Vec<ModelEvent>>,
    before_turn: Option<BeforeTurn>,
) -> RunOutput {
    let traces = trace_buffer();
    let storage_dir = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingProvider {
        inner: ScriptedFakeProvider::new(turns),
        requests: requests.clone(),
        turn: Mutex::new(0),
        before_turn,
    };
    let connection =
        GuardConnection::new(guard.mcp_url.clone(), SecretString::new(handle.to_owned()));
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .tool_policy(ToolPolicy::guard(connection))
        .storage(Arc::new(FileStorage::new(storage_dir.path()).unwrap()))
        .build()
        .await
        .unwrap();
    let request = AgentRequest::new("look up the customer")
        .session_policy(SessionPolicy::Persisted)
        .max_turns(3);
    let mut run = runtime.run(request).await.unwrap();
    let mut events = Vec::new();
    while let Some(ev) = run.events.next().await {
        events.push(ev.unwrap());
    }
    let result = run.final_result().await.unwrap();

    let mut outputs = format!("{result:?}\n{runtime:?}\n");
    for ev in &events {
        outputs.push_str(&serde_json::to_string(ev).unwrap());
        outputs.push('\n');
    }
    for r in requests.lock().unwrap().iter() {
        outputs.push_str(r);
    }
    for entry in walk(storage_dir.path()) {
        outputs.push_str(&String::from_utf8_lossy(&std::fs::read(entry).unwrap()));
    }
    outputs.push_str(&String::from_utf8_lossy(&traces.lock().unwrap()));
    let decisions = events
        .iter()
        .filter_map(|e| match e {
            SdkEvent::SecurityDecision(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    RunOutput {
        status: result.status,
        decisions,
        events,
        outputs,
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn decision<'a>(ds: &'a [SecurityDecision], id: &str) -> &'a SecurityDecision {
    let m: Vec<_> = ds.iter().filter(|d| d.tool_use_id == id).collect();
    assert_eq!(m.len(), 1, "one decision for {id}: {ds:?}");
    m[0]
}

fn tool_completed_is_error(events: &[SdkEvent], id: &str) -> bool {
    events
        .iter()
        .find_map(|e| match e {
            SdkEvent::ToolCallCompleted(c) if c.tool_use_id == id => Some(c.is_error),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no ToolCallCompleted for {id}"))
}

/// Allowed and denied Guard calls produce Crom decisions at layer `guard` that join Guard's audit records on `guard_decision_id`.
#[tokio::test]
async fn allowed_and_denied_calls_correlate_with_guard_audit() {
    let guard = Guard::start().await;
    let handle = guard.issue_handle().await;
    let out = run_crom(
        &guard,
        &handle,
        tool_turn(&[
            ("ok", READ, json!({"customer_id": "acme-cus-000"})),
            ("deny", READ, json!({"customer_id": "globex-cus-000"})),
        ]),
        None,
    )
    .await;

    let ok = decision(&out.decisions, "ok");
    assert_eq!(ok.enforcement, EnforcementLayer::Guard, "{ok:?}");
    assert_eq!(ok.outcome, SecurityOutcome::Executed, "{ok:?}");
    assert_eq!(ok.resource.as_deref(), Some("mcp:guard"));
    let ok_id = ok.guard_decision_id.clone().expect("decision id");

    let deny = decision(&out.decisions, "deny");
    assert_eq!(deny.enforcement, EnforcementLayer::Guard, "{deny:?}");
    assert_eq!(deny.outcome, SecurityOutcome::Blocked, "{deny:?}");
    assert_eq!(deny.reason.as_deref(), Some("resource_not_found"));
    let deny_id = deny.guard_decision_id.clone().expect("decision id");
    assert!(tool_completed_is_error(&out.events, "deny"));
    assert!(!tool_completed_is_error(&out.events, "ok"));

    let audit = guard.audit();
    let records = |id: &str| -> Vec<&Value> {
        audit
            .iter()
            .filter(|r| r["decision_id"].as_str() == Some(id))
            .collect()
    };
    let ok_recs = records(&ok_id);
    assert!(
        ok_recs
            .iter()
            .any(|r| r["record"] == "decision" && r["decision"] == "allow"),
        "{ok_recs:?}"
    );
    assert!(
        ok_recs
            .iter()
            .any(|r| r["record"] == "outcome" && r["outcome"] == "executed"),
        "{ok_recs:?}"
    );
    let deny_recs = records(&deny_id);
    assert_eq!(deny_recs.len(), 1, "{deny_recs:?}");
    assert_eq!(deny_recs[0]["decision"], "deny");
    assert_eq!(deny_recs[0]["reason"]["reason"], "resource_not_found");
}

/// An unreachable Guard turns the call into a tool error recorded `blocked` at layer `guard`.
#[tokio::test]
async fn unreachable_guard_is_a_blocked_tool_error() {
    let mut guard = Guard::start().await;
    let handle = guard.issue_handle().await;
    let server = Arc::new(AsyncMutex::new(guard.server.take()));
    let s = server.clone();
    // Tools are discovered at run start; Guard goes away before the call.
    let hook: BeforeTurn = Box::new(move |turn| {
        let s = s.clone();
        Box::pin(async move {
            if turn == 1 {
                if let Some(h) = s.lock().await.take() {
                    h.shutdown().await;
                }
            }
        })
    });
    let out = run_crom(
        &guard,
        &handle,
        tool_turn(&[("c1", READ, json!({"customer_id": "acme-cus-000"}))]),
        Some(hook),
    )
    .await;
    let d = decision(&out.decisions, "c1");
    assert_eq!(d.enforcement, EnforcementLayer::Guard, "{d:?}");
    assert_eq!(d.outcome, SecurityOutcome::Blocked, "{d:?}");
    assert_eq!(d.reason.as_deref(), Some("guard_unavailable"));
    assert_eq!(d.guard_decision_id, None);
    assert!(tool_completed_is_error(&out.events, "c1"));
}

/// The session handle appears in no Crom output: events, run result, model context, persisted session, or traces.
#[tokio::test]
async fn session_handle_canary_is_absent_from_crom_outputs() {
    let guard = Guard::start().await;
    let handle = guard.issue_handle().await;
    assert!(handle.starts_with("gsh_"));
    let out = run_crom(
        &guard,
        &handle,
        tool_turn(&[
            ("ok", READ, json!({"customer_id": "acme-cus-000"})),
            ("deny", READ, json!({"customer_id": "globex-cus-000"})),
        ]),
        None,
    )
    .await;
    // The run really used Guard, so the canary was in play.
    assert!(decision(&out.decisions, "ok").guard_decision_id.is_some());
    assert!(out.outputs.contains("mcp__guard__customer_read"));
    // Traces were captured, including the MCP client's request logs.
    assert!(out.outputs.contains("tools/call"));
    let bare = handle.trim_start_matches("gsh_");
    assert!(!out.outputs.contains(&handle), "session handle leaked");
    assert!(!out.outputs.contains(bare), "session handle body leaked");
    assert!(!out.outputs.contains(DOWNSTREAM_SECRET));
}

/// A handle Guard does not recognize fails the run at tool discovery (MCP strict), so no Guard tool is called.
#[tokio::test]
async fn unknown_handle_fails_the_run_before_any_call() {
    let guard = Guard::start().await;
    let bogus = format!("gsh_{}", "1".repeat(64));
    let out = run_crom(
        &guard,
        &bogus,
        tool_turn(&[("c1", READ, json!({"customer_id": "acme-cus-000"}))]),
        None,
    )
    .await;
    assert!(
        !out.decisions
            .iter()
            .any(|d| d.outcome == SecurityOutcome::Executed),
        "{:?}",
        out.decisions
    );
    assert_eq!(out.status, AgentRunStatus::Failed);
    assert!(guard.audit().is_empty());
}

/// Guard's `guard.delegate` (it returns a child session handle) is never registered as a Crom tool.
#[tokio::test]
async fn guard_delegate_is_not_exposed() {
    let guard = Guard::start().await;
    let handle = guard
        .issue_handle_with(&["customer.read", "guard.delegate"], 1)
        .await;
    let out = run_crom(
        &guard,
        &handle,
        tool_turn(&[
            ("ok", READ, json!({"customer_id": "acme-cus-000"})),
            (
                "d1",
                "mcp__guard__guard_delegate",
                json!({"actions": ["customer.read"], "ttl_seconds": 60}),
            ),
        ]),
        None,
    )
    .await;
    assert_eq!(
        decision(&out.decisions, "ok").outcome,
        SecurityOutcome::Executed
    );
    let d = decision(&out.decisions, "d1");
    assert_eq!(d.enforcement, EnforcementLayer::Policy, "{d:?}");
    assert_eq!(d.outcome, SecurityOutcome::Blocked, "{d:?}");
    assert_eq!(
        d.reason.as_deref(),
        Some("tool not allowed: mcp__guard__guard_delegate")
    );
    let audit = guard.audit();
    assert!(
        !audit.iter().any(|r| r["action"] == "guard.delegate"),
        "{audit:?}"
    );
}

/// `act` reaches Guard only for the Guard tools the plan names; an unplanned Guard tool never reaches Guard.
#[tokio::test]
async fn act_reaches_guard_only_for_planned_tools() {
    use crom_core::plan::{PlanDocument, PlanId, PlanToolUse};
    let guard = Guard::start().await;
    let handle = guard.issue_handle().await;
    let mut plan = PlanDocument::new_draft(PlanId("plan-guard".into()), "t", "look up");
    plan.requested_tools.push(PlanToolUse {
        tool_name: READ.into(),
        permission_kind: crom_sdk::PermissionKind::External,
        input_constraints: Value::Null,
        reason: None,
    });
    let provider = ScriptedFakeProvider::new(tool_turn(&[
        ("a1", READ, json!({"customer_id": "acme-cus-000"})),
        (
            "a2",
            "mcp__guard__invoice_list",
            json!({"customer_id": "acme-cus-000"}),
        ),
    ]));
    let connection = GuardConnection::new(guard.mcp_url.clone(), SecretString::new(handle));
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .tool_policy(ToolPolicy::guard(connection))
        .build()
        .await
        .unwrap();
    let mut req = crom_sdk::ActRequest::from_plan(plan);
    req.test_force_approval = true;
    runtime.act(req).await.unwrap();
    let audit = guard.audit();
    assert!(
        audit
            .iter()
            .any(|r| r["action"] == "customer.read" && r["outcome"] == "executed"),
        "{audit:?}"
    );
    assert!(
        !audit.iter().any(|r| r["action"] == "invoice.list"),
        "{audit:?}"
    );
}
