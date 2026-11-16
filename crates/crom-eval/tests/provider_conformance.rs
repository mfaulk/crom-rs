//! Offline lane of the `provider-conformance` suite (#284): every task is
//! replayed for every built-in live profile against a wiremock server that
//! serves the profile's transcript from
//! `tests/fixtures/conformance/<profile>/<task>.json`. Each exchange also
//! asserts what the agent sent (tool-result ids round-tripped, error results,
//! truncation notices, dropped truncated calls).
//!
//! The committed transcripts are rendered from the provider-neutral scripts
//! below (`source = "synthetic"`). Regenerate them with
//! `CROM_CONFORMANCE_REGEN=1 cargo test -p crom-eval --test provider_conformance`.
//! A transcript captured from a live run can replace a synthetic one in the
//! same format.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crom_core::ProviderProfileConfig;
use crom_eval::artifacts::RunDir;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::ProviderMode;
use crom_eval::runner::{run_task, RunConfig};
use crom_eval::task::{discover_tasks, EvalTask};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SUITE: &str = "provider-conformance";
const KEY: &str = "conformance-test-key";
const FORMAT: &str = "crom-conformance-transcript/v1";
const MAX_TOKENS: u32 = 4096;

/// (profile, API-key env var, model)
const PROFILES: [(&str, &str, &str); 4] = [
    ("anthropic", "ANTHROPIC_API_KEY", "claude-sonnet-4-6"),
    ("openai", "OPENAI_API_KEY", "gpt-4.1"),
    ("deepseek", "DEEPSEEK_API_KEY", "deepseek-chat"),
    ("gemini", "GEMINI_API_KEY", "gemini-2.5-pro"),
];

// --- Provider-neutral scripts ---------------------------------------------

struct Call {
    id: &'static str,
    name: &'static str,
    input: Value,
}

fn call(id: &'static str, name: &'static str, input: Value) -> Call {
    Call { id, name, input }
}

