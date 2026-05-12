//! Build an in-memory workspace from a file map and run the SDK against it.

use std::sync::Arc;

use code_agent_sdk::fake_provider::EchoFakeProvider;
use code_agent_sdk::{
    AgentRequest, AgentRuntime, InMemoryWorkspace, ProviderConfig, WorkspaceSource,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let workspace = InMemoryWorkspace::from_pairs(vec![
        ("Cargo.toml", b"[package]\nname=\"x\"\n".to_vec()),
        ("src/main.rs", b"fn main(){}\n".to_vec()),
    ])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let result = runtime
        .run_once(AgentRequest::new("read the Cargo.toml"))
        .await?;
    println!("{}", result.summary);
    Ok(())
}
