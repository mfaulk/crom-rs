//! Host approval callback: allow or deny based on hostside logic.

use std::sync::Arc;

use async_trait::async_trait;
use code_agent_sdk::fake_provider::ScriptedFakeProvider;
use code_agent_sdk::{
    AgentRequest, AgentRuntime, ApprovalCallback, ApprovalDecision, ApprovalPolicy,
    ApprovalRequest, BuiltinToolSet, ProviderConfig, SideEffectPolicy, ToolPolicy,
};

/// Allows reads, denies everything else.
struct ReadsOnly;

#[async_trait]
impl ApprovalCallback for ReadsOnly {
    async fn approve(
        &self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, code_agent_sdk::AgentSdkError> {
        if request.tool_name == "Read" {
            Ok(ApprovalDecision::Allow)
        } else {
            Ok(ApprovalDecision::deny("only reads allowed"))
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let provider = ScriptedFakeProvider::one_shot_tool_call(
        "Read",
        serde_json::json!({"file_path": "README.md"}),
    );
    let workspace =
        code_agent_sdk::InMemoryWorkspace::from_iter(vec![("README.md", b"hello\n".to_vec())])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .workspace(code_agent_sdk::WorkspaceSource::in_memory(workspace))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::read_only_defaults(),
            side_effect_policy: SideEffectPolicy::read_only(),
            ..Default::default()
        })
        .approval_callback(Arc::new(ReadsOnly))
        .build()
        .await?;
    let result = runtime
        .run_once(AgentRequest::new("read it").approval_policy(ApprovalPolicy::CallbackRequired))
        .await?;
    println!("status: {:?}", result.status);
    println!(
        "approvals requested: {}",
        result.events_summary.approvals_requested
    );
    Ok(())
}
