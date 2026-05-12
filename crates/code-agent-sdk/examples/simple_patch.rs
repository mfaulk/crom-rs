//! Run a patch-only generation against a tiny fixture workspace.
//!
//! `cargo run --example simple_patch --features fake-provider -p code-agent-sdk`

use std::sync::Arc;

use code_agent_sdk::fake_provider::EchoFakeProvider;
use code_agent_sdk::{
    AgentMode, AgentRequest, AgentRuntime, InMemoryWorkspace, PatchPolicy, ProviderConfig,
    WorkspaceSource,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let workspace = InMemoryWorkspace::from_iter(vec![("README.md", b"# project\n".to_vec())])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let result = runtime
        .run_once(
            AgentRequest::new("Suggest an improvement to README")
                .mode(AgentMode::GeneratePatch)
                .patch_policy(PatchPolicy::ReturnOnly),
        )
        .await?;
    println!("status: {:?}", result.status);
    if let Some(patch) = result.patch {
        println!("patch unified diff:\n{}", patch.unified_diff);
    }
    Ok(())
}
