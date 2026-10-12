//! Demonstrate stateless runs: the SDK uses `NoopStorage` by default so no
//! events are persisted.

use std::sync::Arc;

use crom_sdk::fake_provider::EchoFakeProvider;
use crom_sdk::{AgentRequest, AgentRuntime, NoopStorage, ProviderConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .storage(Arc::new(NoopStorage))
        .build()
        .await?;
    let result = runtime.run_once(AgentRequest::new("hello")).await?;
    println!("status: {:?}", result.status);
    Ok(())
}
