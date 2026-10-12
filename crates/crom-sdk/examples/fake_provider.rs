//! Use the deterministic `EchoFakeProvider` to drive a run with no network.

use std::sync::Arc;

use crom_sdk::fake_provider::EchoFakeProvider;
use crom_sdk::{AgentRequest, AgentRuntime, ProviderConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await?;
    let result = runtime.run_once(AgentRequest::new("hello world")).await?;
    println!("provider: {}", result.provider.as_str());
    println!("response: {}", result.summary);
    Ok(())
}
