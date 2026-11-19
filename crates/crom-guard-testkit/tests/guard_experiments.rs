//! MILESTONE_18 §13.4 experiments E1, E2, E4a, E5 (#363). Each test writes
//! `$CARGO_TARGET_TMPDIR/guard-experiments/<e>.json` (`guard-experiments/v1`),
//! which `crom eval scoreboard --guard-report` renders. Latency (E5) is
//! reported, never asserted.
#![cfg(unix)]

mod server_env;

use std::path::PathBuf;
use std::time::Instant;

use crom_guard_testkit::experiments::{
    decision_sequence, latency_stats, policy_samples_ns, temp_dir_fs, CellResult, E4aResults,
    E5Results, ExperimentReport, LatencyRow, Matrix, OnOffRun, ProviderReplay,
};
use crom_guard_testkit::scenario::{
    fixtures, run, unmediated_effects, AuditView, CallRequest, DirectEnv, InProcessEnv,
    ScenarioEnv, ScenarioReport,
};
use crom_guard_testkit::LedgerEntry;
use serde_json::{json, Value};
use server_env::HttpEnv;

fn out(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("guard-experiments")
        .join(name)
}

fn command(test: &str) -> String {
    format!("cargo test -p crom-guard-testkit --test guard_experiments {test} -- --nocapture")
}

/// E1: every cell of the hand-written expected table agrees with Guard in process and through the reference server, on agent-visible results, ledger effects, mediation, and audit verification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e1_authorization_matrix() {
    let m = Matrix::load();
    let mut results = Vec::new();
    for cell in &m.cells {
        let fx = cell.fixture(&m);
        let env = InProcessEnv::start().await;
        let r = run(&fx, &env).await;
        results.push(CellResult::new(cell, "guard_in_process", &r));
        let env = HttpEnv::start().await;
        let r = run(&fx, &env).await;
        env.shutdown().await;
        results.push(CellResult::new(cell, "guard_server", &r));
    }
    let report = ExperimentReport::new(
        "E1",
        "Authorization matrix",
        &command("e1_"),
        json!({"cells": m.cells.len(), "results": results}),
    );
    report.write(&out("e1.json")).unwrap();
    let bad: Vec<String> = results
        .iter()
        .filter(|r| !r.agrees)
        .map(|r| {
            format!(
                "{} ({}): {} expected {} observed {} ledger_matches={} mismatches={:?}",
                r.id, r.lane, r.label, r.expected, r.observed, r.ledger_matches, r.mismatches
            )
        })
        .collect();
    eprintln!(
        "E1: {}/{} cell runs agree; report: {}",
        results.len() - bad.len(),
        results.len(),
        out("e1.json").display()
    );
    assert!(m.cells.len() >= 40);
    assert!(bad.is_empty(), "E1 disagreements:\n{}", bad.join("\n"));
}

async fn timed<E: ScenarioEnv>(
    fx: &crom_guard_testkit::scenario::Fixture,
    env: &E,
) -> (ScenarioReport, std::time::Duration) {
    let t = Instant::now();
    let r = run(fx, env).await;
    (r, t.elapsed())
}

/// E2: the same scripted workflows and attacks execute no attack with Guard and at least one per guard-dependent scenario with the direct-credential baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e2_guard_on_off() {
    let mut runs = Vec::new();
    let mut failures = Vec::new();
    for fx in fixtures()
        .iter()
        .filter(|f| f.isolation.is_none() && f.placeholder.is_none())
    {
        let env = InProcessEnv::start().await;
        let (r, wall) = timed(fx, &env).await;
        if r.executed != 0 || !r.legit_success {
            failures.push(format!(
                "{} on: executed={} legit={}",
                fx.scenario, r.executed, r.legit_success
            ));
        }
        runs.push(OnOffRun::new(&r, "guard_in_process", true, wall));
        let env = DirectEnv::start().await;
        let (r, wall) = timed(fx, &env).await;
        if fx.guard_dependent && r.executed == 0 {
            failures.push(format!("{} off: no attack executed", fx.scenario));
        }
        runs.push(OnOffRun::new(&r, "guard_off", false, wall));
    }
    let report = ExperimentReport::new(
        "E2",
        "Guard on vs off",
        &command("e2_"),
        json!({"runs": runs}),
    );
    report.write(&out("e2.json")).unwrap();
    eprintln!("E2 report: {}", out("e2.json").display());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The E5 grant: acme operator on `acme-cus-000`.
