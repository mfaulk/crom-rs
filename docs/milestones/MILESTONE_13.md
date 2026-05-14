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
| C | Card identity, selection + expansion + transcript search, n/N match navigation | PR #105 (under review) |
| D | Composer popup: slash / `@token` autocomplete, batched-paste handling, popup overlay renderer | PR #106 (under review) |
| E | Diff / approval workbench + plan dashboard wired to SDK events | in progress |
| F | Inspectors: tools, MCP, agents, skills, plugins | in progress |
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

## Phase D — Composer popup + paste handling

### What's new

| Module | Purpose |
|--------|---------|
| `completion.rs` (new) | Pure popup-state types and trigger detection. `CompletionKind` (Slash / ContextToken / File), `Candidate { display, insert_text }`, `CompletionPopup { kind, candidates, token_start, token_end, selected }`. `detect_trigger(buffer, cursor) -> Option<TriggerSpec>` inspects only the buffer + cursor — no async, no I/O — so it's safe to call after every keystroke. Catalogs: `SLASH_COMMANDS` (kept in sync with `state::apply_slash_command`) and `CONTEXT_TOKENS` (`@plan`, `@changed`, `@last-error`, `@last-diff`, `@checkpoint`, `@session`, `@tool`, `@agent`, `@skill`, `@plugin`, `@mcp`). |
| `state.rs` | `TuiInbound::Paste(String)` for bracketed-paste payloads. `TuiAppState.composer_popup: Option<CompletionPopup>`. `refresh_composer_popup()` runs after every composer-mutating key. `accept_completion()` splices the selected candidate's `insert_text` over the trigger range and closes the popup. Popup keys (Tab accept, Up/Down navigate, Esc close) are intercepted before the layout/composer dispatch. `apply_paste` warns / truncates on large payloads (32 KB warn, 64 KB hard cap) and is a no-op while a modal is open or focus is outside the composer. |
| `fullscreen.rs` | New `CompletionView { title, entries, selected }` field on `UiFrame`. The renderer paints the popup as an overlay just above the composer with `Clear`, a bordered `List`, and a `›` glyph marking the selected entry. Skips painting if the terminal is too short to fit above the composer. |
| `view.rs` | Maps `state.composer_popup` to `CompletionView` and labels the overlay by kind ("Slash commands" / "@ context tokens" / "@ files"). |
| `app.rs` | The input task now forwards bracketed-paste payloads as a single `TuiInbound::Paste(data)` event instead of N `Char(c)` keystrokes; large pastes no longer starve the SDK arm with N redraws. |

### Trigger detection rules

| Buffer state | Kind | Token |
|--------------|------|-------|
| `/<word>` at start of buffer, no whitespace before cursor | Slash | the word |
| `@<word>` preceded by start-of-buffer or whitespace, `<word>` is a prefix of a `CONTEXT_TOKENS` entry | ContextToken | the word |
| `@<word>` preceded by start-of-buffer or whitespace, not a context-token prefix | File | the word |
| anything else | — | — |

File candidates are intentionally **not** computed in the pure reducer (the reducer must not touch the filesystem). Phase D leaves `CompletionKind::File` with an empty candidate list — Phase F or later will populate it from the host workspace.

### Paste thresholds

| Constant | Default | Behavior |
|----------|---------|----------|
| `PASTE_WARN_BYTES` | 32 KiB | Activity entry logged: `large paste: N bytes (over 32768 warn threshold)`. |
| `PASTE_HARD_LIMIT` | 64 KiB | Buffer truncated to the cap at a char boundary; activity entry: `paste truncated: N bytes > 65536 cap`. |

Both thresholds are constants in Phase D; later phases can plumb them through `ui.composer.large_paste_warning_bytes` from `settings.json`.

### Tests

`completion.rs` adds 9 unit tests covering trigger detection (slash, context, file, whitespace boundaries) and popup navigation. `state.rs` adds 7 reducer tests:
`slash_popup_opens_and_accepts`, `at_context_token_popup_opens_and_closes_on_esc`, `popup_up_down_navigates_selection`, `small_paste_inserts_without_warning`, `large_paste_logs_warning`, `huge_paste_is_truncated`, `paste_under_modal_is_swallowed`.

All M1–M12 tests still pass; `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` are clean.

### Out of scope for Phase D

* File candidates from a workspace root — Phase F.
* Prompt history search modal (Ctrl+R) — deferred; Phase D ships popup + paste handling first.
* Multiline composer cursor up/down by line — composer already supports newlines; Phase D doesn't add per-line cursor movement.
* Prompt size warning at submit — low-value vs. paste warning; deferred.
* Slash / `@file` / context-insertion autocomplete — Phase D.
* Diff / approval workbench, plan dashboard — Phase E.
* Tools / MCP / agents / skills / plugins inspectors — Phase F.
* Session browser, recovery, model / provider / auth selectors — Phase G.
* Command palette, help overlay, accessibility hardening — Phase H.
* `examples/tui-demo`, new CLI subcommands, UI telemetry — Phase I.
* Real provider routing in `code-agent tui` beyond fake / Anthropic-API-key.
* OpenTelemetry export, theme system, VS Code / JetBrains / web / desktop
  surfaces (excluded for the whole milestone).

