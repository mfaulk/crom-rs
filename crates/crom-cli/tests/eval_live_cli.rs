//! Offline coverage of `crom eval run --provider <profile>` for live
//! profiles: each profile's base URL is pointed at a wiremock server that
//! speaks that provider's wire format, so no key or network is needed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{json, Value};
use tempfile::TempDir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "eval-live-test-key-0123456789";
const TASK_ID: &str = "live-wire-edit";

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crom"))
}

/// One scripted model turn.
enum Turn {
    Tool(&'static str, &'static str, Value),
    Text(&'static str),
}

fn script() -> Vec<Turn> {
    vec![
        Turn::Tool("call_1", "Read", json!({ "file_path": "data.txt" })),
        Turn::Tool(
            "call_2",
            "Edit",
            json!({ "file_path": "data.txt", "old_string": "value=0", "new_string": "value=42" }),
        ),
        Turn::Text("Set value=42 in data.txt."),
    ]
}

/// Chat Completions (OpenAI, DeepSeek) non-streaming response body.
fn openai_body(turn: &Turn) -> String {
    let (message, finish) = match turn {
        Turn::Tool(id, name, input) => (
            json!({ "role": "assistant", "content": null, "tool_calls": [{
                "id": id, "type": "function",
                "function": { "name": name, "arguments": input.to_string() } }] }),
            "tool_calls",
        ),
        Turn::Text(text) => (json!({ "role": "assistant", "content": text }), "stop"),
    };
    json!({
        "id": "chatcmpl-test", "object": "chat.completion", "model": "test",
        "choices": [{ "index": 0, "finish_reason": finish, "message": message }],
        "usage": { "prompt_tokens": 40, "completion_tokens": 8, "total_tokens": 48 }
    })
    .to_string()
}

/// Gemini `generateContent` response body.
fn gemini_body(turn: &Turn) -> String {
    let part = match turn {
        Turn::Tool(_, name, input) => json!({ "functionCall": { "name": name, "args": input } }),
        Turn::Text(text) => json!({ "text": text }),
    };
    json!({
        "candidates": [{ "content": { "role": "model", "parts": [part] }, "finishReason": "STOP" }],
        "usageMetadata": { "promptTokenCount": 40, "candidatesTokenCount": 8 }
    })
    .to_string()
}

/// Anthropic Messages SSE stream.
fn anthropic_body(turn: &Turn) -> String {
    let mut events = vec![json!({ "type": "message_start", "message": {
        "id": "msg_test", "model": "test", "role": "assistant", "content": [],
        "stop_reason": null, "usage": { "input_tokens": 40, "output_tokens": 0 } } })];
    let stop = match turn {
        Turn::Tool(id, name, input) => {
            events.push(json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }));
            events.push(json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }));
            "tool_use"
        }
        Turn::Text(text) => {
            events.push(json!({ "type": "content_block_start", "index": 0,
                "content_block": { "type": "text", "text": "" } }));
            events.push(json!({ "type": "content_block_delta", "index": 0,
                "delta": { "type": "text_delta", "text": text } }));
            "end_turn"
        }
    };
    events.push(json!({ "type": "content_block_stop", "index": 0 }));
    events.push(
        json!({ "type": "message_delta", "delta": { "stop_reason": stop },
        "usage": { "output_tokens": 8 } }),
    );
    events.push(json!({ "type": "message_stop" }));
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect()
}

struct Wire {
    profile: &'static str,
    key_env: &'static str,
    model: &'static str,
}

impl Wire {
    fn path(&self) -> String {
        match self.profile {
            "gemini" => format!("/v1beta/models/{}:generateContent", self.model),
            "anthropic" => "/v1/messages".to_string(),
            _ => "/v1/chat/completions".to_string(),
        }
    }

    fn auth(&self) -> (&'static str, String) {
        match self.profile {
            "gemini" => ("x-goog-api-key", KEY.to_string()),
            "anthropic" => ("x-api-key", KEY.to_string()),
            _ => ("authorization", format!("Bearer {KEY}")),
        }
    }

    fn response(&self, turn: &Turn) -> ResponseTemplate {
        match self.profile {
            "gemini" => {
                ResponseTemplate::new(200).set_body_raw(gemini_body(turn), "application/json")
            }
            "anthropic" => {
                ResponseTemplate::new(200).set_body_raw(anthropic_body(turn), "text/event-stream")
            }
            _ => ResponseTemplate::new(200).set_body_raw(openai_body(turn), "application/json"),
        }
    }