enum Reply {
    Calls(Vec<Call>),
    /// A call whose arguments do not parse (`raw`); Gemini always returns a
    /// JSON object, so it gets the schema-invalid `gemini_args` instead.
    BadCall {
        id: &'static str,
        name: &'static str,
        raw: &'static str,
        gemini_args: Value,
    },
    Text(&'static str),
    /// Output cut off at the token limit, optionally mid tool call.
    Truncated {
        text: &'static str,
        call: Option<(&'static str, &'static str, &'static str)>,
    },
}

enum Expect {
    /// The request carries the result of call `id` (tool `tool`).
    ResultFor(&'static str, &'static str),
    /// The request carries an error tool result.
    ErrorResult,
    /// Literal text in the request body.
    Text(&'static str),
    /// Call `id` must not appear (it was truncated and dropped).
    NoCall(&'static str),
}

struct Step {
    expect: Vec<Expect>,
    reply: Reply,
}

fn step(expect: Vec<Expect>, reply: Reply) -> Step {
    Step { expect, reply }
}

fn script(task: &str) -> Option<Vec<Step>> {
    use Expect as E;
    use Reply as R;
    Some(match task {
        "conf-read-edit-verify" => vec![
            step(
                vec![],
                R::Calls(vec![call("rev1", "Read", json!({"file_path": "data.txt"}))]),
            ),
            step(
                vec![E::ResultFor("rev1", "Read"), E::Text("value=0")],
                R::Calls(vec![call(
                    "rev2",
                    "Edit",
                    json!({"file_path": "data.txt", "old_string": "value=0", "new_string": "value=42"}),
                )]),
            ),
            step(
                vec![E::ResultFor("rev2", "Edit")],
                R::Calls(vec![call(
                    "rev3",
                    "Bash",
                    json!({"command": "grep -q 'value=42' data.txt"}),
                )]),
            ),
            step(
                vec![E::ResultFor("rev3", "Bash")],
                R::Text("data.txt now has value=42 and the check passes."),
            ),
        ],
        "conf-tool-error-recovery" => vec![
            step(
                vec![],
                R::Calls(vec![call(
                    "ter1",
                    "Read",
                    json!({"file_path": "settings.toml"}),
                )]),
            ),
            step(
                vec![E::ResultFor("ter1", "Read"), E::ErrorResult],
                R::Calls(vec![call(
                    "ter2",
                    "Read",
                    json!({"file_path": "config/settings.toml"}),
                )]),
            ),
            step(
                vec![E::ResultFor("ter2", "Read"), E::Text("enabled = false")],
                R::Calls(vec![call(
                    "ter3",
                    "Edit",
                    json!({
                        "file_path": "config/settings.toml",
                        "old_string": "enabled = false",
                        "new_string": "enabled = true"
                    }),
                )]),
            ),
            step(
                vec![E::ResultFor("ter3", "Edit")],
                R::Text("settings.toml was missing; enabled config/settings.toml instead."),
            ),
        ],
        "conf-parallel-tools" => vec![
            step(
                vec![],
                R::Calls(vec![
                    call("par1", "Read", json!({"file_path": "a.txt"})),
                    call("par2", "Read", json!({"file_path": "b.txt"})),
                    call("par3", "Read", json!({"file_path": "c.txt"})),
                ]),
            ),
            step(
                vec![
                    E::ResultFor("par1", "Read"),
                    E::ResultFor("par2", "Read"),
                    E::ResultFor("par3", "Read"),
                ],
                R::Calls(vec![call(
                    "par4",
                    "Write",
                    json!({"file_path": "sum.txt", "content": "60\n"}),
                )]),
            ),
            step(
                vec![E::ResultFor("par4", "Write")],
                R::Text("10 + 20 + 30 = 60, written to sum.txt."),
            ),
        ],
        "conf-bash-check-loop" => vec![
            step(
                vec![],
                R::Calls(vec![call(
                    "bcl1",
                    "Bash",
                    json!({"command": "sh check.sh"}),
                )]),
            ),
            step(
                vec![E::ResultFor("bcl1", "Bash")],
                R::Calls(vec![call(
                    "bcl2",
                    "Write",
                    json!({"file_path": "status.txt", "content": "ready\n"}),
                )]),
            ),
            step(
                vec![E::ResultFor("bcl2", "Write")],
                R::Calls(vec![call(
                    "bcl3",
                    "Bash",
                    json!({"command": "sh check.sh"}),
                )]),
            ),
            step(
                vec![E::ResultFor("bcl3", "Bash")],
                R::Text("check.sh now exits 0."),
            ),
        ],
        "conf-invalid-args-recovery" => vec![
            step(
                vec![],
                R::BadCall {
                    id: "iar1",
                    name: "Write",
                    raw: "{\"file_path\": \"out.txt\", \"content\": ",
                    gemini_args: json!({"path": "out.txt"}),
                },
            ),
            step(
                vec![E::ResultFor("iar1", "Write"), E::ErrorResult],
                R::Calls(vec![call(
                    "iar2",
                    "Write",
                    json!({"file_path": "out.txt", "content": "hello\n"}),
                )]),
            ),
            step(
                vec![E::ResultFor("iar2", "Write")],
                R::Text("Created out.txt."),
            ),
        ],
        "conf-long-output" => vec![
            step(
                vec![],
                R::Truncated {
                    text: "Writing the file now.\n1\n2\n3\n",
                    call: Some((
                        "lo1",
                        "Write",
                        "{\"file_path\": \"numbers.txt\", \"content\": \"1\\n2\\n3\\n4\\n5\\n",
                    )),
                },
            ),
            step(
                vec![E::Text("output token limit"), E::NoCall("lo1")],
                R::Calls(vec![call(
                    "lo2",
                    "Bash",
                    json!({"command": "seq 1 3000 > numbers.txt"}),
                )]),
            ),
            step(
                vec![E::ResultFor("lo2", "Bash")],
                R::Text("Generated numbers.txt with seq."),
            ),
        ],
        _ => return None,
    })
}

// --- Transcript format ------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct Transcript {
    format: String,
    profile: String,
    task: String,
    model: String,
    /// `synthetic` (rendered from the scripts here) or `recorded`.
    source: String,
    path: String,
    exchanges: Vec<Exchange>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Exchange {
    request_contains: Vec<String>,
    #[serde(default)]
    request_excludes: Vec<String>,
    status: u16,
    content_type: String,
    body: String,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/conformance")
}

fn transcript_path(profile: &str, task: &str) -> PathBuf {
    fixtures_dir().join(profile).join(format!("{task}.json"))
}

// --- Rendering --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    Anthropic,
    OpenAi,
    Gemini,
}

fn wire(profile: &str) -> Wire {
    match profile {
        "anthropic" => Wire::Anthropic,
        "gemini" => Wire::Gemini,
        _ => Wire::OpenAi,
    }
}

fn wire_id(w: Wire, id: &str) -> String {
    match w {
        Wire::Anthropic => format!("toolu_{id}"),
        _ => format!("call_{id}"),
    }
}

fn render_expect(w: Wire, e: &Expect) -> Option<(bool, String)> {
    // (must_contain, needle)
    Some(match (w, e) {
        (Wire::OpenAi, Expect::ResultFor(id, _)) => {
            (true, format!("\"tool_call_id\":\"{}\"", wire_id(w, id)))
        }
        (Wire::Anthropic, Expect::ResultFor(id, _)) => {
            (true, format!("\"tool_use_id\":\"{}\"", wire_id(w, id)))
        }
        (Wire::Gemini, Expect::ResultFor(_, tool)) => {
            (true, format!("\"functionResponse\":{{\"name\":\"{tool}\""))
        }
        (Wire::OpenAi, Expect::ErrorResult) => (true, "\"content\":\"ERROR: ".to_string()),
        (Wire::Anthropic, Expect::ErrorResult) => (true, "\"is_error\":true".to_string()),
        (Wire::Gemini, Expect::ErrorResult) => (true, "\"isError\":true".to_string()),
        (_, Expect::Text(t)) => (true, (*t).to_string()),
        // Gemini has no wire ids; nothing to look for.
        (Wire::Gemini, Expect::NoCall(_)) => return None,
        (_, Expect::NoCall(id)) => (false, wire_id(w, id)),
    })
}

fn openai_body(w: Wire, reply: &Reply, model: &str) -> String {
    let tool_call = |id: &str, name: &str, args: String| {
        json!({"id": wire_id(w, id), "type": "function",
               "function": {"name": name, "arguments": args}})
    };
    let (content, calls, finish): (Value, Vec<Value>, &str) = match reply {
        Reply::Calls(cs) => (
            Value::Null,
            cs.iter()
                .map(|c| tool_call(c.id, c.name, c.input.to_string()))
                .collect(),
            "tool_calls",
        ),
        Reply::BadCall { id, name, raw, .. } => (
            Value::Null,
            vec![tool_call(id, name, (*raw).to_string())],
            "tool_calls",
        ),
        Reply::Text(t) => (json!(t), vec![], "stop"),
        Reply::Truncated { text, call } => (
            json!(text),
            call.iter()
                .map(|(id, name, raw)| tool_call(id, name, (*raw).to_string()))
                .collect(),
            "length",
        ),
    };
    let mut message = json!({"role": "assistant", "content": content});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    serde_json::to_string_pretty(&json!({
        "id": "chatcmpl-conformance", "object": "chat.completion", "model": model,
        "choices": [{"index": 0, "finish_reason": finish, "message": message}],
        "usage": {"prompt_tokens": 120, "completion_tokens": 24, "total_tokens": 144}
    }))
    .unwrap()
}

fn gemini_body(reply: &Reply) -> String {
    let fc = |name: &str, args: &Value| json!({"functionCall": {"name": name, "args": args}});
    let (parts, finish): (Vec<Value>, &str) = match reply {
        Reply::Calls(cs) => (cs.iter().map(|c| fc(c.name, &c.input)).collect(), "STOP"),
        Reply::BadCall {
            name, gemini_args, ..
        } => (vec![fc(name, gemini_args)], "STOP"),
        Reply::Text(t) => (vec![json!({"text": t})], "STOP"),
        // Gemini does not emit a partial functionCall; truncation surfaces as
        // cut-off text with finishReason MAX_TOKENS.
        Reply::Truncated { text, .. } => (vec![json!({"text": text})], "MAX_TOKENS"),
    };
    serde_json::to_string_pretty(&json!({
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": finish}],
        "usageMetadata": {"promptTokenCount": 120, "candidatesTokenCount": 24}
    }))
    .unwrap()
}

fn anthropic_body(reply: &Reply, model: &str) -> String {
    let mut events = vec![json!({"type": "message_start", "message": {
        "id": "msg_conformance", "type": "message", "role": "assistant", "model": model,
        "content": [], "stop_reason": null,
        "usage": {"input_tokens": 120, "output_tokens": 0}}})];
    let mut index = 0;
    let mut text_block = |events: &mut Vec<Value>, text: &str| {
        events.push(json!({"type": "content_block_start", "index": index,
            "content_block": {"type": "text", "text": ""}}));
        events.push(json!({"type": "content_block_delta", "index": index,
            "delta": {"type": "text_delta", "text": text}}));
        events.push(json!({"type": "content_block_stop", "index": index}));
        index += 1;
    };
    let mut tool_blocks: Vec<(String, String, String)> = Vec::new();
    let stop = match reply {
        Reply::Calls(cs) => {
            for c in cs {
                tool_blocks.push((
                    wire_id(Wire::Anthropic, c.id),
                    c.name.to_string(),
                    c.input.to_string(),
                ));
            }
            "tool_use"
        }
        Reply::BadCall { id, name, raw, .. } => {
            tool_blocks.push((
                wire_id(Wire::Anthropic, id),
                name.to_string(),
                raw.to_string(),
            ));
            "tool_use"
        }
        Reply::Text(t) => {
            text_block(&mut events, t);
            "end_turn"
        }
        Reply::Truncated { text, call } => {
            text_block(&mut events, text);
            if let Some((id, name, raw)) = call {
                tool_blocks.push((
                    wire_id(Wire::Anthropic, id),
                    name.to_string(),
                    raw.to_string(),
                ));
            }
            "max_tokens"
        }
    };
    // Live Anthropic cuts a tool_use at `max_tokens` without a
    // `content_block_stop` (observed in the M17 live conformance run).
    let truncated = matches!(reply, Reply::Truncated { .. });
    for (idx, (id, name, partial)) in (index..).zip(tool_blocks) {
        events.push(json!({"type": "content_block_start", "index": idx,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}));
        events.push(json!({"type": "content_block_delta", "index": idx,
            "delta": {"type": "input_json_delta", "partial_json": partial}}));
        if !truncated {
            events.push(json!({"type": "content_block_stop", "index": idx}));
        }
    }
    events.push(
        json!({"type": "message_delta", "delta": {"stop_reason": stop},
        "usage": {"output_tokens": 24}}),
    );
    events.push(json!({"type": "message_stop"}));
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

fn render(profile: &str, model: &str, task: &str, steps: &[Step]) -> Transcript {
    let w = wire(profile);
    let path = match w {
        Wire::Anthropic => "/v1/messages".to_string(),
        Wire::OpenAi => "/v1/chat/completions".to_string(),
        Wire::Gemini => format!("/v1beta/models/{model}:generateContent"),
    };
    // Every request names the model and the profile's max-tokens field.
    let mut always = match (profile, w) {
        (_, Wire::Gemini) => vec![format!("\"maxOutputTokens\":{MAX_TOKENS}")],
        ("openai", _) => vec![format!("\"max_completion_tokens\":{MAX_TOKENS}")],
        _ => vec![format!("\"max_tokens\":{MAX_TOKENS}")],
    };
    if w != Wire::Gemini {
        always.push(format!("\"model\":\"{model}\""));
    }
    let exchanges = steps
        .iter()
        .map(|s| {
            let mut request_contains = always.clone();
            let mut request_excludes = Vec::new();
            for e in &s.expect {
                match render_expect(w, e) {
                    Some((true, n)) => request_contains.push(n),
                    Some((false, n)) => request_excludes.push(n),
                    None => {}
                }
            }
            let (content_type, body) = match w {
                Wire::OpenAi => ("application/json", openai_body(w, &s.reply, model)),
                Wire::Gemini => ("application/json", gemini_body(&s.reply)),
                Wire::Anthropic => ("text/event-stream", anthropic_body(&s.reply, model)),
            };
            Exchange {
                request_contains,
                request_excludes,
                status: 200,
                content_type: content_type.to_string(),
                body,
            }
        })
        .collect();
    Transcript {
        format: FORMAT.to_string(),
        profile: profile.to_string(),
        task: task.to_string(),
        model: model.to_string(),
        source: "synthetic".to_string(),
        path,
        exchanges,
    }
}

// --- Replay -----------------------------------------------------------------

struct BodyExcludes(String);

impl wiremock::Match for BodyExcludes {
    fn matches(&self, request: &Request) -> bool {
        !String::from_utf8_lossy(&request.body).contains(&self.0)
    }
}

struct BodyContains(String);

impl wiremock::Match for BodyContains {
    fn matches(&self, request: &Request) -> bool {
        String::from_utf8_lossy(&request.body).contains(&self.0)
    }
}

fn auth(profile: &str) -> (&'static str, String) {
    match wire(profile) {
        Wire::Anthropic => ("x-api-key", KEY.to_string()),
        Wire::Gemini => ("x-goog-api-key", KEY.to_string()),
        Wire::OpenAi => ("authorization", format!("Bearer {KEY}")),
    }
}

fn conformance_tasks() -> Vec<EvalTask> {
    let tasks: Vec<EvalTask> = discover_tasks(&default_tasks_dir())
        .unwrap()
        .into_iter()
        .filter(|t| t.in_suite(SUITE))
        .collect();
    assert!(tasks.len() >= 6, "expected >= 6 {SUITE} tasks");
    tasks
}

fn load(profile: &str, task: &str) -> Transcript {
    let p = transcript_path(profile, task);
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("missing transcript {}: {e}", p.display()));
    let t: Transcript = serde_json::from_str(&text).unwrap();
    assert_eq!(t.format, FORMAT, "{}", p.display());
    assert_eq!((t.profile.as_str(), t.task.as_str()), (profile, task));
    t
}

/// Replay one transcript; returns a failure description or `None`.
async fn replay(task: &EvalTask, t: &Transcript) -> Option<String> {
    let server = MockServer::start().await;
    let (auth_name, auth_value) = auth(&t.profile);
    for (i, ex) in t.exchanges.iter().enumerate() {
        let mut mock = Mock::given(method("POST"))
            .and(path(t.path.as_str()))
            .and(header(auth_name, auth_value.as_str()));
        for needle in &ex.request_contains {
            mock = mock.and(BodyContains(needle.clone()));
        }
        for needle in &ex.request_excludes {
            mock = mock.and(BodyExcludes(needle.clone()));
        }
        mock.respond_with(
            ResponseTemplate::new(ex.status).set_body_raw(ex.body.clone(), &ex.content_type),
        )
        .up_to_n_times(1)
        .named(format!("{}/{} exchange {i}", t.profile, t.task))
        .mount(&server)
        .await;
    }

    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "conformance").unwrap();
    let profiles: BTreeMap<String, ProviderProfileConfig> =
        serde_json::from_value(json!({ t.profile.as_str(): { "base_url": server.uri() } }))
            .unwrap();
    let config = RunConfig {
        provider: ProviderMode::Live(t.profile.clone()),
        model: Some(t.model.clone()),
        provider_profiles: profiles,
        timeout_grace: Duration::from_secs(30),
        ..RunConfig::default()
    };
    let result = run_task(task, &run, &config).await;
    let received = server.received_requests().await.unwrap_or_default();
    let mut problems = Vec::new();
    if !result.resolved {
        problems.push(format!(
            "not resolved: status={:?} failure={:?} skipped={:?} notes={:?}",
            result.status, result.failure_class, result.skipped_reason, result.score.notes
        ));
    }
    if received.len() != t.exchanges.len() {
        problems.push(format!(
            "expected {} requests, got {}",
            t.exchanges.len(),
            received.len()
        ));
    }
    // Explain the first request that matched no exchange.
    if let Some((i, req)) = received.iter().enumerate().find(|(i, req)| {
        t.exchanges.get(*i).is_some_and(|ex| {
            let body = String::from_utf8_lossy(&req.body);
            ex.request_contains
                .iter()
                .any(|n| !body.contains(n.as_str()))
                || ex
                    .request_excludes
                    .iter()
                    .any(|n| body.contains(n.as_str()))
        })
    }) {
        let body = String::from_utf8_lossy(&req.body);
        let ex = &t.exchanges[i];
        let missing: Vec<&String> = ex
            .request_contains
            .iter()
            .filter(|n| !body.contains(n.as_str()))
            .collect();
        let present: Vec<&String> = ex
            .request_excludes
            .iter()
            .filter(|n| body.contains(n.as_str()))
            .collect();
        problems.push(format!(
            "request {i}: missing {missing:?}, unexpected {present:?}; body={body}"
        ));
    }
    (!problems.is_empty()).then(|| format!("{}/{}: {}", t.profile, t.task, problems.join("; ")))
}

// --- Tests --------------------------------------------------------------------

/// Opt in to live providers with placeholder keys. Called first by every test
/// in this binary so the environment is written once, before any reads.
fn live_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var(crom_eval::LIVE_ENV, "1");
        for (_, key_env, _) in PROFILES {
            std::env::set_var(key_env, KEY);
        }
    });
}

