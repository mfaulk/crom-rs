# Quickstart

Add the SDK to your `Cargo.toml`:

```toml
[dependencies]
crom-sdk = { path = "path/to/crates/crom-sdk", features = ["fake-provider"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Run a single coding task with the deterministic fake provider:

```rust,no_run
use std::sync::Arc;

use crom_sdk::fake_provider::EchoFakeProvider;
use crom_sdk::{
    AgentRequest, AgentRuntime, InMemoryWorkspace, ProviderConfig, WorkspaceSource,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let workspace = InMemoryWorkspace::from_iter(vec![
        ("README.md", b"hello\n".to_vec()),
    ])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let result = runtime
        .run_once(AgentRequest::new("describe the project"))
        .await?;
    println!("status: {:?}", result.status);
    println!("summary: {}", result.summary);
    Ok(())
}
```

Drop the fake provider for an Anthropic-backed run:

```rust,no_run
let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::anthropic_api_key(std::env::var("ANTHROPIC_API_KEY")?.into()))
    .workspace(WorkspaceSource::filesystem("./repo"))
    .build()
    .await?;
```

The runtime's defaults are conservative: read-only side effects, no Bash, no
plugins, no MCP, no global keychain lookup. See [`safety.md`](safety.md)
for what each opt-in widens.

For a streamed run with cancellation, use `runtime.run(...)` instead of
`run_once`. See [`events.md`](events.md).

CLI users can drive the SDK without touching their app code via the
`crom headless-run` subcommand.