    /// Serve the script in order; each turn answers exactly one request.
    async fn mount(&self, server: &MockServer) {
        let (name, value) = self.auth();
        for turn in script() {
            Mock::given(method("POST"))
                .and(path(self.path()))
                .and(header(name, value.as_str()))
                .respond_with(self.response(&turn))
                .up_to_n_times(1)
                .expect(1)
                .mount(server)
                .await;
        }
    }
}

/// A temp project with one eval task, settings that point `profile` at
/// `base_url`, and isolated home/config dirs.
struct Project {
    dir: TempDir,
}

impl Project {
    fn new(profile: &str, base_url: Option<&str>) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let task = root.join("tasks").join(TASK_ID);
        std::fs::create_dir_all(task.join("repo")).unwrap();
        std::fs::write(task.join("repo").join("data.txt"), "value=0\n").unwrap();
        std::fs::write(
            task.join("task.toml"),
            format!(
                r#"schema_version = 3
task_version = "1.0.0"
kind = "harness"
level = "L0"
network = "none"
id = "{TASK_ID}"
title = "Live wire: read and edit a value"
suites = ["live-wire"]
prompt = "Set value=42 in data.txt."
max_turns = 6
timeout_secs = 60

[fixture]
kind = "copy_dir"
path = "repo"

[oracle]
kind = "command"
commands = ["grep -q 'value=42' data.txt"]
"#
            ),
        )
        .unwrap();
        std::fs::create_dir_all(root.join("work")).unwrap();
        // User settings, not repo `.agent/settings.json`: an untrusted
        // workspace's `providers` are ignored (#290).
        let config_home = root.join("home").join("crom");
        std::fs::create_dir_all(&config_home).unwrap();
        let providers = match base_url {
            Some(url) => json!({ profile: { "base_url": url } }),
            None => json!({}),
        };
        std::fs::write(
            config_home.join("settings.json"),
            json!({ "providers": providers }).to_string(),
        )
        .unwrap();
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn command(&self, profile: &str, model: &str) -> Command {
        let root = self.root();
        let home = root.join("home");
        let mut cmd = Command::new(bin());
        cmd.args([
            "eval",
            "run",
            TASK_ID,
            "--provider",
            profile,
            "--model",
            model,
        ])
        .arg("--tasks-dir")
        .arg(root.join("tasks"))
        .arg("--runs-dir")
        .arg(root.join("runs"))
        .args(["--sandbox", "off", "--no-index"])
        .current_dir(root.join("work"))
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CROM_CONFIG_HOME", home.join("crom"))
        .env_remove("CROM_EVAL_MODEL")
        .env_remove("CODE_AGENT_EVAL_MODEL")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("RUST_LOG");
        for var in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "GEMINI_API_KEY",
        ] {
            cmd.env_remove(var);
        }
        cmd
    }

    /// The single `result.json` written under the runs dir.
    fn result(&self) -> Value {
        let runs = self.root().join("runs");
        let run = std::fs::read_dir(&runs)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text =
            std::fs::read_to_string(run.join("tasks").join(TASK_ID).join("result.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

fn files_containing(root: &Path, needle: &str) -> Vec<PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if std::fs::read(&p)
                .map(|b| String::from_utf8_lossy(&b).contains(needle))
                .unwrap_or(false)
            {
                hits.push(p);
            }
        }
    }
    hits
}

async fn spawn(mut cmd: Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().expect("spawn crom"))
        .await
        .unwrap()
}

async fn resolves_through(wire: Wire) {
    let server = MockServer::start().await;
    wire.mount(&server).await;
    let project = Project::new(wire.profile, Some(&server.uri()));
    let mut cmd = project.command(wire.profile, wire.model);
    cmd.env("CROM_EVAL_LIVE", "1").env(wire.key_env, KEY);
    let out = spawn(cmd).await;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{}: stdout={stdout}\nstderr={stderr}",
        wire.profile
    );
    let result = project.result();
    assert_eq!(result["resolved"], true, "{}: {result:#}", wire.profile);
    assert_eq!(result["outcome"], "resolved");
    assert_eq!(result["provider"], wire.profile);
    assert_eq!(result["model"], wire.model);
    assert_eq!(result["repro"]["provider"]["profile"], wire.profile);
    assert_eq!(result["repro"]["provider"]["model"], wire.model);
    let host = server.address().to_string();
    assert_eq!(result["repro"]["provider"]["base_url_host"], host.as_str());
    assert!(!stdout.contains(KEY) && !stderr.contains(KEY));
    let leaked = files_containing(project.root(), KEY);
    assert!(leaked.is_empty(), "API key written to {leaked:?}");
    server.verify().await;
}

