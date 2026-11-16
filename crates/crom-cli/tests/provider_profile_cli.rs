use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;
use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "ds-secret-test-key-0123456789";

// Synthetic DeepSeek chat-completions responses: a tool call, then a final answer.
// `reasoning_content` and the DeepSeek cache counters are included to prove they
// are tolerated.
const TOOL_CALL_BODY: &str = r#"{
  "id": "ds-1", "object": "chat.completion", "model": "deepseek-chat",
  "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
    "role": "assistant", "content": "", "reasoning_content": "need to read the file",
    "tool_calls": [{"id": "call_0", "type": "function",
      "function": {"name": "Read", "arguments": "{\"file_path\":\"hello.txt\"}"}}]}}],
  "usage": {"prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60,
    "prompt_cache_hit_tokens": 0, "prompt_cache_miss_tokens": 50}
}"#;

const FINAL_BODY: &str = r#"{
  "id": "ds-2", "object": "chat.completion", "model": "deepseek-chat",
  "choices": [{"index": 0, "finish_reason": "stop", "message": {
    "role": "assistant", "content": "The file says: profile-fixture-marker"}}],
  "usage": {"prompt_tokens": 80, "completion_tokens": 8, "total_tokens": 88,
    "prompt_cache_hit_tokens": 50, "prompt_cache_miss_tokens": 30}
}"#;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crom"))
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

// `crom run --provider deepseek --model deepseek-chat` completes a tool-using turn
// against a wiremock DeepSeek endpoint configured via the settings `providers` key,
// authenticating with DEEPSEEK_API_KEY, and the key never reaches output or files.
#[tokio::test(flavor = "multi_thread")]
async fn deepseek_profile_completes_tool_turn() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(
            serde_json::json!({"model": "deepseek-chat"}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(TOOL_CALL_BODY))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_string_contains("hello-from-fixture"))
        .respond_with(ResponseTemplate::new(200).set_body_string(FINAL_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let dir = TempDir::new().unwrap();
    let work = dir.path().join("work");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join("crom")).unwrap();
    std::fs::write(work.join("hello.txt"), "hello-from-fixture\n").unwrap();
    // User settings: an untrusted repo may not redirect provider endpoints (#290).
    std::fs::write(
        home.join("crom").join("settings.json"),
        serde_json::json!({
            "providers": { "deepseek": { "base_url": server.uri() } },
            "allowed_tools": ["Read"]
        })
        .to_string(),
    )
    .unwrap();

    let work_arg = work.clone();
    let home_env = home.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(bin())
            .args([
                "run",
                "--cwd",
                work_arg.to_str().unwrap(),
                "--provider",
                "deepseek",
                "--model",
                "deepseek-chat",
                "--permission-mode",
                "dontAsk",
                "--allowed-tools",
                "Read",
                "Read hello.txt and tell me what it says.",
            ])
            .current_dir(&work_arg)
            .env("HOME", &home_env)
            .env("XDG_CONFIG_HOME", home_env.join(".config"))
            .env("CROM_CONFIG_HOME", home_env.join("crom"))
            .env("DEEPSEEK_API_KEY", KEY)
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("RUST_LOG")
            .output()
            .expect("spawn crom")
    })
    .await
    .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout={stdout}\nstderr={stderr}");
    assert!(
        stdout.contains("profile-fixture-marker"),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(!stdout.contains(KEY) && !stderr.contains(KEY));
    let leaked = files_containing(dir.path(), KEY);
    assert!(leaked.is_empty(), "API key written to {leaked:?}");
    server.verify().await;
}

// An unknown provider id fails fast and names the available profiles.
#[test]
fn unknown_provider_lists_profiles() {
    let dir = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["run", "--cwd", ".", "--provider", "nope-ai", "hi"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("CROM_CONFIG_HOME", dir.path().join("crom"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("deepseek"), "stderr={stderr}");
}