// Every conformance task has a script and a committed transcript for every built-in profile (regenerated when CROM_CONFORMANCE_REGEN=1).
#[test]
fn every_task_has_a_transcript_per_profile() {
    live_env();
    let regen = std::env::var("CROM_CONFORMANCE_REGEN").as_deref() == Ok("1");
    for task in conformance_tasks() {
        let steps = script(&task.id).unwrap_or_else(|| panic!("no script for {}", task.id));
        for (profile, _, model) in PROFILES {
            let p = transcript_path(profile, &task.id);
            if regen {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                let t = render(profile, model, &task.id, &steps);
                std::fs::write(&p, serde_json::to_string_pretty(&t).unwrap() + "\n").unwrap();
            }
            let t = load(profile, &task.id);
            assert!(!t.exchanges.is_empty(), "{}", p.display());
        }
    }
}

// Conformance tasks are L0 harness tasks whose unmodified fixture fails the oracle.
#[tokio::test]
async fn conformance_tasks_validate() {
    use crom_eval::schema::{Level, TaskKind};
    live_env();
    for task in conformance_tasks() {
        assert_eq!(task.kind, Some(TaskKind::Harness), "{}", task.id);
        assert_eq!(task.level, Some(Level::L0), "{}", task.id);
        let report = crom_eval::validate::validate_task(&task).await;
        assert!(report.ok(), "{}", report.to_markdown());
    }
}

// Every conformance task resolves for anthropic, openai, deepseek, and gemini when replayed against its transcript.
#[tokio::test(flavor = "multi_thread")]
async fn suite_passes_offline_for_every_profile() {
    live_env();
    let tasks = conformance_tasks();
    let mut failures = Vec::new();
    for (profile, _, _) in PROFILES {
        for task in &tasks {
            let t = load(profile, &task.id);
            if let Some(f) = replay(task, &t).await {
                failures.push(f);
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
