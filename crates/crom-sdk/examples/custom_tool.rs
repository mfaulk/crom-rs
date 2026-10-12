//! Register a host-supplied tool and let the fake provider call it.

use std::sync::Arc;

use async_trait::async_trait;
use crom_core::ToolError;
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::{
    AgentRequest, AgentRuntime, AllowAllApprovals, ApprovalPolicy, BuiltinToolSet, PermissionKind,
    ProviderConfig, SideEffectPolicy, Tool, ToolContext, ToolPolicy, ToolResult,
};

struct GreetingTool;

#[async_trait]
impl Tool for GreetingTool {
    fn name(&self) -> &str {
        "Greet"
    }
    fn description(&self) -> &str {
        "Returns a fixed greeting"
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }
    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text("hello from the host tool"))
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let provider = ScriptedFakeProvider::one_shot_tool_call("Greet", serde_json::json!({}));
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .approval_callback(Arc::new(AllowAllApprovals))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::read_only_defaults(),
            side_effect_policy: SideEffectPolicy::read_only(),
            custom_tools: vec![Arc::new(GreetingTool)],
            ..Default::default()
        })
        .build()
        .await?;
    let result = runtime
        .run_once(
            AgentRequest::new("Call the greeting tool")
                .approval_policy(ApprovalPolicy::CallbackRequired),
        )
        .await?;
    println!("custom tool result status: {:?}", result.status);
    Ok(())
}
