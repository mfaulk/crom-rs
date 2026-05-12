# code-agent SDK overview

`code-agent-sdk` exposes the coding agent as a stable, typed, async, headless
Rust SDK. Applications import the crate, build an [`AgentRuntime`], and call
[`run_once`] or [`run`] to produce code, patches, validations, and progress
events without touching the CLI, TUI, terminal, or global credentials.

## What the SDK delivers

- Typed entry points: `AgentRequest`, `AgentRunResult`, `SdkEvent`.
- Workspace abstraction: filesystem, in-memory, read-only, patch-only,
  overlay, or your own.
- First-class patches: `PatchSet` is the unit of code change. No need to
  parse assistant text.
- Pluggable host callbacks: approvals, credential resolution, event sinks,
  secret redaction, host tools.
- Conservative defaults: read-only side effects, headless-deny approvals,
  no global keychain lookup, no plugins, no MCP.
- Budgets and deadlines: cancel, time-box, and bound every run.
- Plan/act API: programmatic planning that mirrors the M11 plan-mode flow.
- Storage: noop, in-memory, or JSONL-on-disk; pluggable.

## What the SDK does not do

- No CLI dependencies. The SDK never imports the CLI crate.
- No TUI dependencies. The SDK never opens a terminal.
- No `stdin/stdout` prompts. Approvals flow through callbacks.
- No global credential lookup by default. Bring your own keys.
- No filesystem mutation by default. Patches are returned, not applied.
- No Bash execution by default. Opt in explicitly.
- No plugin/MCP/skill loading by default. Opt in via features.

## Feature flags

```toml
[features]
default = ["filesystem-workspace", "in-memory-workspace", "anthropic", "planning"]
filesystem-workspace = []
in-memory-workspace = []
anthropic            = []   # depends on code-agent-anthropic
fake-provider        = []
planning             = []
mcp                  = []
plugins              = []
skills               = []
subagents            = []
checkpointing        = []   # depends on code-agent-checkpoint
cli                  = []   # placeholder; SDK never depends on the CLI
tui                  = []   # placeholder; SDK never depends on the TUI
```

[`AgentRuntime`]: ../../crates/code-agent-sdk/src/runtime/mod.rs
[`run_once`]: ../../crates/code-agent-sdk/src/runtime/mod.rs
[`run`]: ../../crates/code-agent-sdk/src/runtime/mod.rs