## Phase E — Diff / approval workbench + plan dashboard

### What's new

| Module | Purpose |
|--------|---------|
| `state.rs` | New `TuiAppState` fields `current_plan: Option<PlanDocument>`, `pending_approval: Option<PendingApproval>`, `last_patch: Option<PatchSet>`. New `TuiAction` variants: `ResolveApproval(ApprovalDecisionAction)`, `ApprovePlan(PlanId)`, `RejectPlan { plan_id, reason }`, `RevisePlan { plan_id, hint }`. New constants `APPROVAL_MODAL_ID` and `PLAN_APPROVAL_MODAL_ID` identify the Phase E modals. `apply_sdk_event` now branches into `apply_phase_e_workbench_event` to pull SDK payloads (PlanCreated / PlanApproved / ApprovalRequired / ApprovalResolved / PatchProposed) into reducer state and steer the side / detail panels. `apply_modal_key` recognizes the two modal ids and routes `y` / `s` / `n` / `r` (tool approval) and `a` / `r` / `u` (plan approval) into the corresponding actions. |
| `view.rs` | The Plan side-panel now renders the M9 `format_plan_panel(current_plan)` when a plan is loaded; otherwise the Phase B placeholder. The Approval detail-panel renders the pending approval's tool + summary + key legend. The Diff detail-panel renders the `PatchSet` summary (files +N ~M -K, lines +N -M) followed by the SDK-provided `unified_diff` capped at 200 lines. |
| `app.rs` | Handles the new `TuiAction` variants. For Phase E they log to the activity panel; live wiring of the SDK `ApprovalCallback` is a small Phase G follow-up since the EchoFakeProvider never triggers an approval and the integration is testable only via synthetic events. |

### SDK event → reducer effects

| SdkEvent | Effect |
|----------|--------|
| `PlanCreated { plan }` | Stores `plan`, switches side panel to `Plan`, opens the `plan-approval` modal with `[a] approve [r] reject [u] revise`. |
| `PlanApproved` | Pops the plan-approval modal if still open. |
| `ApprovalRequired { tool_name, summary }` | Stores `pending_approval`, switches detail panel to `Approval`, opens the `approval` modal with `[y] yes [s] yes-for-session [n] no [r] revise`. |
| `ApprovalResolved` | Clears `pending_approval`, pops the approval modal. |
| `PatchProposed { patch }` | Stores `last_patch`, switches detail panel to `Diff`. |

### Keyboard

| Modal | Key | Action |
|-------|-----|--------|
| Approval | `y` | `ResolveApproval(Allow)` |
| Approval | `s` | `ResolveApproval(AllowForSession)` |
| Approval | `n` | `ResolveApproval(Deny { reason: "denied by user" })` |
| Approval | `r` | `ResolveApproval(RequestRevision)` |
| Plan approval | `a` | `ApprovePlan(plan_id)` |
| Plan approval | `r` | `RejectPlan { plan_id, reason: None }` |
| Plan approval | `u` | `RevisePlan { plan_id, hint: "" }` |

### Tests

`state.rs` adds 3 reducer tests covering the SDK→state transitions for ApprovalRequired and PatchProposed, plus the `y`-key path through the approval modal.

All M1–M12 tests still pass; `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` are clean.

### Out of scope for Phase E

* Live wiring of `ResolveApproval` back through the SDK's `ApprovalCallback` — Phase G follow-up; Phase E's surface is testable via synthetic events.
* Free-form input collection for the `r`-revise and `u`-revise paths (modal text input) — Phase H when the command palette ships a generic input modal.
* Per-hunk / per-file partial approval — spec explicitly says full approval first; partial approval is "not yet supported".
* Side-by-side diff mode — `ui.diff.mode = side_by_side` setting wiring lands later.
* Plan revision diff between two `PlanDocument`s — the M9 `format_plan_diff` helper exists; surfacing it in the UI is Phase F/G work.

## Phase F — Inspectors

### What's new

