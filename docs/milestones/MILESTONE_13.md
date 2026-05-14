# Milestone 13 — Mature TUI Coding Agent Experience

**Theme:** Turn the existing beta TUI into a mature daily-use coding-agent
cockpit. The TUI consumes the M12 SDK event stream, exposes a multi-pane
layout with virtualized timeline / diff workbench / plan dashboard /
inspectors / session browser / diagnostics, and ships an offline
fake-provider demo with terminal-snapshot tests.

This document tracks M13 as it lands in phases. Each phase is its own
feature branch + PR. The whole milestone is "complete" only when every
phase has merged and the acceptance checklist at the bottom is satisfied.

## Phase status

| Phase | Scope | Status |
|-------|-------|--------|
| A | Live SDK-driven app shell: event loop, composer, transcript, status line; fake-provider end-to-end; `cmd_tui` no longer a stub | in progress |
| B | `TuiAppState` layout primitives, side / detail panel infra, modal stack | planned |
| C | Virtualized timeline, event grouping, comprehensive renderers, search / navigation | planned |
| D | Composer maturity: slash / `@file` / context completions, history search, multiline | planned |
| E | Diff / approval workbench + plan dashboard + deviation modal | planned |
| F | Inspectors: tools, MCP, agents, skills, plugins | planned |
| G | Session browser, recovery flow, auth / model / provider selectors, diagnostics panel | planned |
| H | Command palette, help overlay, keybindings, accessibility, activity panel | planned |
| I | New CLI subcommands, `examples/tui-demo`, UI telemetry events | planned |

## Phase A — Live SDK-driven app shell

### What's new

`crates/code-agent-tui` previously shipped only rendering primitives
(status line, transcript cards, approvals, diffs, plan view) plus a
`Fullscreen` / `Plain` renderer split. The CLI's `code-agent tui`
command was a stub that printed one preview frame and exited.

Phase A introduces the first interactive event loop:

| Module | Purpose |
|--------|---------|
| `state.rs` | `TuiAppState`, `TuiAction`, `TuiInbound`, `RunStatus`. Pure, synchronous reducer that takes inbound events (SDK events, keys, resize, stream errors / endings) and emits side-effect intents. No terminal or runtime I/O. |
| `event_adapter.rs` | `map_sdk_event` — total mapper from `code_agent_sdk::SdkEvent` to `EventCard` + activity hint + run-terminal marker. Unknown variants render as generic cards. |
| `app.rs` | `TuiApp` and `drive_single_prompt`. `TuiApp::run` enters raw-mode + alternate-screen, spawns a blocking task that decodes crossterm events into the composer's abstract `KeyEvent`, and runs a `tokio::select!` loop over input, the active `AgentRun` event stream, and a redraw tick. `TerminalGuard` restores the terminal on drop / panic. |

### Public API additions

```rust
use code_agent_tui::{TuiApp, TuiAppState, ComposerState, RunStatus};

let app = TuiApp::new(runtime, state);
app.run().await?;
```

The reducer is exported for tests / embedders that want to drive
state without owning a terminal:

```rust
let actions = state.apply(TuiInbound::Key(KeyEvent::Enter));
match actions.first() {
    Some(TuiAction::SubmitPrompt(p)) => { /* start a run */ }
    Some(TuiAction::CancelRun) => { /* cancel the active run */ }
    Some(TuiAction::Exit) => { /* tear down */ }
    None => {}
}
```

### CLI surface

`code-agent tui` now launches the live shell when stdout is a TTY and
fullscreen rendering is selected. Flags:

| Flag | Behavior |
|------|----------|
| `--snapshot` | Existing M9 behavior: print one preview frame and exit. |
| `--renderer plain` | Existing M9 plain fallback. |
| `--renderer fullscreen` | Force the live shell even when settings prefer plain. |
| `--session <id>` | Existing M9 wiring; deeper recovery lands in Phase G. |
| `--fake-provider` | New: use the SDK's offline echo fake provider — handy for tests / demos and the only path with no real account required. |
| `--model <id>` | New: seed the SDK runtime with a specific model id. |

When neither `--fake-provider` nor `ANTHROPIC_API_KEY` is set the CLI
prints a one-line preview frame and exits with a clear hint instead of
launching an empty cockpit. Real provider routing for OAuth / OpenAI /
Gemini is intentionally deferred to a later phase; the existing
`code-agent run` / `code-agent headless-run` paths already cover those
cases.

### Event coverage

The `event_adapter` maps every `SdkEvent` variant the SDK emits today,
including `RunStarted` / `PlanningStarted` / `PlanCreated` /
`PlanApproved` / `ActStarted` / `AssistantMessage` /
`AssistantTextDelta` (coalesced into a single assistant card) /
`ToolCallStarted` / `ToolCallCompleted` / `ApprovalRequired` /
`ApprovalResolved` / `PatchProposed` / `FileGenerated` /
`ValidationStarted` / `ValidationCompleted` / `Diagnostic` /
`RunCompleted` / `RunFailed` / `RunCancelled` / `Internal`. The
matcher is exhaustive: when the SDK adds a new variant the TUI fails
to compile rather than silently dropping the event.

### Tests

`crates/code-agent-tui/src/state.rs` adds 9 unit tests covering submit,
run start, run completion, run failure, delta coalescing, activity
capacity, Ctrl+C semantics, stream error recovery, resize, and
transcript bounding. `event_adapter.rs` adds 5 unit tests covering the
most-trafficked variants. `tests/live_shell.rs` adds 3 end-to-end
tests that build a real `AgentRuntime` with the SDK's `EchoFakeProvider`,
drive the reducer through `drive_single_prompt`, and assert that the
resulting state contains user + agent text — and renders to a
ratatui `TestBackend` frame that includes the project name, the
prompt, and the echo reply.

All M1–M12 tests continue to pass.

### Safety / resilience notes

* `TerminalGuard` enters raw mode + alternate screen on construction
  and disables raw mode + leaves alternate screen + disables bracketed
  paste on drop. The drop is best-effort and runs even on panic, so
  panics don't strand the terminal.
* The input thread uses `tokio::task::spawn_blocking` so `crossterm::event::read`
  doesn't starve the runtime.
* `tokio::select! { biased; ... }` favors input over SDK events over
  ticks, so keystrokes never lag a busy event stream.
* The transcript and activity log are bounded (1024 and 16 entries
  respectively) so long sessions stay responsive.
* `Ctrl+C` is overloaded: cancel an in-flight run if one exists, else
  exit. `Ctrl+D` exits when the composer is empty.
* Bracketed-paste data is fed to the composer one character at a time
  so embedded newlines never accidentally submit mid-paste.

### Out of scope for Phase A

* Multi-pane layout (side / detail panel, modal stack) — Phase B.
* Event grouping, virtualized scrollback, transcript search — Phase C.
* Slash / `@file` / context-insertion autocomplete — Phase D.
* Diff / approval workbench, plan dashboard — Phase E.
* Tools / MCP / agents / skills / plugins inspectors — Phase F.
* Session browser, recovery, model / provider / auth selectors — Phase G.
* Command palette, help overlay, accessibility hardening — Phase H.
* `examples/tui-demo`, new CLI subcommands, UI telemetry — Phase I.
* Real provider routing in `code-agent tui` beyond fake / Anthropic-API-key.
* OpenTelemetry export, theme system, VS Code / JetBrains / web / desktop
  surfaces (excluded for the whole milestone).