fn e5_grant() -> Value {
    json!({
        "tenant": "acme",
        "delegator": {"id": "acme-agent-operator", "role": "agent-operator",
                      "attrs": {"refund_limit_cents": {"long": 5000}}},
        "run_id": "e5",
        "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"],
        "constraints": {
            "customers": {"set": [{"type": "Support::Customer", "id": "acme-cus-000"}]},
            "max_refund_cents": {"at_most": 10000}
        },
        "ttl_seconds": 3600,
        "max_depth": 0
    })
}

/// (operation label, tool, arguments, expected result, measured on `direct`)
fn e5_operations() -> Vec<(&'static str, &'static str, Value, &'static str, bool)> {
    vec![
        (
            "customer.read (allow)",
            "customer.read",
            json!({"customer_id": "acme-cus-000"}),
            "executed",
            true,
        ),
        (
            "invoice.list (allow)",
            "invoice.list",
            json!({"customer_id": "acme-cus-000"}),
            "executed",
            true,
        ),
        (
            "refund.issue 0.01 (allow)",
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 1}),
            "executed",
            true,
        ),
        (
            "refund.issue other customer (policy_deny)",
            "refund.issue",
            json!({"invoice_id": "acme-inv-002", "amount_cents": 1}),
            "policy_deny",
            false,
        ),
        (
            "customer.export (policy_deny)",
            "customer.export",
            json!({"customer_id": "acme-cus-000"}),
            "policy_deny",
            false,
        ),
    ]
}

fn iterations() -> usize {
    std::env::var("CROM_GUARD_E5_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
}

/// Times `iters` calls of one operation after `warmup` untimed ones; panics if any result differs from `want`.
async fn measure<E: ScenarioEnv>(
    env: &E,
    handle: &str,
    tool: &str,
    args: &Value,
    want: &str,
    warmup: usize,
    iters: usize,
) -> Vec<u64> {
    let req = || CallRequest {
        handle: handle.to_owned(),
        tool: tool.to_owned(),
        arguments: args.clone(),
        approval_id: None,
        attack: want != "executed",
        revoke_after_decision: None,
    };
    let mut samples = Vec::with_capacity(iters);
    for i in 0..warmup + iters {
        let t = Instant::now();
        let o = env.call(req()).await;
        let ns = t.elapsed().as_nanos() as u64;
        assert_eq!(o.code, want, "{tool} {args}: {}", o.text);
        if i >= warmup {
            samples.push(ns);
        }
    }
    samples
}

/// E5: p50/p95/p99 of warm calls per operation for the direct trusted executor, Guard in process, Guard over HTTP, and Guard's policy-only time; reported, not asserted. Set `CROM_GUARD_E5_ITERS` (default 200) and use `--release` for recorded numbers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e5_latency() {
    let iters = iterations();
    let warmup = (iters / 10).clamp(10, 100);
    let ops = e5_operations();
    let grant = e5_grant();
    let mut rows = Vec::new();
    let mut push = |op: &str, path: &str, result: &str, s: &[u64]| {
        if let Some(stats) = latency_stats(s) {
            rows.push(LatencyRow {
                operation: op.into(),
                path: path.into(),
                result: result.into(),
                warmup,
                stats,
            });
        }
    };

    let direct = DirectEnv::start().await;
    let h = direct.issue("parent", &grant).await.unwrap();
    for (op, tool, args, _, on_direct) in &ops {
        if *on_direct {
            let s = measure(&direct, &h, tool, args, "executed", warmup, iters).await;
            push(op, "direct", "executed", &s);
        }
    }

    let inproc = InProcessEnv::start().await;
    let h = inproc.issue("parent", &grant).await.unwrap();
    let mut policy = Vec::new();
    for (op, tool, args, want, _) in &ops {
        measure(&inproc, &h, tool, args, want, 0, warmup).await;
        // Policy time comes from the decision records this operation's timed
        // calls appended, and from no other operation's.
        let start = inproc.audit().unwrap().records.len();
        let s = measure(&inproc, &h, tool, args, want, 0, iters).await;
        push(op, "guard_in_process", want, &s);
        let p = policy_samples_ns(&inproc.audit().unwrap().records[start..], tool);
        assert_eq!(p.len(), iters, "one decision record per timed {op} call");
        policy.push((op, want, p));
    }
    assert!(inproc.audit().unwrap().verified.is_ok());
    for (op, want, p) in policy {
        push(op, "policy_only", want, &p);
    }

    let http = HttpEnv::start().await;
    let h = http.issue("parent", &grant).await.unwrap();
    for (op, tool, args, want, _) in &ops {
        let s = measure(&http, &h, tool, args, want, warmup, iters).await;
        push(op, "guard_http", want, &s);
    }
    http.shutdown().await;

    let results = E5Results {
        iterations: iters,
        warmup,
        audit_sink: format!(
            "FileAuditSink (hash-chained JSONL, fsync per record) in {}",
            temp_dir_fs()
        ),
        rows,
    };
    for r in &results.rows {
        eprintln!(
            "{:<44} {:<17} p50={:>8.1}us p95={:>8.1}us p99={:>8.1}us n={}",
            r.operation, r.path, r.stats.p50_us, r.stats.p95_us, r.stats.p99_us, r.stats.n
        );
    }
    let report = ExperimentReport::new(
        "E5",
        "Overhead",
        &format!(
            "CROM_GUARD_E5_ITERS={iters} cargo test{} -p crom-guard-testkit --test guard_experiments e5_ -- --nocapture",
            if cfg!(debug_assertions) { "" } else { " --release" }
        ),
        results,
    );
    report.write(&out("e5.json")).unwrap();
    eprintln!("E5 report: {}", out("e5.json").display());
}

