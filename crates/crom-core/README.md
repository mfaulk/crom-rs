# crom-core

Core types, traits, and the agent runtime for crom.

This crate defines the contracts the rest of the workspace builds on and
hosts the agent loop itself. It does not know about any specific model
provider, terminal, or storage backend — those are supplied by other crates.

## What's here

- **Agent runtime** — `Agent`, `AgentBuilder`, and `query` drive the
  conversation loop: send a request, stream `ModelEvent`s, dispatch tool
  calls, and apply hooks and permission checks.
- **Provider contract** — `ModelProvider`, `ModelRequest`, `ModelEvent`,
  `TokenUsage`. Implemented by `crom-anthropic`,
  `crom-provider-openai`, and `crom-provider-gemini`.
- **Provider errors & retries** — typed `ProviderError` classification and
  the shared `provider_retry` policy (`RetryPolicy`, `retry_provider_call`,
  `with_retry_observer`) used by all three providers. See
  [`docs/providers.md`](../../docs/providers.md).
- **Usage accounting** — `TokenUsage` fields are `Option<u64>` (`None` =
  unreported, never zero); `UsageTotals` sums round trips strictly (`None` if
  any turn is unknown). The loop emits `AgentEvent::ModelTurnCompleted` per
  round trip. Semantics: [`docs/sdk/events.md`](../../docs/sdk/events.md#token-usage).
- **Tools** — the `Tool` trait, `ToolRegistry`, and `ToolCatalog` for
  exposing local and MCP tools, including tool-search filtering.
- **Subagents** — `AgentRegistry`, `AgentDefinition`, and the `SubagentRuntime`
  for spawning child agents.
- **Policy & safety** — `permission`, `policy`, `hooks` (and `builtin_hooks`),
  used to gate side effects.
- **Provider profiles** — `provider_profile` resolves a provider id to its
  kind, endpoint, API-key env var, default model, and capability flags from
  the checked-in `providers.toml` plus settings `providers` overrides. See
  `docs/providers.md`.
- **Supporting types** — conversations, events, settings, skills, slash
  commands, plans, todos, compaction, telemetry, and checkpoint traits.

## Usage

Depended on by every other crate in the workspace. Applications typically use
`crom-sdk` (embeddable) or the `crom` CLI rather than wiring the
runtime up directly.
