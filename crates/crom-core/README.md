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
- **Tools** — the `Tool` trait, `ToolRegistry`, and `ToolCatalog` for
  exposing local and MCP tools, including tool-search filtering.
- **Subagents** — `AgentRegistry`, `AgentDefinition`, and the `SubagentRuntime`
  for spawning child agents.
- **Policy & safety** — `permission`, `policy`, `hooks` (and `builtin_hooks`),
  used to gate side effects.
- **Supporting types** — conversations, events, settings, skills, slash
  commands, plans, todos, compaction, telemetry, and checkpoint traits.

## Usage

Depended on by every other crate in the workspace. Applications typically use
`crom-sdk` (embeddable) or the `crom` CLI rather than wiring the
runtime up directly.
