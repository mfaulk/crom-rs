# code-agent-sdk

Embeddable headless Rust SDK for code-agent.

Exposes a stable, typed Rust API that applications can use to generate code,
produce and apply patches, run validation, and stream progress events —
without depending on the CLI, TUI, terminal prompts, global credentials, or
filesystem settings files.

## Safe defaults

The default configuration is conservative; applications opt into side effects
explicitly via `AgentRuntimeBuilder`:

- read-only side-effect policy (no Bash, no Edit/Write)
- `InteractivityMode::HeadlessDeny` (no approval prompts)
- no global credential lookup
- no plugin / MCP / skill loading
- in-memory or no-op storage
- patch-only output (`PatchPolicy::ReturnOnly`)

## Quick start

```rust
use code_agent_sdk::{
    AgentRequest, AgentRuntime, ProviderConfig, fake_provider::EchoFakeProvider,
};
use std::sync::Arc;

let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider::default())))
    .build()
    .await?;
let result = runtime.run_once(AgentRequest::new("describe the workspace")).await?;
println!("summary = {}", result.summary);
```

## Features

Providers and checkpointing are opt-in via Cargo features: `anthropic`,
`openai`, `gemini`, `checkpointing`, and `fake-provider` (for tests and
examples). See `examples/` for runnable usage:

```sh
cargo run --example <name> --features fake-provider -p code-agent-sdk
```