// The openai profile resolves a task through a Chat Completions wiremock endpoint with --model.
#[tokio::test(flavor = "multi_thread")]
async fn openai_profile_resolves_task() {
    resolves_through(Wire {
        profile: "openai",
        key_env: "OPENAI_API_KEY",
        model: "gpt-4.1-mini",
    })
    .await;
}

// The deepseek profile resolves a task through a Chat Completions wiremock endpoint with --model.
#[tokio::test(flavor = "multi_thread")]
async fn deepseek_profile_resolves_task() {
    resolves_through(Wire {
        profile: "deepseek",
        key_env: "DEEPSEEK_API_KEY",
        model: "deepseek-chat",
    })
    .await;
}

// The gemini profile resolves a task through a generateContent wiremock endpoint with --model.
#[tokio::test(flavor = "multi_thread")]
async fn gemini_profile_resolves_task() {
    resolves_through(Wire {
        profile: "gemini",
        key_env: "GEMINI_API_KEY",
        model: "gemini-2.5-flash",
    })
    .await;
}

// The anthropic profile resolves a task through a Messages SSE wiremock endpoint with --model.
#[tokio::test(flavor = "multi_thread")]
async fn anthropic_profile_resolves_task() {
    resolves_through(Wire {
        profile: "anthropic",
        key_env: "ANTHROPIC_API_KEY",
        model: "claude-haiku-4-5",
    })
    .await;
}

// Without the profile's key every task is skipped as credentials_unavailable and the exit code is 3, not 1.
#[tokio::test(flavor = "multi_thread")]
async fn missing_key_skips_with_distinct_exit_code() {
    for profile in ["openai", "deepseek", "gemini", "anthropic"] {
        let project = Project::new(profile, None);
        let mut cmd = project.command(profile, "any-model");
        cmd.env("CROM_EVAL_LIVE", "1");
        let out = spawn(cmd).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(3), "{profile}: stderr={stderr}");
        assert!(stderr.contains("credentials_unavailable"), "{stderr}");
        let result = project.result();
        assert_eq!(result["outcome"], "skipped");
        assert_eq!(result["model"], "any-model");
        assert!(result["skipped_reason"]
            .as_str()
            .unwrap()
            .starts_with("credentials_unavailable: "));
    }
}

// Without CROM_EVAL_LIVE=1 a live run is skipped as live_disabled, also with exit code 3.
#[tokio::test(flavor = "multi_thread")]
async fn live_opt_in_missing_skips() {
    let project = Project::new("deepseek", None);
    let mut cmd = project.command("deepseek", "deepseek-chat");
    cmd.env_remove("CROM_EVAL_LIVE")
        .env_remove("CODE_AGENT_EVAL_LIVE")
        .env("DEEPSEEK_API_KEY", KEY);
    let out = spawn(cmd).await;
    assert_eq!(out.status.code(), Some(3));
    assert!(project.result()["skipped_reason"]
        .as_str()
        .unwrap()
        .starts_with("live_disabled: "));
}

// A task that runs and fails exits 1, distinct from the all-skipped exit code.
#[tokio::test(flavor = "multi_thread")]
async fn failed_task_exits_one() {
    let project = Project::new("openai", None);
    let mut cmd = Command::new(bin());
    cmd.args(["eval", "run", TASK_ID, "--provider", "fake"])
        .arg("--tasks-dir")
        .arg(project.root().join("tasks"))
        .arg("--runs-dir")
        .arg(project.root().join("runs"))
        .arg("--no-index")
        .current_dir(project.root().join("work"))
        .env("HOME", project.root().join("home"))
        .env("CROM_CONFIG_HOME", project.root().join("home").join("crom"));
    let out = spawn(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(project.result()["outcome"], "failed");
}
