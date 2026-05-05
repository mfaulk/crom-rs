use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use code_agent_mcp::{McpClient, McpError, McpLimits, McpServerConfig, StdioMcpServerConfig};

fn fake_server_config() -> StdioMcpServerConfig {
    let manifest =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-mcp-server/Cargo.toml");
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
    tokio::time::sleep(Duration::from_millis(50)).await;
    let logs = client.drain_logs();
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
