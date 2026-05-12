//! Plan-then-act: generate a plan, then act on it.

use std::sync::Arc;

use code_agent_sdk::fake_provider::EchoFakeProvider;
use code_agent_sdk::{
    ActRequest, AgentRuntime, InMemoryWorkspace, PatchPolicy, ProviderConfig, SdkPlanRequest,
    WorkspaceSource,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let workspace = InMemoryWorkspace::from_iter(vec![("README.md", b"old\n".to_vec())])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace.clone()))
        .build()
        .await?;
    let plan_result = runtime
        .plan(SdkPlanRequest::new("Add a TOC to README"))
        .await?;
    println!(
        "plan {} created with {} steps",
        plan_result.plan.id,
        plan_result.plan.steps.len()
    );
    let act_result = runtime
        .act(
            ActRequest::from_plan(plan_result.plan)
                .workspace(WorkspaceSource::in_memory(workspace))
                .patch_policy(PatchPolicy::ReturnOnly)
                .test_force_approval(),
        )
        .await?;
    println!("act status: {:?}", act_result.status);
    Ok(())
}