// ------------------------------------------------------------------ E4a

const E4A_FORMAT: &str = "crom-conformance-transcript/v1";
const E4A_KEY: &str = "e4a-replay-key";
/// (profile, model), as in the provider-conformance suite.
/// Profile, model, and wire format (OpenAI and DeepSeek share chat
/// completions: four profiles, three formats).
const E4A_PROFILES: [(&str, &str, &str); 4] = [
    ("anthropic", "claude-sonnet-4-6", "anthropic-messages"),
    ("openai", "gpt-4.1", "openai-chat-completions"),
    ("deepseek", "deepseek-chat", "openai-chat-completions"),
    ("gemini", "gemini-2.5-pro", "gemini-generate-content"),
];

/// The E4a grant: the E5 grant under its own run id.
fn e4a_grant() -> Value {
    let mut g = e5_grant();
    g["run_id"] = json!("e4a");
    g
}

/// The provider-neutral calls every transcript encodes, one per model turn:
/// (call id, Guard action, arguments).
fn e4a_calls() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        (
            "e4a1",
            "invoice.list",
            json!({"customer_id": "acme-cus-000"}),
        ),
        (
            "e4a2",
            "invoice.list",
            json!({"customer_id": "globex-cus-000"}),
        ),
        (
            "e4a3",
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        ),
        (
            "e4a4",
            "refund.issue",
            json!({"invoice_id": "acme-inv-000", "amount_cents": 1000}),
        ),
        (
            "e4a5",
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 7500}),
        ),
        (
            "e4a6",
            "customer.export",
            json!({"customer_id": "acme-cus-000"}),
        ),
        (
            "e4a7",
            "customer.read",
            json!({"customer_id": "acme-cus-000"}),
        ),
    ]
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Transcript {
    format: String,
    profile: String,
    task: String,
    model: String,
    source: String,
    path: String,
    exchanges: Vec<Exchange>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Exchange {
    request_contains: Vec<String>,
    #[serde(default)]
    request_excludes: Vec<String>,
    status: u16,
    content_type: String,
    body: String,
}

fn transcript_path(profile: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/experiments/e4a")
        .join(format!("{profile}.json"))
}

/// Crom's agent-visible name for a Guard action.
fn crom_tool(action: &str) -> String {
    format!("mcp__guard__{}", action.replace('.', "_"))
}

/// One provider response in `profile`'s wire format: a single tool call, or
/// the final text.
fn render_body(profile: &str, model: &str, call: Option<&(&str, &str, Value)>) -> (String, String) {
    let text = "Ticket handled.";
    match profile {
        "anthropic" => {
            let mut ev = vec![json!({"type": "message_start", "message": {
                "id": "msg_e4a", "type": "message", "role": "assistant", "model": model,
                "content": [], "stop_reason": null,
                "usage": {"input_tokens": 120, "output_tokens": 0}}})];
            let stop = match call {
                Some((id, action, args)) => {
                    ev.push(json!({"type": "content_block_start", "index": 0, "content_block":
                        {"type": "tool_use", "id": format!("toolu_{id}"), "name": crom_tool(action), "input": {}}}));
                    ev.push(json!({"type": "content_block_delta", "index": 0,
                        "delta": {"type": "input_json_delta", "partial_json": args.to_string()}}));
                    "tool_use"
                }
                None => {
                    ev.push(json!({"type": "content_block_start", "index": 0,
                        "content_block": {"type": "text", "text": ""}}));
                    ev.push(json!({"type": "content_block_delta", "index": 0,
                        "delta": {"type": "text_delta", "text": text}}));
                    "end_turn"
                }
            };
            ev.push(json!({"type": "content_block_stop", "index": 0}));
            ev.push(
                json!({"type": "message_delta", "delta": {"stop_reason": stop},
                "usage": {"output_tokens": 24}}),
            );
            ev.push(json!({"type": "message_stop"}));
            let body = ev
                .iter()
                .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                .collect();
            ("text/event-stream".into(), body)
        }
        "gemini" => {
            let parts = match call {
                Some((_, action, args)) => {
                    json!([{"functionCall": {"name": crom_tool(action), "args": args}}])
                }
                None => json!([{"text": text}]),
            };
            let body = json!({
                "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 24}
            });
            (
                "application/json".into(),
                serde_json::to_string_pretty(&body).unwrap(),
            )
        }
        _ => {
            let mut message = json!({"role": "assistant", "content": null});
            let finish = match call {
                Some((id, action, args)) => {
                    message["tool_calls"] = json!([{"id": format!("call_{id}"), "type": "function",
                        "function": {"name": crom_tool(action), "arguments": args.to_string()}}]);
                    "tool_calls"
                }
                None => {
                    message["content"] = json!(text);
                    "stop"
                }
            };
            let body = json!({
                "id": "chatcmpl-e4a", "object": "chat.completion", "model": model,
                "choices": [{"index": 0, "finish_reason": finish, "message": message}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 24, "total_tokens": 144}
            });
            (
                "application/json".into(),
                serde_json::to_string_pretty(&body).unwrap(),
            )
        }
    }
}

/// The request after a call carries that call's result, in `profile`'s wire format.
fn result_needle(profile: &str, call: &(&str, &str, Value)) -> String {
    match profile {
        "anthropic" => format!("\"tool_use_id\":\"toolu_{}\"", call.0),
        "gemini" => format!("\"functionResponse\":{{\"name\":\"{}\"", crom_tool(call.1)),
        _ => format!("\"tool_call_id\":\"call_{}\"", call.0),
    }
}

fn render_transcript(profile: &str, model: &str) -> Transcript {
    let calls = e4a_calls();
    let path = match profile {
        "anthropic" => "/v1/messages".to_owned(),
        "gemini" => format!("/v1beta/models/{model}:generateContent"),
        _ => "/v1/chat/completions".to_owned(),
    };
    let mut exchanges = Vec::new();
    for i in 0..=calls.len() {
        let (content_type, body) = render_body(profile, model, calls.get(i));
        exchanges.push(Exchange {
            request_contains: i
                .checked_sub(1)
                .map(|p| vec![result_needle(profile, &calls[p])])
                .unwrap_or_default(),
            request_excludes: Vec::new(),
            status: 200,
            content_type,
            body,
        });
    }
    Transcript {
        format: E4A_FORMAT.into(),
        profile: profile.into(),
        task: "guard-e4a".into(),
        model: model.into(),
        source: "synthetic".into(),
        path,
        exchanges,
    }
}

fn load_transcript(profile: &str, model: &str) -> Transcript {
    let path = transcript_path(profile);
    if std::env::var_os("CROM_E4A_REGEN").is_some() {
        let t = render_transcript(profile, model);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&t).unwrap() + "\n").unwrap();
    }
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; regenerate with CROM_E4A_REGEN=1", path.display()));
    let t: Transcript = serde_json::from_str(&text).unwrap();
    assert_eq!(
        (t.format.as_str(), t.profile.as_str()),
        (E4A_FORMAT, profile)
    );
    t
}

