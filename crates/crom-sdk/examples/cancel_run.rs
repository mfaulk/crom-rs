//! Cancel a streaming run via the cancellation handle.

use std::sync::Arc;
use std::time::Duration;

use crom_sdk::fake_provider::EchoFakeProvider;
use crom_sdk::{AgentRequest, AgentRuntime, ProviderConfig};
use futures::StreamExt;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await?;
    let mut handle = runtime
        .run(AgentRequest::new("slow task").deadline(Duration::from_secs(30)))
        .await?;
    handle.cancellation.cancel();
    while handle.events.next().await.is_some() {}
    let result = handle.final_result().await?;
    println!("final status after cancel: {:?}", result.status);
    Ok(())
}
