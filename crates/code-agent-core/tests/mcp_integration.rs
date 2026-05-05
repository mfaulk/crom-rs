use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use code_agent_core::{
    query, AgentEvent, AgentOptions, McpOptions, ModelEvent, ModelProvider, ModelRequest,
    PermissionMode, ProviderError, ResultSubtype, StdioMcpServerConfig, ToolRegistry, ToolRule,
};
use futures::stream::{self, BoxStream, StreamExt};

fn fake_mcp_options() -> McpOptions {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../code-agent-mcp/tests/fixtures/fake-mcp-server/Cargo.toml");
    McpOptions {
        enabled: true,
        servers: BTreeMap::from([(
            "fake".into(),
            code_agent_mcp::McpServerConfig::Stdio(StdioMcpServerConfig {
                command: "cargo".into(),
                args: vec![
                    "run".into(),
                    "--quiet".into(),
                    "--manifest-path".into(),
                    manifest.display().to_string(),
                ],
                env: BTreeMap::new(),
                cwd: None,
                startup_timeout: Some(Duration::from_secs(5)),
                request_timeout: Some(Duration::from_secs(5)),
            }),
        )]),
        ..McpOptions::default()
    }
}

#[derive(Clone)]
struct CapturingProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    tool_names: Arc<Mutex<Vec<String>>>,
}

impl CapturingProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            tool_names: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl ModelProvider for CapturingProvider {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        *self.tool_names.lock().unwrap() = req.tools.iter().map(|t| t.name.clone()).collect();
        let events = self.turns.lock().unwrap().remove(0);
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

#[tokio::test]
// Discovered MCP tools are registered with `mcp__server__tool` names.
async fn discovered_mcp_tools_register_as_agent_tools() {
    let provider = Arc::new(CapturingProvider::new(vec![vec![
        ModelEvent::TextDelta {
            text: "done".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]]));
    let options = AgentOptions {
        mcp: fake_mcp_options(),
        allowed_tools: vec!["mcp__fake__list_issues".into()],
        allowed_tool_rules: vec![ToolRule::ExactTool("mcp__fake__list_issues".into())],
        max_turns: Some(1),
        ..AgentOptions::default()
    };
    let stream = query("hello", options, provider.clone(), ToolRegistry::new())
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(provider
        .tool_names
        .lock()
        .unwrap()
        .contains(&"mcp__fake__list_issues".to_string()));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::McpServerConnected(_))));
}

#[tokio::test]
// MCP tool calls flow through the normal agent loop and return tool results.
async fn mcp_tool_call_runs_through_agent_loop() {
    let provider = Arc::new(CapturingProvider::new(vec![
        vec![
            ModelEvent::ToolCall(code_agent_core::ToolCall {
                id: "toolu_1".into(),
                name: "mcp__fake__list_issues".into(),
                input: serde_json::json!({}),
            }),
            ModelEvent::MessageStop {
                stop_reason: Some("tool_use".into()),
                usage: None,
            },
        ],
        vec![
            ModelEvent::TextDelta {
                text: "summarized".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ],
    ]));
    let options = AgentOptions {
        mcp: fake_mcp_options(),
        allowed_tools: vec!["mcp__fake__list_issues".into()],
        allowed_tool_rules: vec![ToolRule::ExactTool("mcp__fake__list_issues".into())],
        permission_mode: PermissionMode::Default,
        max_turns: Some(3),
        ..AgentOptions::default()
    };
    let stream = query("use mcp", options, provider, ToolRegistry::new())
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert!(events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::UserToolResult(r)
                if r.tool_name == "mcp__fake__list_issues"
                    && r.content.contains("Parser trims too late")
        )
    }));
    assert!(events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::Result(r) if matches!(r.subtype, ResultSubtype::Success)
        )
    }));
}