struct BodyContains(String);

impl wiremock::Match for BodyContains {
    fn matches(&self, request: &wiremock::Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(&self.0)
    }
}

struct Replayed {
    served: usize,
    audit: AuditView,
    ledger: Vec<LedgerEntry>,
}

/// Serves `t` and runs a Crom agent with that provider against a fresh
/// reference server.
async fn replay_through_crom(t: &Transcript) -> Replayed {
    use crom_sdk::{
        AgentRequest, AgentRuntime, GuardConnection, ProviderConfig, SecretString, ToolPolicy,
        WorkspaceSource,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let mock = MockServer::start().await;
    for (i, ex) in t.exchanges.iter().enumerate() {
        let mut m = Mock::given(method("POST")).and(path(t.path.as_str()));
        for n in &ex.request_contains {
            m = m.and(BodyContains(n.clone()));
        }
        m.respond_with(
            ResponseTemplate::new(ex.status).set_body_raw(ex.body.clone(), &ex.content_type),
        )
        .up_to_n_times(1)
        .named(format!("{} exchange {i}", t.profile))
        .mount(&mock)
        .await;
    }
    let env = HttpEnv::start().await;
    let handle = env.issue("parent", &e4a_grant()).await.unwrap();
    let overrides: std::collections::BTreeMap<String, crom_core::ProviderProfileConfig> =
        serde_json::from_value(json!({ t.profile.as_str(): { "base_url": mock.uri() } })).unwrap();
    let profile = crom_core::provider_profile::resolve_profile(&t.profile, &overrides).unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::profile(
            profile,
            SecretString::new(E4A_KEY.to_owned()),
        ))
        .model(t.model.clone())
        .tool_policy(ToolPolicy::guard(GuardConnection::new(
            env.mcp_url(),
            SecretString::new(handle),
        )))
        .workspace(WorkspaceSource::Filesystem(workspace.path().to_path_buf()))
        .build()
        .await
        .unwrap();
    let mut run = runtime
        .run(AgentRequest::new("Handle support ticket E4A.").max_turns(20))
        .await
        .unwrap();
    while let Some(ev) = futures::StreamExt::next(&mut run.events).await {
        ev.unwrap();
    }
    let result = run.final_result().await.unwrap();
    let served = mock.received_requests().await.unwrap_or_default().len();
    assert_eq!(
        served,
        t.exchanges.len(),
        "{}: {:?} {}",
        t.profile,
        result.status,
        result.summary
    );
    let audit = env.audit().unwrap();
    let ledger = env.api().ledger().entries();
    env.shutdown().await;
    Replayed {
        served,
        audit,
        ledger,
    }
}

