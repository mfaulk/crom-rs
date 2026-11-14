# Milestone 2 — Control Plane + Interactive Runtime

**Theme:** Turn the M1 one-shot agent loop into a controllable, long-lived
runtime with hooks, a permission policy engine, settings, telemetry, and
richer session/CLI surfaces.

## Long-lived `AgentClient`

Builder-based client backed by an `mpsc` command channel and a `CancelHandle`
(atomic flag + `Notify`) so interrupts propagate into an awaiting provider call.

API:

- `send_user_message(...)` — queued or interleaved while the agent is busy
- `next_event(...)`
- `interrupt()`
- `set_permission_mode(...)`
- `shutdown()` — clean shutdown that propagates through an active run

User prompts are persisted to history before spawning so assistant/tool-result
events land after the prompt.

## Hook system

Events: `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`PostToolBatch`, `Stop`.

Decisions merge with priority `Deny > Ask > Allow > AddContext > Continue`.
`PreToolUse` `Ask` forces the callback even under `acceptEdits` / read-only
auto-allow paths.

Built-in hooks (in `crom_core::builtin_hooks`):

- `BlockEnvFileWritesHook`
- `BlockDangerousBashHook`
- `MaxPatchSizeHook`
- `AuditLogHook`
- `SecretRedactionHook`

## Permission policy engine

Formal evaluation order:

```
hooks → disallow rules → permission mode → allow rules → callback → default deny
```

Typed `ToolRule`: `ExactTool`, `ToolGlob`, `BashCommandRegex`, `FilePathGlob`.
`dontAsk` honors both structured `allowed_tool_rules` and the simple-name
`allowed_tools` list (preserving the M1 contract).

## Settings

Project (`<cwd>/.agent/settings.json`) and user
(`~/.config/crom/settings.json`) layers. Project overrides user on
scalars; list-typed fields concatenate.

CLI flags include `--settings`, `--config`, `--yes-to`, `--deny`,
`--log-format pretty|json`, `--show-usage`.

## Telemetry

Structured events for model requests, tool execution, hook evaluation,
permission evaluation, and agent interruption. Optional `ModelPricing` cost
estimation.

## Sessions

- New tool: `TodoWrite` with persistence and `TodoUpdate` events.
- New commands: `chat`, `continue`, `fork`, `sessions show`, `sessions export`.
- Fork-metadata records and a typed `SessionSummary`.
- `cmd_resume` verifies the session exists before executing (no silent
  fresh-start on a typo'd id).

## Approval UX

Richer prompt menu with allow-once / allow-for-session / allow-for-project
and deny-once / deny-for-session, Bash risk classification
(Low / Medium / High / Blocked), and a unified-diff preview for `Edit` / `Write`
via `similar`.

## Demo

`examples/policy-demo` — failing test, a `.env` file, `AGENTS.md`, and a
`.agent/settings.json` that disallows env-file writes and limits Bash to
`cargo (test|check|fmt|clippy)`. `examples/m2-e2e-demo.sh` drives the demo
through every M2 surface in one non-interactive run.

## Tests

94 tests pass; `cargo fmt` and `cargo clippy --workspace --all-targets` are
clean. M1's `query()` API and existing tests are preserved.
