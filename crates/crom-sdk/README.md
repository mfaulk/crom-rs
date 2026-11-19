# crom-sdk

Embeddable headless Rust SDK for crom.

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

`ToolPolicy.disallowed_tools` blocks matching tool calls regardless of
approvals; `allowed_tools` pre-approves matching calls. Malformed rules fail
`build()`.

`ToolPolicy::guard(GuardConnection::new(url, handle))` connects the run to a
Crom Guard agent endpoint as its only MCP server. Guard decides every
protected operation; Crom records each call at the `guard` audit layer with
Guard's decision id. `GuardConnection::with_approver` lets the trusted
application answer Guard's `approval_required` (Crom then retries the call
once with `_meta.approval_id`). See [`docs/sdk/guard.md`](../../docs/sdk/guard.md)
(`cargo test -p crom-sdk --features fake-provider --test guard_client`).

`RunBudgets` caps turns, tool calls, duration, patch size, cumulative
input/output tokens, and nominal cost (`with_max_cost_usd`, which needs
`AgentRuntimeBuilder::pricing`). A breach fails the run with a `budget.*`
diagnostic. See [`docs/sdk/safety.md`](../../docs/sdk/safety.md).

## Quick start

```rust
use crom_sdk::{
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

Provider integrations are Cargo features: `anthropic`, `openai`, and
`gemini` are on by default, so `ProviderConfig::anthropic_api_key`,
`openai_api_key`, and `gemini_api_key` build HTTP-backed providers directly.
`ProviderConfig::profile(profile, key)` builds any provider profile (e.g.
DeepSeek or a local OpenAI-compatible server) via
`crom_sdk::profile::build_profile_provider`, the same path the CLI and eval
use; see `docs/providers.md`.
`checkpointing` and `fake-provider` (for tests and examples) are opt-in.
Disabling a provider feature makes its config fail with
`provider.feature_disabled`. See `examples/` for runnable usage:

```sh
cargo run --example <name> --features fake-provider -p crom-sdk
```
