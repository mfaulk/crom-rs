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
| A | Live SDK-driven app shell: event loop, composer, transcript, status line; fake-provider end-to-end; `cmd_tui` no longer a stub | PR #103 (under review) |
| B | `TuiAppState` layout primitives, side / detail panel infra, modal stack, multi-pane renderer, slash routing | PR #104 (under review) |
| C | Card identity, selection + expansion + transcript search, n/N match navigation | in progress |
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

## Phase B — Layout primitives, side / detail panels, modal stack

### What's new

| Module | Purpose |
|--------|---------|
| `layout.rs` | Pure layout state: `FocusPane`, `SidePanelMode` (14 variants), `DetailPanelMode` (10 variants), `SidePanelState`, `DetailPanelState`, `TuiModal`, `LayoutMode` (Full / Compact / Tiny), `LayoutState`. Reducer-friendly types only — content rendering lives in `view.rs`. |
| `view.rs` | Adapter that builds a `UiFrame` from `TuiAppState`. Each `SidePanelMode` / `DetailPanelMode` resolves to a placeholder pane title + body for Phase B; later phases (E, F, G, H) replace the placeholder bodies with real plan / inspector / session / palette content. |
| `fullscreen.rs` | Extended `UiFrame` with `layout_mode`, `side_panel`, `detail_panel`, `modal`, `composer_focused`, `transcript_focused`. The renderer paints a responsive multi-pane layout: transcript + sidebar across the top, detail panel below (Full); single-column M9 layout (Compact); transcript + composer only (Tiny). Modal overlays paint last, centered via a `Clear` + bordered `List`. |
| `state.rs` | Now owns a `LayoutState`. The reducer intercepts `Tab` / `Shift+Tab` for pane cycling, pops the topmost modal on `Esc` when one is open, and routes `/foo` slash commands into the layout state instead of submitting a prompt. New helpers: `set_side_panel`, `set_detail_panel`, `push_modal`, `pop_modal`. |
| `composer.rs` | New `KeyEvent::ShiftTab` variant (no-op in the composer, consumed before composer dispatch by the reducer). |

### Slash commands (Phase B scope)

| Command | Action |
|---------|--------|
| `/plan` `/changes` `/files` `/tools` `/sessions` `/diagnostics` `/agents` (alias `/subagents`) `/mcp` `/plugins` `/skills` `/usage` `/auth` `/model` `/provider` | Switch the side panel mode and focus it. |
| `/help` `/keys` | Open the Help modal. |
| `/palette` | Open the Command Palette modal (content arrives in Phase H). |
| `/search` | Open the Transcript Search modal (content arrives in Phase C). |
| `/history` | Open the Prompt History modal (content arrives in Phase D). |
| `/toggle-side` `/side` | Toggle side panel visibility. |
| `/toggle-detail` `/detail` | Toggle detail panel visibility. |
| Unknown `/foo` | Logged to the activity panel; no prompt is submitted. |

### Responsive layout thresholds

| Mode | Min width × height | Renders |
|------|---------------------|---------|
| Full | 100 × 18 | status + (transcript ┃ side) + detail + diagnostics + composer + footer |
| Compact | 60 × 10 | status + transcript + diagnostics + composer + footer (no side/detail) |
| Tiny | smaller | transcript + composer only (no status, footer, or panels) |

The renderer additionally re-evaluates the responsive mode against the actual terminal `Rect` so a state that says "Full" still degrades gracefully if the terminal shrinks between resize events.

### Keyboard

| Key | Action |
|-----|--------|
| `Tab` | Focus the next visible pane. |
| `Shift+Tab` | Focus the previous visible pane. |
| `Esc` | Close the topmost modal if one is open. (Composer Esc stays a no-op.) |

### Tests

`layout.rs` adds 6 unit tests (responsive thresholds, resize preserves selection, focus skip-invisible cycle, side-panel mode round-trip, modal stack LIFO, panel suppression in compact/tiny). `state.rs` adds 6 reducer tests (Tab/Shift+Tab cycle, Esc modal pop, slash panel switch, /help, unknown-slash diagnostic, resize-recomputes-mode). `view.rs` adds 4 tests (default state → Plan side panel, hidden panel omits pane, modal title stability, detail placeholder bodies non-empty). `fullscreen.rs` adds 4 snapshot tests (full multi-pane render, modal overlay, Tiny drops chrome, Full-on-tiny-terminal degrades safely). The existing M9 + M13 Phase A tests continue to pass.

All M1–M12 tests still pass; `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` (958 tests total) are clean.

### Out of scope for Phase B

