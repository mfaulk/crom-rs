use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crom_mcp::{McpClient, McpError, McpLimits, McpServerConfig, StdioMcpServerConfig};

fn fake_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mcp-server/Cargo.toml")
}

static BUILD_FAKE_SERVER: std::sync::Once = std::sync::Once::new();

/// Pre-builds the fake MCP server fixture once per test binary so the
/// `cargo run` used to launch it is a near-instant no-op spawn rather than a
/// build that races other concurrent spawns under load (#35).
fn ensure_fake_server_built() {
    BUILD_FAKE_SERVER.call_once(|| {
        let status = std::process::Command::new("cargo")
            .args([
                "build",
                "--quiet",
                "--manifest-path",
                &fake_manifest().display().to_string(),
            ])
            .status()
            .expect("failed to spawn cargo build for fake-mcp-server fixture");
        assert!(
            status.success(),
            "pre-building the fake-mcp-server fixture failed"
        );
    });
}

fn fake_server_config() -> StdioMcpServerConfig {
    let manifest = fake_manifest();
    StdioMcpServerConfig {
        command: "cargo".into(),
        args: vec![
            "run".into(),
            "--quiet".into(),
            "--manifest-path".into(),
            manifest.display().to_string(),
        ],
        env: BTreeMap::from([("FAKE_SECRET".into(), "super-secret-token".into())]),
        cwd: None,
        startup_timeout: Some(Duration::from_secs(5)),
        request_timeout: Some(Duration::from_secs(5)),
    }
}

async fn fake_client(config: StdioMcpServerConfig) -> McpClient {
    ensure_fake_server_built();
    McpClient::connect(
        "fake",
        &McpServerConfig::Stdio(config),
        McpLimits::default(),
        vec!["super-secret-token".into()],
    )
    .await
    .unwrap()
}

#[tokio::test]
// Stdio transport starts the fake server and completes initialize.
async fn starts_fake_server_and_initializes() {
    let mut client = fake_client(fake_server_config()).await;
    let init = client.initialize().await.unwrap();
    assert_eq!(init.server_info.name, "fake-mcp-server");
    client.shutdown().await.unwrap();
}

#[tokio::test]
// The lifecycle sends notifications/initialized and discovers paginated tools.
async fn initialized_notification_then_tools_list() {
    let mut client = fake_client(fake_server_config()).await;
    client.initialize().await.unwrap();
    let tools = client.list_tools().await.unwrap();
    let names: Vec<_> = tools.into_iter().map(|t| t.name).collect();
    assert_eq!(names, vec!["list_issues", "get_issue", "search_docs"]);
    client.shutdown().await.unwrap();
}

#[tokio::test]
// tools/call sends the tool name and arguments and maps a text result.
async fn tools_call_returns_text_result() {
    let mut client = fake_client(fake_server_config()).await;
    client.initialize().await.unwrap();
    let result = client
        .call_tool("get_issue", serde_json::json!({"id": 1}))
        .await
        .unwrap();
    assert!(!result.is_error);
    assert_eq!(result.content.len(), 1);
    client.shutdown().await.unwrap();
}

#[tokio::test]
// Server stderr is captured as logs, capped, and secrets are redacted.
async fn stderr_is_logged_and_redacted() {
    let mut client = fake_client(fake_server_config()).await;
    client.initialize().await.unwrap();
    // The server's stderr line is captured asynchronously. Instead of sleeping
    // a fixed interval and hoping it arrived, poll-drain (accumulating across
    // drains, since `drain_logs` consumes) until the redacted line shows up,
    // bounded by an overall deadline so a real failure still fails loudly.
    let mut logs = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        logs.extend(client.drain_logs());
        if logs.iter().any(|l| l.message.contains("[REDACTED]")) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "server stderr was never captured as a log; got: {logs:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(logs.iter().any(|l| l.message.contains("[REDACTED]")));
    assert!(!logs
        .iter()
        .any(|l| l.message.contains("super-secret-token")));
    client.shutdown().await.unwrap();
}

#[tokio::test]
// Invalid stdout from a server returns a structured transport error.
async fn invalid_stdout_is_rejected() {
    let mut cfg = fake_server_config();
    cfg.env
        .insert("FAKE_MCP_MODE".into(), "invalid-json".into());
    let mut client = fake_client(cfg).await;
    let err = client.initialize().await.unwrap_err();
    assert!(matches!(err, McpError::Transport(_)));
}

#[tokio::test]
// Startup timeout applies to the initialize lifecycle request.
async fn startup_timeout_works() {
    let mut cfg = fake_server_config();
    cfg.env.insert("FAKE_MCP_MODE".into(), "sleep".into());
    cfg.startup_timeout = Some(Duration::from_millis(50));
    let mut client = fake_client(cfg).await;
    let err = client.initialize().await.unwrap_err();
    assert!(matches!(err, McpError::Timeout { .. }));
}

#[tokio::test]
// Resources and prompts are available for explicit inspection.
async fn resources_and_prompts_work() {
    let mut client = fake_client(fake_server_config()).await;
    client.initialize().await.unwrap();
    assert_eq!(client.list_resources().await.unwrap().len(), 1);
    assert_eq!(
        client.read_resource("fake://docs/parser").await.unwrap()[0]
            .text
            .as_deref(),
        Some("Parser docs stay offline.")
    );
    assert_eq!(client.list_prompts().await.unwrap().len(), 1);
    client.shutdown().await.unwrap();
}