/// E4a: tool calls recorded in each provider's wire format, decoded by Crom's provider adapters and sent through Crom's MCP client, produce exactly the Guard decisions of the same calls sent directly, for every provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn e4a_provider_replay() {
    let env = HttpEnv::start().await;
    let handle = env.issue("parent", &e4a_grant()).await.unwrap();
    for (_, action, args) in e4a_calls() {
        env.call(CallRequest {
            handle: handle.clone(),
            tool: action.into(),
            arguments: args,
            approval_id: None,
            attack: false,
            revoke_after_decision: None,
        })
        .await;
    }
    let reference = decision_sequence(&env.audit().unwrap().records);
    env.shutdown().await;
    assert_eq!(reference.len(), e4a_calls().len());

    let mut providers = Vec::new();
    for (profile, model, wire_format) in E4A_PROFILES {
        let t = load_transcript(profile, model);
        let r = replay_through_crom(&t).await;
        let decisions = decision_sequence(&r.audit.records);
        providers.push(ProviderReplay {
            provider: profile.into(),
            model: t.model.clone(),
            wire_format: wire_format.into(),
            transcript: format!("fixtures/experiments/e4a/{profile}.json"),
            source: t.source.clone(),
            requests_served: r.served,
            equal_to_reference: decisions == reference,
            ledger_effects: r.ledger.len(),
            audit_verified: r.audit.verified.is_ok(),
            unmediated: unmediated_effects(&r.ledger, &r.audit.records).len(),
            decisions,
        });
    }
    let all_equal = providers.iter().all(|p| p.equal_to_reference);
    let results = E4aResults {
        reference,
        providers,
        all_equal,
    };
    let report = ExperimentReport::new(
        "E4a",
        "Provider independence (offline replay)",
        &command("e4a_"),
        &results,
    );
    report.write(&out("e4a.json")).unwrap();
    eprintln!("E4a report: {}", out("e4a.json").display());
    for p in &results.providers {
        assert!(p.audit_verified && p.unmediated == 0, "{}", p.provider);
        assert!(
            p.equal_to_reference,
            "{}: {:#?}\nreference: {:#?}",
            p.provider, p.decisions, results.reference
        );
    }
}

/// Each committed synthetic E4a transcript is exactly what the provider-neutral call script renders; a `recorded` transcript is exempt.
#[test]
fn e4a_transcripts_match_the_script() {
    for (profile, model, _) in E4A_PROFILES {
        let text = std::fs::read_to_string(transcript_path(profile)).unwrap();
        let committed: Value = serde_json::from_str(&text).unwrap();
        if committed["source"] == "recorded" {
            continue;
        }
        let rendered = serde_json::to_value(render_transcript(profile, model)).unwrap();
        assert_eq!(
            committed, rendered,
            "{profile}: regenerate with CROM_E4A_REGEN=1"
        );
    }
}