* Real content for any side / detail panel — those land in their owning phase (E, F, G, H).
* Real command palette content — Phase H.
* Transcript search behavior beyond opening the modal — Phase C.
* `@file` autocomplete in the composer — Phase D.

## Phase C — Card identity, selection / expansion / search

### What's new

| Module | Purpose |
|--------|---------|
| `transcript.rs` | `EventCard` gains stable `id: u64`, optional `expanded_body: Vec<String>`, optional `source: Option<String>`. New `lines_with_expansion(bool)` and `matches_search(&str)` helpers. Derives `Default` so callers can opt into the new fields via `..Default::default()`. |
| `state.rs` | `TuiAppState` gains `next_card_id`, `selected_card_id`, `expanded_card_ids: HashSet<u64>`, `search_query: Option<String>`, `search_input: String`. New helpers `visible_card_indices()` and `is_card_expanded()`. `push_transcript` stamps each card with a fresh id. New `apply_modal_key` (modal-aware key router) and `apply_transcript_nav_key` (transcript-focused navigation). |
| `view.rs` | Builds the renderer transcript via a `projected_transcript` step that applies the search filter, inlines each expanded card's `expanded_body`, and marks the selected card with a `›` glyph (and other cards with a leading `  `). The SearchTranscript modal body reflects the in-progress `search_input`, the committed `search_query`, and a `visible of total` match count. |

### Reducer behavior

Transcript-focused navigation (only fires when `FocusPane::Transcript`):

| Key | Action |
|-----|--------|
| `j` / `Down` | Move selection down by one. Saturates at the end. |
| `k` / `Up` | Move selection up by one. Saturates at the start. |
| `g` | Jump selection to the first visible card. |
| `G` | Jump selection to the last visible card. |
| `e` / `Enter` | Toggle expansion of the selected card. |
| `n` | Next search match (no-op without an active query). |
| `N` | Previous search match. |

Modal-aware input (when the modal stack is non-empty):

| Modal | Char / Backspace / Enter behavior |
|-------|------------------------------------|
| `SearchTranscript` | Char appends to `search_input`; Backspace pops; Enter commits to `search_query` and jumps selection to the first match; Esc cancels and clears the input. |
| any other | All input is swallowed except `Esc` (close) and `Ctrl+C` (cancel run). |

### Card identity and search semantics

* `id` is assigned by `push_transcript` from a per-state monotonic counter (`next_card_id`). Wraps via `wrapping_add` for safety, though 2^64 cards is well beyond realistic use.
* Selection and expansion sets are keyed by id rather than index, so they survive ring-buffer eviction (cards age out of `transcript` while their ids persist in the sets — helpers ignore ids that no longer resolve).
* `matches_search` is case-insensitive and searches headline + body + expanded_body + source. The empty string normalizes to "no filter".
* `visible_card_indices()` returns the indices into `self.transcript` that the renderer should display; without a query it returns `0..len()`.

### Tests

`state.rs` adds 8 reducer tests:
`push_transcript_assigns_monotonic_ids`, `transcript_nav_j_k_moves_selection`, `transcript_nav_inert_when_composer_focused`, `e_toggles_expansion`, `search_modal_filters_transcript`, `search_modal_backspace_and_cancel`, `n_and_capital_n_navigate_matches`, plus an updated `transcript_is_bounded` that exercises Default-based literal construction.

`view.rs` tests still pass and now exercise the projected-transcript path (selection glyph, expansion inlining).

All M1–M12 tests still pass; `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` (972 tests total) are clean.

### Out of scope for Phase C

* Scrollbar / overflow indicators in the renderer (the existing `Paragraph::wrap` does soft wrapping; explicit virtualization beyond bounded `transcript` size lands when the inspectors in Phase F need it).
* Rich per-event-type renderers (Bash stdout/stderr/exit grouping, MCP arg/result preview, subagent child links) — these need richer SDK event payloads first; Phase C's `expanded_body` and `source` fields are the seam those phases will fill.
* Copy-selected-event-text via OSC52 — Phase H.
* Raw-JSON detail-pane toggle — Phase E/F (depends on richer detail panel content).
* Slash / `@file` / context-insertion autocomplete — Phase D.
* Diff / approval workbench, plan dashboard — Phase E.
* Tools / MCP / agents / skills / plugins inspectors — Phase F.
* Session browser, recovery, model / provider / auth selectors — Phase G.
* Command palette, help overlay, accessibility hardening — Phase H.
* `examples/tui-demo`, new CLI subcommands, UI telemetry — Phase I.
* Real provider routing in `code-agent tui` beyond fake / Anthropic-API-key.
* OpenTelemetry export, theme system, VS Code / JetBrains / web / desktop
  surfaces (excluded for the whole milestone).