| Module | Purpose |
|--------|---------|
| `inspectors.rs` (new) | Pure read-only record types — `InspectorTool`, `InspectorMcp`, `InspectorSubagent`, `InspectorSkill`, `InspectorPlugin` — and the `Inspectors` snapshot that owns the five vectors. Plain `String` / `usize` / `Option<u32>` fields so the renderer doesn't depend on `code-agent-core` / `code-agent-mcp` enum types. |
| `state.rs` | New `TuiAppState` field `inspectors: Inspectors` and `inspector_selection: InspectorSelection { tools, mcp, subagents, skills, plugins: Option<usize> }`. New builder method `with_inspectors(inspectors)`. New helpers `inspector_list_len(mode)` / `inspector_selected_index(mode)` and the private `move_inspector_selection` / `set_inspector_selection` mutators. The reducer's side-panel key path intercepts `j`/`k`/`Up`/`Down`/`g`/`G` only when the panel is focused **and** the mode is one of the five inspector modes — non-inspector modes (Plan, Changes, Files, Sessions, …) continue to fall through to the composer so typed slash commands still work. |
| `view.rs` | New `inspector_body(state, mode)` builds the side-panel body from the snapshot rows. Rows render with a leading `›` for the active selection and `  ` otherwise. Empty categories render `<Mode> inspector` / `(none configured)` instead of the Phase B placeholder. Long descriptions are truncated with `…` so a single row fits a ~80-column side panel. |
| `code-agent-cli/src/main.rs` | `cmd_tui` now calls a new `build_inspector_snapshot_for_tui(cwd, settings)` before constructing `TuiAppState`, then chains `.with_inspectors(snapshot)`. The snapshot is built best-effort: each category that fails to load (missing dir, parse error, etc.) is left empty. Loaders: `code_agent_tools::register_builtin_with_limits` for tools, `code_agent_mcp::load_mcp_options` for MCP, `code_agent_core::agents::loader::load_filesystem_agents` for subagents, a TUI-local skill discoverer for skills, `plugins::build_for_invocation` for plugins. |

### Inspector rows

| Mode | Row shape |
|------|-----------|
| Tools | `name [permission] — description` |
| MCP | `name [transport] endpoint (N entries)` |
| Subagents | `name [model | tools: N] — description` |
| Skills | `name [source] — description (N resources)` |
| Plugins | `name vX.Y [scope | on/off | status] — N components` |

Permission tags come from `PermissionKind` via a snake_case label
(`read_only`, `mutating`, `execution`, `external`, `orchestration`).
Skill source labels match `SkillSource::as_str`
(`project` / `user` / `compat-claude` / `builtin`). Plugin scopes /
statuses match `PluginScope::as_str` / `PluginStatus`.

### Keyboard

| Key | Action |
|-----|--------|
| `j` / `Down` | Move selection down within the focused inspector. |
| `k` / `Up` | Move selection up. |
| `g` | Jump to first row. |
| `G` | Jump to last row. |
| `Ctrl+C` | Cancel run / exit when idle (mirrors transcript-pane behavior). |
| `Ctrl+D` | Exit. |

Selection is per-mode and persists when toggling between panels via
slash commands. A stale snapshot index (e.g. after a hot reload that
trims rows) is silently clamped to the last valid row by
`inspector_selected_index` so the renderer never reads past the
backing vector.

### Tests

`inspectors.rs` adds 2 unit tests (`default_is_empty`, `record_defaults_are_blank`).

`state.rs` adds 5 reducer tests:
`with_inspectors_replaces_snapshot`,
`inspector_j_k_navigates_selection` (j/k/g/G against a 3-row list),
`inspector_nav_on_empty_list_is_inert` (nav keys swallowed without
falling through to the composer),
`inspector_selection_is_per_mode` (per-mode persistence across
panel switches),
`inspector_selection_clamps_to_visible_range` (out-of-range
selection clamps to last row).

`view.rs` adds 3 renderer tests:
`inspector_body_renders_real_rows_with_selection_glyph`,
`inspector_body_empty_state_is_informative`,
`all_inspector_modes_render_real_rows_when_populated`.

All M1–M12 tests still pass; `cargo fmt --check`, `cargo clippy --workspace --all-targets`, and `cargo test --workspace` (1011 tests total) are clean.

### Out of scope for Phase F

* Detail-panel "inspect this row" view — selecting a tool / skill / plugin and pressing `Enter` to show its full schema / manifest / instructions. The detail panel's `ToolDetails` / `FilePreview` modes were stubbed in Phase B; populating them lands when Phase G adds the diagnostics workbench since the two share the same detail-pane real estate.
* Live MCP server status (connected / failed / tool count) — needs a connection attempt; deferred so `code-agent tui` startup stays fast. The Phase F MCP inspector only lists *configured* servers, not running ones.
* Plugin marketplace browsing / install / enable-disable actions — Phase F is read-only.
* File-system `@file` autocomplete candidates (the Phase D `CompletionKind::File` empty-list seam) — needs a workspace scanner; Phase F's host has the cwd and a workspace handle but the actual scanner and candidate sort are deferred to Phase H so the candidate model can share the command-palette infrastructure.
* Hot-reload of the inspector snapshot when the user runs `/plugins reload` or edits a SKILL.md — Phase H+.
