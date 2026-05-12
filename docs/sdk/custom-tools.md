# Custom tools

Hosts register their own tools through `ToolPolicy::custom_tools`. Every
tool implements the same `code_agent_core::Tool` trait that ships with the
built-ins.

```rust,no_run
use std::sync::Arc;
use async_trait::async_trait;
use code_agent_core::{ToolError, ToolResult};
use code_agent_sdk::{
    AgentRuntime, BuiltinToolSet, PermissionKind, ProviderConfig, SideEffectPolicy,
    Tool, ToolContext, ToolPolicy,
};
use code_agent_sdk::fake_provider::EchoFakeProvider;

struct PingTool;

#[async_trait]
impl Tool for PingTool {
    fn name(&self) -> &str { "Ping" }
    fn description(&self) -> &str { "Returns 'pong'" }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object", "properties": {} })
    }
    fn permission_kind(&self) -> PermissionKind { PermissionKind::ReadOnly }
    async fn execute(&self, _: serde_json::Value, _: ToolContext) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text("pong"))
    }
}

# async fn run() -> anyhow::Result<()> {
let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
    .tool_policy(ToolPolicy {
        enable_builtin_tools: BuiltinToolSet::read_only_defaults(),
        side_effect_policy: SideEffectPolicy::read_only(),
        custom_tools: vec![Arc::new(PingTool)],
        ..Default::default()
    })
    .build()
    .await?;
# Ok(())
# }
```

Custom tools participate in:

- Permission gating via `permission_kind()`.
- Side-effect policy enforcement.
- Approval callbacks (when `ApprovalPolicy::CallbackRequired`).
- The tool catalog (visible to `SearchTools` if enabled).

Use `ToolHost` for purely host-managed tools that need richer context
(tenant ids, request ids, metadata) than `ToolContext` carries.
