//! Pure, deterministic state machine that drives the live TUI.
//!
//! `TuiAppState` owns the transcript, composer state, activity log, run
//! status, and status-line context. It is mutated only through
//! [`TuiAppState::apply`], which consumes a [`TuiInbound`] event and
//! returns a list of side-effect intents ([`TuiAction`]) for the host
//! event loop to perform. All terminal I/O, runtime calls, and async
//! work live in `app.rs`; this module remains synchronous and pure so
//! it can be exercised by unit tests with no terminal or runtime
//! involvement.

use std::collections::HashSet;

use code_agent_core::StatusLineContext;
use code_agent_sdk::SdkEvent;

use crate::completion::{
    context_token_candidates, detect_trigger, slash_candidates, CompletionKind, CompletionPopup,
    SLASH_COMMANDS,
};
use crate::composer::{ComposerState, KeyAction, KeyEvent};
use crate::event_adapter::map_sdk_event;
use crate::layout::{DetailPanelMode, FocusPane, LayoutState, SidePanelMode, TuiModal};
use crate::transcript::EventCard;

/// Max bytes accepted in a single paste before the reducer truncates
/// and surfaces a warning. Phase D ceiling; later phases can wire
/// `ui.composer.large_paste_warning_bytes` through `TuiAppState`.
pub const PASTE_HARD_LIMIT: usize = 64 * 1024;

/// Threshold at which a paste produces a warning entry in the
/// activity log. Pastes between this and [`PASTE_HARD_LIMIT`] are
/// inserted in full but logged.
pub const PASTE_WARN_BYTES: usize = 32 * 1024;

/// Maximum number of transcript cards retained in state. Bounded so a
/// long-running session doesn't grow unbounded memory; later phases will
/// virtualize scrollback over a larger backing store.
pub const TRANSCRIPT_CAP: usize = 1024;

/// Maximum number of activity-log entries retained.
pub const ACTIVITY_CAP: usize = 16;

/// Side-effect intents emitted by the reducer for the host loop to handle.
///
/// All UI-internal mutations (panel toggling, focus cycling, modal stack)
/// happen inside the reducer and produce no `TuiAction`. The host loop
/// only needs to react to actions that cross the runtime boundary or end
/// the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TuiAction {
    /// Submit a new prompt to the runtime. The host should construct an
    /// `AgentRequest`, call `runtime.run(...)`, and feed the resulting
    /// event stream back into the reducer.
    SubmitPrompt(String),
    /// Cancel the active run, if any. No-op when no run is in flight.
    CancelRun,
    /// Exit the TUI cleanly.
    Exit,
}

/// Events the reducer consumes.
///
/// `Sdk` boxes its payload because `SdkEvent` is much larger than every
/// other variant (clippy `large_enum_variant`), and inbound events are
/// allocated per delivery anyway.
#[derive(Debug, Clone)]
pub enum TuiInbound {
    /// A typed SDK event from the active run.
    Sdk(Box<SdkEvent>),
    /// The active run's event stream encountered a fatal error.
    RunStreamError(String),
    /// The active run's event stream finished. Resets run status if a
    /// terminal SDK event has not already been observed.
    RunStreamEnded,
    /// A key press from the terminal, already decoded to the composer's
    /// abstract `KeyEvent` enum.
    Key(KeyEvent),
    /// A whole-paste payload. Phase D delivers bracketed-paste data as
    /// a single event so the reducer can compute size, warn on large
    /// pastes, and insert the text in one go instead of per-char.
    Paste(String),
    /// Terminal was resized to the given dimensions.
    Resize { width: u16, height: u16 },
}

impl TuiInbound {
    /// Convenience: wrap an `SdkEvent` into a boxed `Sdk` variant.
    pub fn sdk(event: SdkEvent) -> Self {
        Self::Sdk(Box::new(event))
    }
}

/// Run lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunStatus {
    #[default]
    Idle,
    Running,
}

/// Top-level TUI state. Phase B added `layout` for the multi-pane
/// cockpit; Phase C adds transcript navigation + search state.
/// Later phases extend without breaking this surface.
///
/// `#[non_exhaustive]` reserves the right to add private fields in
/// future phases without forcing every literal construction to update.
/// Internal code in this crate constructs the type via [`Self::new`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TuiAppState {
    pub composer: ComposerState,
    pub transcript: Vec<EventCard>,
    pub activity: Vec<String>,
    pub status_template: String,
    pub status_ctx: StatusLineContext,
    pub run_status: RunStatus,
    pub last_error: Option<String>,
    pub size: (u16, u16),
    pub should_quit: bool,
    pub layout: LayoutState,
    /// Monotonic id assigned to each card as it enters state. Used to
    /// give cards a stable identity that survives filtering / scrolling.
    pub next_card_id: u64,
    /// Id of the currently-selected transcript card, if any. `None`
    /// means no selection; navigation keys initialize selection to the
    /// most recent visible card.
    pub selected_card_id: Option<u64>,
    /// Ids of cards the user has explicitly expanded (e/Enter). Cards
    /// with empty `expanded_body` ignore the expansion bit so the
    /// renderer doesn't paint phantom whitespace.
    pub expanded_card_ids: HashSet<u64>,
    /// Active substring filter for the transcript. `None` means no
    /// filter; the empty string is normalized to `None` on commit.
    pub search_query: Option<String>,
    /// Input buffer for the in-progress search modal (filled when the
    /// SearchTranscript modal is open).
    pub search_input: String,
    /// Cached visible indices for the active `search_query`. Recomputed
    /// only when the query or transcript content changes, so the
    /// renderer's per-frame call to `visible_card_indices` is cheap
    /// even for large transcripts with case-insensitive substring
    /// matching.
    visible_cache: Option<Vec<usize>>,
    /// Active completion popup, if any. Phase D autocomplete state:
    /// rebuilt by `refresh_composer_popup` after every key that
    /// mutates the composer buffer.
    pub composer_popup: Option<CompletionPopup>,
}

impl TuiAppState {
    /// Construct a new state from a composer state and an initial status
    /// context.
    pub fn new(
        composer: ComposerState,
        status_template: String,
        status_ctx: StatusLineContext,
    ) -> Self {
        let mut layout = LayoutState::default();
        layout.recompute_for_size(80, 24);
        Self {
            composer,
            transcript: Vec::new(),
            activity: Vec::new(),
            status_template,
            status_ctx,
            run_status: RunStatus::Idle,
            last_error: None,
            size: (80, 24),
            should_quit: false,
            layout,
            next_card_id: 0,
            selected_card_id: None,
            expanded_card_ids: HashSet::new(),
            search_query: None,
            search_input: String::new(),
            visible_cache: None,
            composer_popup: None,
        }
    }

    /// Return the indices into `self.transcript` for the cards that
    /// pass the current search filter. With no filter active, returns
    /// `0..transcript.len()`.
    ///
    /// Uses an internal cache so a repeated call in the same draw
    /// cycle is O(visible). The cache is invalidated by
    /// `push_transcript` and by any mutation of `search_query` that
    /// goes through the reducer's modal path.
    pub fn visible_card_indices(&self) -> Vec<usize> {
        if let Some(cached) = &self.visible_cache {
            return cached.clone();
        }
        self.compute_visible_indices()
    }

    fn compute_visible_indices(&self) -> Vec<usize> {
        match &self.search_query {
            Some(q) if !q.is_empty() => self
                .transcript
                .iter()
                .enumerate()
                .filter_map(|(i, c)| if c.matches_search(q) { Some(i) } else { None })
                .collect(),
            _ => (0..self.transcript.len()).collect(),
        }
    }

    fn invalidate_search_cache(&mut self) {
        self.visible_cache = None;
    }

    fn refresh_search_cache(&mut self) {
        self.visible_cache = Some(self.compute_visible_indices());
    }

    /// Is the given card expanded? An expanded card with empty
    /// `expanded_body` still reports `true` (caller can short-circuit).
    pub fn is_card_expanded(&self, id: u64) -> bool {
        self.expanded_card_ids.contains(&id)
    }

    /// Replace the initial side panel mode + visibility. Convenience for
    /// callers that want to honor `ui.default_side_panel` /
    /// `ui.show_side_panel` settings without poking the layout field
    /// directly.
    pub fn with_initial_side_panel(mut self, mode: SidePanelMode, visible: bool) -> Self {
        self.layout.side_panel.mode = mode;
        self.layout.side_panel.visible = visible;
        self
    }

    /// Apply an inbound event. Returns the list of host-side actions that
    /// the event loop should execute (e.g. submit a prompt to the runtime,
    /// cancel the active run, exit cleanly).
    ///
    /// The reducer is total: it never panics, never blocks, and the same
    /// (state, event) input always produces the same (state', actions)
    /// output — so it is straightforward to snapshot-test.
    pub fn apply(&mut self, event: TuiInbound) -> Vec<TuiAction> {
        match event {
            TuiInbound::Sdk(ev) => {
                self.apply_sdk_event(ev.as_ref());
                Vec::new()
            }
            TuiInbound::RunStreamError(message) => {
                self.last_error = Some(message.clone());
                self.run_status = RunStatus::Idle;
                self.push_transcript(EventCard {
                    kind: "run_failed",
                    headline: "✗ run stream error".to_string(),
                    body: vec![message],
                    ..Default::default()
                });
                Vec::new()
            }
            TuiInbound::RunStreamEnded => {
                if matches!(self.run_status, RunStatus::Running) {
                    self.run_status = RunStatus::Idle;
                }
                Vec::new()
            }
            TuiInbound::Key(k) => self.apply_key(k),
            TuiInbound::Paste(data) => self.apply_paste(data),
            TuiInbound::Resize { width, height } => {
                self.size = (width, height);
                self.layout.recompute_for_size(width, height);
                // Responsive shrink may hide the panel that currently
                // holds focus. Bring focus back to the composer so the
                // user always has a visible cursor.
                self.clamp_focus();
                Vec::new()
            }
        }
    }

    /// Insert a clipboard / bracketed-paste payload into the composer.
    /// Emits an activity warning when the paste exceeds
    /// [`PASTE_WARN_BYTES`] and truncates when above
    /// [`PASTE_HARD_LIMIT`].
    fn apply_paste(&mut self, mut data: String) -> Vec<TuiAction> {
        // When a modal is open, bracketed-paste is intercepted by
        // the modal-specific handler (e.g. SearchTranscript). If
        // the focus is on the transcript pane, pasting is a no-op.
        if self.layout.current_modal().is_some()
            || !matches!(self.layout.focus, FocusPane::Composer)
        {
            return Vec::new();
        }
        let original_len = data.len();
        if original_len > PASTE_HARD_LIMIT {
            // Truncate at a char boundary to keep the buffer valid UTF-8.
            let mut cut = PASTE_HARD_LIMIT;
            while cut > 0 && !data.is_char_boundary(cut) {
                cut -= 1;
            }
            data.truncate(cut);
            self.push_activity(format!(
                "paste truncated: {original_len} bytes > {PASTE_HARD_LIMIT} cap"
            ));
        } else if original_len > PASTE_WARN_BYTES {
            self.push_activity(format!(
                "large paste: {original_len} bytes (over {PASTE_WARN_BYTES} warn threshold)"
            ));
        }
        // Insert at the cursor.
        let pos = self.composer.cursor.min(self.composer.buffer.len());
        self.composer.buffer.insert_str(pos, &data);
        self.composer.cursor = pos + data.len();
        self.refresh_composer_popup();
        Vec::new()
    }

    /// Rebuild [`Self::composer_popup`] from the current composer
    /// buffer + cursor. Pure: only the buffer + cursor are inspected.
    /// File completions are intentionally NOT fetched here — the
    /// reducer must not touch the filesystem. The host (`app.rs`)
    /// can populate file candidates later if it wants to support
    /// `@<path>` completions in the live shell; Phase D ships
    /// slash + context-token completions only.
    pub fn refresh_composer_popup(&mut self) {
        let trigger = detect_trigger(&self.composer.buffer, self.composer.cursor);
        self.composer_popup = match trigger {
            Some(t) => {
                let candidates = match t.kind {
                    CompletionKind::Slash => slash_candidates(&t.token, SLASH_COMMANDS, 12),
                    CompletionKind::ContextToken => context_token_candidates(&t.token, 12),
                    // File candidates require the workspace root, which
                    // lives in the host's runtime context; Phase D
                    // leaves these to be populated by the host. We
                    // still surface the popup with an empty list to
                    // signal intent so later phases can plug in real
                    // file completion.
                    CompletionKind::File => Vec::new(),
                };
                if candidates.is_empty() {
                    None
                } else {
                    Some(CompletionPopup::new(
                        t.kind,
                        candidates,
                        t.token_start,
                        t.token_end,
                    ))
                }
            }
            None => None,
        };
    }

    /// Accept the currently-selected completion candidate. Splices the
    /// candidate's `insert_text` into the composer buffer at the
    /// trigger range, moves the cursor to the end of the inserted
    /// text, and closes the popup.
    pub fn accept_completion(&mut self) {
        let Some(popup) = self.composer_popup.take() else {
            return;
        };
        let Some(candidate) = popup.selected_candidate().cloned() else {
            return;
        };
        let start = popup.token_start.min(self.composer.buffer.len());
        let end = popup.token_end.min(self.composer.buffer.len()).max(start);
        self.composer
            .buffer
            .replace_range(start..end, &candidate.insert_text);
        self.composer.cursor = start + candidate.insert_text.len();
    }

    fn apply_key(&mut self, k: KeyEvent) -> Vec<TuiAction> {
        // Completion popup keys take priority over everything except
        // the global modal/layout dispatch — but only when the
        // composer pane is the focused pane. If focus has moved
        // (Tab cycled to transcript), the popup is treated as stale
        // and Tab/Up/Down/Esc fall through to pane-specific behavior.
        if self.composer_popup.is_some()
            && self.layout.current_modal().is_none()
            && matches!(self.layout.focus, FocusPane::Composer)
        {
            match k {
                KeyEvent::Tab => {
                    self.accept_completion();
                    return Vec::new();
                }
                KeyEvent::Up => {
                    if let Some(p) = self.composer_popup.as_mut() {
                        p.prev();
                    }
                    return Vec::new();
                }
                KeyEvent::Down => {
                    if let Some(p) = self.composer_popup.as_mut() {
                        p.next();
                    }
                    return Vec::new();
                }
                KeyEvent::Esc => {
                    self.composer_popup = None;
                    return Vec::new();
                }
                _ => {}
            }
        }
        // Layout-priority keys intercepted before the composer. These
        // are global navigation that does not depend on which pane has
        // focus, so the composer never sees them. Cycling focus away
        // from the composer also closes any stale completion popup
        // so navigation keys in the new pane aren't ambiguous.
        match k {
            KeyEvent::Tab => {
                self.layout.focus = self.layout.focus.next(
                    self.layout.side_panel_rendered(),
                    self.layout.detail_panel_rendered(),
                );
                if !matches!(self.layout.focus, FocusPane::Composer) {
                    self.composer_popup = None;
                }
                return Vec::new();
            }
            KeyEvent::ShiftTab => {
                self.layout.focus = self.layout.focus.prev(
                    self.layout.side_panel_rendered(),
                    self.layout.detail_panel_rendered(),
                );
                if !matches!(self.layout.focus, FocusPane::Composer) {
                    self.composer_popup = None;
                }
                return Vec::new();
            }
            _ => {}
        }
        // When a modal is open, route keys to the modal-specific
        // handler. The SearchTranscript modal accumulates an input
        // buffer; other modals swallow most input. Esc always pops
        // the modal; Ctrl+C always reaches the run-cancel path.
        if let Some(modal) = self.layout.current_modal().cloned() {
            return self.apply_modal_key(modal, k);
        }
        // Transcript-focused navigation. While the transcript pane is
        // focused, the composer is intentionally read-only: typing
        // characters does not modify its buffer, and unmapped keys are
        // swallowed. Only global exits (Ctrl+C / Ctrl+D) escape this
        // mode without first cycling focus back to the composer.
        if matches!(self.layout.focus, FocusPane::Transcript) {
            match k {
                KeyEvent::CtrlC => {
                    return match self.run_status {
                        RunStatus::Running => vec![TuiAction::CancelRun],
                        RunStatus::Idle => vec![TuiAction::Exit],
                    };
                }
                KeyEvent::CtrlD => return vec![TuiAction::Exit],
                _ => {}
            }
            if let Some(actions) = self.apply_transcript_nav_key(k) {
                return actions;
            }
            // Any other key (including unmapped chars and Up/Down when
            // visible_card_indices is empty) is swallowed — the
            // composer never sees it. Without this guard, typing
            // "agent" while focused on the transcript would write "at"
            // into the composer and randomly trigger `g`/`e`/`n`.
            return Vec::new();
        }
        let action = self.composer.handle_key(k);
        // Anything that may have mutated the composer buffer requires
        // a popup refresh. Submit (which clears the buffer) closes the
        // popup; chars / backspace / paste may open or update it.
        self.refresh_composer_popup();
        match action {
            KeyAction::Submit(prompt) => {
                let trimmed = prompt.trim();
                if trimmed.is_empty() {
                    return Vec::new();
                }
                // `//foo` is the literal-prompt escape: collapse the
                // leading double-slash into a single `/` and submit
                // `/foo` to the runtime. Lets users send absolute
                // paths or anything that starts with `/` without
                // colliding with the slash-command space.
                if let Some(rest) = trimmed.strip_prefix("//") {
                    let with_one_slash = format!("/{rest}");
                    return self.submit_prompt_action(&with_one_slash);
                }
                // Slash commands route into the layout state machine
                // instead of the runtime. Unknown commands are NOT
                // fall-through-submitted: that would turn a typo
                // (`/halp`) into a silently-sent prompt, which is worse
                // than a clear diagnostic.
                if let Some(stripped) = trimmed.strip_prefix('/') {
                    self.apply_slash_command(stripped);
                    return Vec::new();
                }
                self.submit_prompt_action(trimmed)
            }
            KeyAction::Interrupt => match self.run_status {
                RunStatus::Running => vec![TuiAction::CancelRun],
                RunStatus::Idle => vec![TuiAction::Exit],
            },
            KeyAction::Exit => vec![TuiAction::Exit],
            KeyAction::None | KeyAction::HistorySearch => Vec::new(),
        }
    }

    /// Route a key to the topmost modal. Most modals swallow input
    /// other than Esc (close) and Ctrl+C (cancel run). The
    /// SearchTranscript modal additionally accumulates printable
    /// characters into `self.search_input` and commits the query on
    /// Enter.
    fn apply_modal_key(&mut self, modal: TuiModal, k: KeyEvent) -> Vec<TuiAction> {
        match (modal, k) {
            (_, KeyEvent::Esc) => {
                let _ = self.layout.pop_modal();
                // Cancelling search clears the in-progress input.
                self.search_input.clear();
                Vec::new()
            }
            (_, KeyEvent::CtrlC) => match self.run_status {
                RunStatus::Running => vec![TuiAction::CancelRun],
                RunStatus::Idle => Vec::new(),
            },
            (TuiModal::SearchTranscript, KeyEvent::Char(c)) => {
                self.search_input.push(c);
                Vec::new()
            }
            (TuiModal::SearchTranscript, KeyEvent::Backspace) => {
                self.search_input.pop();
                Vec::new()
            }
            (TuiModal::SearchTranscript, KeyEvent::Enter) => {
                let q = self.search_input.trim().to_string();
                self.search_query = if q.is_empty() { None } else { Some(q) };
                self.search_input.clear();
                let _ = self.layout.pop_modal();
                // Search filter changed; recompute the cached visible
                // set eagerly so the per-frame `visible_card_indices`
                // call is cheap even for long transcripts.
                self.refresh_search_cache();
                // Jump selection to the first visible match.
                let first = self
                    .visible_card_indices()
                    .first()
                    .and_then(|i| self.transcript.get(*i).map(|c| c.id));
                if first.is_some() {
                    self.selected_card_id = first;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Transcript-focused navigation. Returns `Some(actions)` when the
    /// key was handled (so the caller should NOT also forward it to
    /// the composer), `None` otherwise.
    fn apply_transcript_nav_key(&mut self, k: KeyEvent) -> Option<Vec<TuiAction>> {
        let visible = self.visible_card_indices();
        if visible.is_empty() {
            return None;
        }
        let current = self
            .selected_card_id
            .and_then(|id| visible.iter().position(|i| self.transcript[*i].id == id))
            .unwrap_or_else(|| visible.len() - 1);
        match k {
            KeyEvent::Char('j') | KeyEvent::Down => {
                let new = (current + 1).min(visible.len() - 1);
                self.selected_card_id = Some(self.transcript[visible[new]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('k') | KeyEvent::Up => {
                let new = current.saturating_sub(1);
                self.selected_card_id = Some(self.transcript[visible[new]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('g') => {
                self.selected_card_id = Some(self.transcript[visible[0]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('G') => {
                self.selected_card_id = Some(self.transcript[visible[visible.len() - 1]].id);
                Some(Vec::new())
            }
            KeyEvent::Char('e') | KeyEvent::Enter => {
                // Resolve to the currently-visible cursor in case
                // `selected_card_id` points at a card that aged out of
                // the ring buffer. Toggling a vanished id silently
                // mutates state that's not rendered.
                let cursor_id = self.transcript[visible[current]].id;
                self.selected_card_id = Some(cursor_id);
                if self.expanded_card_ids.contains(&cursor_id) {
                    self.expanded_card_ids.remove(&cursor_id);
                } else {
                    self.expanded_card_ids.insert(cursor_id);
                }
                Some(Vec::new())
            }
            KeyEvent::Char('n') => {
                // Next search match. Without an active query this is a
                // no-op (the visible set is everything).
                if self.search_query.is_some() {
                    let new = (current + 1).min(visible.len() - 1);
                    self.selected_card_id = Some(self.transcript[visible[new]].id);
                }
                Some(Vec::new())
            }
            KeyEvent::Char('N') => {
                if self.search_query.is_some() {
                    let new = current.saturating_sub(1);
                    self.selected_card_id = Some(self.transcript[visible[new]].id);
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Common path for submitting a literal prompt (used both by
    /// vanilla input and the `//` escape). Enforces the
    /// "no submit while run is in progress" guard.
    fn submit_prompt_action(&mut self, prompt: &str) -> Vec<TuiAction> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Vec::new();
        }
        if matches!(self.run_status, RunStatus::Running) {
            self.push_activity("ignored prompt: run already in progress".to_string());
            return Vec::new();
        }
        let prompt = prompt.to_string();
        self.push_transcript(EventCard {
            kind: "user",
            headline: "You:".to_string(),
            body: prompt.lines().map(String::from).collect(),
            ..Default::default()
        });
        self.run_status = RunStatus::Running;
        self.last_error = None;
        vec![TuiAction::SubmitPrompt(prompt)]
    }

    fn apply_slash_command(&mut self, cmd: &str) {
        // Allow `/cmd args...` by splitting off the first whitespace
        // run. Phase B doesn't act on args yet; Phase H will use them
        // for palette / search inputs.
        let (cmd_name, _args) = cmd
            .trim()
            .split_once(char::is_whitespace)
            .unwrap_or((cmd.trim(), ""));
        // Side-panel switches. Accept any of the 14 canonical config
        // keys plus `agents` (alias for `subagents`) — kept in sync
        // with `SidePanelMode::from_config_key`.
        if let Some(mode) = SidePanelMode::from_config_key(cmd_name) {
            self.layout.side_panel.mode = mode;
            self.layout.side_panel.visible = true;
            // Only move focus to the side panel if it will actually be
            // rendered: in Compact / Tiny layouts the panel is hidden,
            // and stranding focus on an invisible pane creates a
            // no-visible-focus state.
            if self.layout.side_panel_rendered() {
                self.layout.focus = FocusPane::SidePanel;
            }
            self.push_activity(format!("side panel → {}", mode.label()));
            self.clamp_focus();
            return;
        }
        match cmd_name {
            "help" | "keys" => {
                self.layout.push_modal(TuiModal::Help);
            }
            "palette" => {
                self.layout.push_modal(TuiModal::CommandPalette);
            }
            "search" => {
                self.layout.push_modal(TuiModal::SearchTranscript);
            }
            "history" => {
                self.layout.push_modal(TuiModal::PromptHistory);
            }
            "toggle-side" | "side" => {
                self.layout.side_panel.visible = !self.layout.side_panel.visible;
            }
            "toggle-detail" | "detail" => {
                self.layout.detail_panel.visible = !self.layout.detail_panel.visible;
            }
            "" => {
                // Bare `/` — no-op, harmless.
            }
            other => {
                self.push_activity(format!("unknown slash command: /{other}"));
            }
        }
        // Whatever changed above, make sure focus still points at a
        // pane that is rendered. Otherwise the user has no visible
        // cursor until they press Tab.
        self.clamp_focus();
    }

    /// If `layout.focus` points at a pane that is not currently
    /// rendered (because it was hidden or because responsive mode
    /// suppresses it), reset focus to the composer. Cheap to call
    /// after any panel-state or layout-mode mutation.
    fn clamp_focus(&mut self) {
        let side_ok = self.layout.side_panel_rendered();
        let detail_ok = self.layout.detail_panel_rendered();
        match self.layout.focus {
            FocusPane::SidePanel if !side_ok => {
                self.layout.focus = FocusPane::Composer;
            }
            FocusPane::DetailPanel if !detail_ok => {
                self.layout.focus = FocusPane::Composer;
            }
            _ => {}
        }
    }

    /// Public helper: directly set the side-panel mode (e.g. from a
    /// command palette entry in a future phase, or from CLI args).
    pub fn set_side_panel(&mut self, mode: SidePanelMode, visible: bool) {
        self.layout.side_panel.mode = mode;
        self.layout.side_panel.visible = visible;
    }

    /// Public helper: directly set the detail-panel mode.
    pub fn set_detail_panel(&mut self, mode: DetailPanelMode, visible: bool) {
        self.layout.detail_panel.mode = mode;
        self.layout.detail_panel.visible = visible;
    }

    /// Public helper: open a modal explicitly.
    pub fn push_modal(&mut self, modal: TuiModal) {
        self.layout.push_modal(modal);
    }

    /// Public helper: close the topmost modal.
    pub fn pop_modal(&mut self) -> Option<TuiModal> {
        self.layout.pop_modal()
    }

    fn apply_sdk_event(&mut self, ev: &SdkEvent) {
        let outcome = map_sdk_event(ev);
        if let Some(delta) = outcome.coalesce_assistant_delta {
            self.coalesce_assistant_delta(&delta);
        }
        if let Some(card) = outcome.card {
            self.push_transcript(card);
        }
        if let Some(label) = outcome.activity {
            self.push_activity(label);
        }
        if let Some(err) = outcome.error {
            self.last_error = Some(err);
        }
        if let Some(success) = outcome.run_terminal {
            self.run_status = RunStatus::Idle;
            self.status_ctx
                .set("run_status", if success { "ok" } else { "failed" });
        } else if matches!(ev, SdkEvent::RunStarted(_)) {
            self.run_status = RunStatus::Running;
            self.status_ctx.set("run_status", "running");
        }
    }

    fn coalesce_assistant_delta(&mut self, delta: &str) {
        // Streaming deltas are coalesced into the most recent assistant
        // card so the transcript reflects a single message rather than a
        // wall of fragments. Embedded newlines must split into new body
        // entries — otherwise a single body line could contain a literal
        // `\n` and the transcript pane would silently lose the break.
        // If the last card isn't an assistant card, start a fresh one.
        let mut pieces = delta.split('\n');
        let first = pieces.next().unwrap_or("");
        let rest: Vec<&str> = pieces.collect();
        if let Some(last) = self.transcript.last_mut() {
            if last.kind == "assistant" || last.kind == "assistant_delta" {
                if last.body.is_empty() {
                    last.body.push(first.to_string());
                } else {
                    let n = last.body.len();
                    last.body[n - 1].push_str(first);
                }
                for line in rest {
                    last.body.push(line.to_string());
                }
                return;
            }
        }
        let mut body: Vec<String> = Vec::with_capacity(1 + rest.len());
        body.push(first.to_string());
        for line in rest {
            body.push(line.to_string());
        }
        self.push_transcript(EventCard {
            kind: "assistant_delta",
            headline: "Agent:".to_string(),
            body,
            ..Default::default()
        });
    }

    fn push_transcript(&mut self, mut card: EventCard) {
        // Stamp the card with a stable id before it enters state. The
        // id is used by selection / expansion / search-result sets so
        // they survive the transcript ring buffer's eviction of older
        // cards.
        card.id = self.next_card_id;
        self.next_card_id = self.next_card_id.wrapping_add(1);
        self.transcript.push(card);
        // Bounded ring buffer; oldest cards drop first. As we evict,
        // drop their ids from the expansion set and clear the
        // selection cursor if it points at an evicted card. Without
        // this, `expanded_card_ids` would grow unboundedly across a
        // long session.
        if self.transcript.len() > TRANSCRIPT_CAP {
            let excess = self.transcript.len() - TRANSCRIPT_CAP;
            for evicted in self.transcript.drain(..excess) {
                self.expanded_card_ids.remove(&evicted.id);
                if self.selected_card_id == Some(evicted.id) {
                    self.selected_card_id = None;
                }
            }
        }
        // Search results may have changed; invalidate cache.
        self.invalidate_search_cache();
    }

    fn push_activity(&mut self, label: String) {
        self.activity.push(label);
        if self.activity.len() > ACTIVITY_CAP {
            let excess = self.activity.len() - ACTIVITY_CAP;
            self.activity.drain(..excess);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::ComposerConfig;
    use code_agent_sdk::event::{
        AssistantTextDelta, RunCompletedEvent, RunFailedEvent, RunStarted, ToolCallStartedEvent,
    };
    use code_agent_sdk::request::RequestMetadata;
    use code_agent_sdk::result::{AgentRunStatus, EventSummary};
    use code_agent_sdk::{ProviderId, RunId};

    fn fresh_state() -> TuiAppState {
        TuiAppState::new(
            ComposerState::new(ComposerConfig::default()),
            "{project}".to_string(),
            StatusLineContext::default(),
        )
    }

    fn rid() -> RunId {
        RunId::new()
    }

    #[test]
    // Typing characters then pressing Enter emits a SubmitPrompt action and
    // marks the run as in-progress, with a "You:" card in the transcript.
    fn submit_prompt_emits_action_and_run_started() {
        let mut s = fresh_state();
        for c in "hi".chars() {
            assert!(s.apply(TuiInbound::Key(KeyEvent::Char(c))).is_empty());
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(actions, vec![TuiAction::SubmitPrompt("hi".to_string())]);
        assert_eq!(s.run_status, RunStatus::Running);
        assert_eq!(s.transcript.last().unwrap().kind, "user");
    }

    #[test]
    // RunStarted updates run_status and pushes a "run started" card.
    fn run_started_updates_state() {
        let mut s = fresh_state();
        let ev = SdkEvent::RunStarted(RunStarted {
            run_id: rid(),
            session_id: None,
            provider: ProviderId::Fake,
            model: "fake-model".to_string(),
            metadata: RequestMetadata::default(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Running);
        assert!(s.transcript.iter().any(|c| c.kind == "run_started"));
    }

    #[test]
    // RunCompleted moves run_status back to Idle and clears nothing else.
    fn run_completed_moves_to_idle() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        let ev = SdkEvent::RunCompleted(RunCompletedEvent {
            run_id: rid(),
            status: AgentRunStatus::Completed,
            summary: "done".to_string(),
            usage: None,
            events_summary: EventSummary::default(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert!(s.last_error.is_none());
    }

    #[test]
    // RunFailed records the error message and returns to Idle.
    fn run_failed_records_error_and_idles() {
        let mut s = fresh_state();
        let ev = SdkEvent::RunFailed(RunFailedEvent {
            run_id: rid(),
            status: AgentRunStatus::Failed,
            message: "boom".to_string(),
            diagnostics: Vec::new(),
        });
        s.apply(TuiInbound::sdk(ev));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert_eq!(s.last_error.as_deref(), Some("boom"));
    }

    #[test]
    // Successive AssistantTextDelta events accumulate into a single card.
    fn assistant_deltas_coalesce() {
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "hel".to_string(),
            },
        )));
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "lo".to_string(),
            },
        )));
        let last = s.transcript.last().unwrap();
        assert_eq!(last.kind, "assistant_delta");
        assert_eq!(last.body, vec!["hello".to_string()]);
    }

    #[test]
    // Tool calls show up in the activity log; capacity is enforced.
    fn tool_call_pushes_activity_with_cap() {
        let mut s = fresh_state();
        for i in 0..(ACTIVITY_CAP + 4) {
            s.apply(TuiInbound::sdk(SdkEvent::ToolCallStarted(
                ToolCallStartedEvent {
                    run_id: rid(),
                    tool_name: format!("tool_{i}"),
                    tool_use_id: i.to_string(),
                    input: serde_json::Value::Null,
                },
            )));
        }
        assert_eq!(s.activity.len(), ACTIVITY_CAP);
        assert!(s
            .activity
            .last()
            .unwrap()
            .contains(&format!("tool_{}", ACTIVITY_CAP + 3)));
    }

    #[test]
    // Ctrl+C while idle exits; Ctrl+C while running cancels.
    fn ctrl_c_exits_or_cancels() {
        let mut s = fresh_state();
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::Exit]
        );
        s.run_status = RunStatus::Running;
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::CancelRun]
        );
    }

    #[test]
    // RunStreamError records the message, idles the run, and adds a card.
    fn stream_error_records_and_idles() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        s.apply(TuiInbound::RunStreamError("disconnected".to_string()));
        assert_eq!(s.run_status, RunStatus::Idle);
        assert_eq!(s.last_error.as_deref(), Some("disconnected"));
        assert!(s.transcript.last().unwrap().kind == "run_failed");
    }

    #[test]
    // Submitting a prompt while a run is already in progress is rejected
    // before any state mutation, so no orphan "You:" card is left for a
    // prompt that never runs. The user gets an activity entry instead.
    fn submit_while_running_is_rejected() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        for c in "second".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert!(s.transcript.iter().all(|c| c.kind != "user"));
        assert!(s
            .activity
            .iter()
            .any(|a| a.contains("run already in progress")));
    }

    #[test]
    // AssistantTextDelta events that contain a newline split into a new
    // body line so the transcript pane never silently swallows a `\n`.
    fn delta_with_newline_splits_into_lines() {
        let mut s = fresh_state();
        s.apply(TuiInbound::sdk(SdkEvent::AssistantTextDelta(
            AssistantTextDelta {
                run_id: rid(),
                delta: "first\nsecond".to_string(),
            },
        )));
        let last = s.transcript.last().unwrap();
        assert_eq!(last.kind, "assistant_delta");
        assert_eq!(
            last.body,
            vec!["first".to_string(), "second".to_string()],
            "delta with `\\n` should split into separate body lines"
        );
    }

    #[test]
    // Resize updates the cached terminal size.
    fn resize_updates_size() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 132,
            height: 50,
        });
        assert_eq!(s.size, (132, 50));
    }

    #[test]
    // Tab cycles focus forward through visible panes; Shift+Tab cycles
    // backward. The composer never sees these keys.
    fn tab_and_shift_tab_cycle_focus() {
        let mut s = fresh_state();
        s.layout.side_panel.visible = true;
        s.layout.detail_panel.visible = true;
        // Recompute mode for a wide-enough size so panels are rendered.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        assert_eq!(s.layout.focus, FocusPane::Composer);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Transcript);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::DetailPanel);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Composer);
        s.apply(TuiInbound::Key(KeyEvent::ShiftTab));
        assert_eq!(s.layout.focus, FocusPane::DetailPanel);
    }

    #[test]
    // Esc with a modal on top of the stack pops it; without a modal,
    // Esc is a no-op (the composer ignores Esc by design).
    fn esc_pops_modal_when_open() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        assert!(s.layout.current_modal().is_some());
        let actions = s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(actions.is_empty());
        assert!(s.layout.current_modal().is_none());
        // Without a modal Esc is harmless.
        let _ = s.apply(TuiInbound::Key(KeyEvent::Esc));
    }

    #[test]
    // A `/files` slash command switches the side panel mode and never
    // submits a prompt to the runtime. On a Full-sized terminal the
    // side panel is rendered, so focus moves to it.
    fn slash_command_routes_panel_switch() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        for c in "/files".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty(), "slash must not emit SubmitPrompt");
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
        assert!(s.layout.side_panel.visible);
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
    }

    #[test]
    // /help opens the help modal.
    fn slash_help_opens_help_modal() {
        let mut s = fresh_state();
        for c in "/help".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(matches!(s.layout.current_modal(), Some(TuiModal::Help)));
    }

    #[test]
    // Unknown slash commands surface as an activity entry but never
    // submit a prompt.
    fn unknown_slash_is_diagnostic_not_prompt() {
        let mut s = fresh_state();
        for c in "/nope".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert!(s
            .activity
            .iter()
            .any(|a| a.contains("unknown slash command")));
    }

    #[test]
    // Resize recomputes the layout mode but preserves the user's
    // panel selections.
    fn resize_recomputes_layout_mode() {
        let mut s = fresh_state();
        s.layout.side_panel.mode = SidePanelMode::Changes;
        s.apply(TuiInbound::Resize {
            width: 40,
            height: 6,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Tiny);
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Changes);
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Full);
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Changes);
    }

    #[test]
    // A `//path/...` prompt is the literal-prompt escape: the leading
    // pair of slashes is collapsed to one and submitted unchanged.
    fn double_slash_escapes_to_prompt() {
        let mut s = fresh_state();
        for c in "//var/log/syslog is broken".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(
            actions,
            vec![TuiAction::SubmitPrompt(
                "/var/log/syslog is broken".to_string()
            )]
        );
    }

    #[test]
    // Slash commands accept trailing arguments (Phase H will use them).
    // The command name still resolves and the activity log records it.
    fn slash_command_accepts_arguments() {
        let mut s = fresh_state();
        for c in "/files src/main.rs".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty());
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
    }

    #[test]
    // While a modal is open, the composer must not see Enter / Char
    // events. Esc still pops the modal; Ctrl+C cancels a running run.
    fn modal_blocks_input_to_composer() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        for c in "this should be ignored".chars() {
            let actions = s.apply(TuiInbound::Key(KeyEvent::Char(c)));
            assert!(actions.is_empty());
        }
        let actions = s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(actions.is_empty(), "Enter must not submit while modal open");
        assert!(
            s.composer.buffer.is_empty(),
            "composer should not have absorbed keys"
        );
        // Esc still pops the modal.
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.layout.current_modal().is_none());
    }

    #[test]
    // Ctrl+C with a modal open and a run in flight still cancels the
    // run rather than getting swallowed.
    fn ctrl_c_through_modal_cancels_run() {
        let mut s = fresh_state();
        s.run_status = RunStatus::Running;
        s.layout.push_modal(TuiModal::Help);
        let actions = s.apply(TuiInbound::Key(KeyEvent::CtrlC));
        assert_eq!(actions, vec![TuiAction::CancelRun]);
    }

    #[test]
    // Toggling a panel off while it has focus snaps focus back to the
    // composer so the user always has a visible cursor.
    fn hide_panel_clamps_focus() {
        let mut s = fresh_state();
        // Reach Full mode so panel is rendered.
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus the side panel.
        s.apply(TuiInbound::Key(KeyEvent::Tab)); // Composer → Transcript
        s.apply(TuiInbound::Key(KeyEvent::Tab)); // Transcript → SidePanel
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        // /toggle-side hides it.
        for c in "/toggle-side".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(!s.layout.side_panel.visible);
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    #[test]
    // On a Compact terminal where the side panel is not rendered,
    // `/files` keeps focus on the composer because moving it to an
    // invisible pane would leave the user with no visible cursor.
    fn slash_switch_on_compact_keeps_focus_on_composer() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 70,
            height: 16,
        });
        assert_eq!(s.layout.mode, crate::layout::LayoutMode::Compact);
        for c in "/files".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(s.layout.side_panel.mode, SidePanelMode::Files);
        // Mode is still Compact → side panel not rendered → focus
        // stays on the composer.
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    #[test]
    // Resizing down to Tiny while focus is on a panel clamps focus.
    fn resize_to_tiny_clamps_focus() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::SidePanel);
        s.apply(TuiInbound::Resize {
            width: 40,
            height: 6,
        });
        assert_eq!(s.layout.focus, FocusPane::Composer);
    }

    fn push_user_card(s: &mut TuiAppState, text: &str) {
        s.push_transcript(EventCard {
            kind: "user",
            headline: text.to_string(),
            body: vec![text.to_string()],
            ..Default::default()
        });
    }

    #[test]
    // Each push_transcript assigns a fresh monotonic id; cards retain
    // their ids through subsequent pushes and eviction.
    fn push_transcript_assigns_monotonic_ids() {
        let mut s = fresh_state();
        push_user_card(&mut s, "one");
        push_user_card(&mut s, "two");
        push_user_card(&mut s, "three");
        let ids: Vec<u64> = s.transcript.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
        assert_eq!(s.next_card_id, 3);
    }

    #[test]
    // With transcript focused, j/k navigate selection through visible
    // cards and saturate at the ends.
    fn transcript_nav_j_k_moves_selection() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        push_user_card(&mut s, "a");
        push_user_card(&mut s, "b");
        push_user_card(&mut s, "c");
        // First j with no selection: starts from last card (most recent).
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert_eq!(s.selected_card_id, Some(2));
        // k moves up to b.
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(1));
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(0));
        // Saturates at top.
        s.apply(TuiInbound::Key(KeyEvent::Char('k')));
        assert_eq!(s.selected_card_id, Some(0));
        // g/G jump to top/bottom.
        s.apply(TuiInbound::Key(KeyEvent::Char('G')));
        assert_eq!(s.selected_card_id, Some(2));
        s.apply(TuiInbound::Key(KeyEvent::Char('g')));
        assert_eq!(s.selected_card_id, Some(0));
    }

    #[test]
    // Transcript navigation keys are ignored when focus is on the
    // composer (default). Typing `j` adds it to the composer buffer.
    fn transcript_nav_inert_when_composer_focused() {
        let mut s = fresh_state();
        push_user_card(&mut s, "a");
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        // Composer absorbed the keystroke.
        assert_eq!(s.composer.buffer, "j");
        assert!(s.selected_card_id.is_none());
    }

    #[test]
    // `e` and Enter toggle expansion of the selected card; cards
    // without expanded_body still record the bit so the renderer
    // can decide what to do.
    fn e_toggles_expansion() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        push_user_card(&mut s, "a");
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert!(!s.is_card_expanded(0));
        s.apply(TuiInbound::Key(KeyEvent::Char('e')));
        assert!(s.is_card_expanded(0));
        // Enter also toggles when transcript-focused.
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(!s.is_card_expanded(0));
    }

    #[test]
    // `/search foo` opens the SearchTranscript modal; typing `foo` +
    // Enter commits the query. Visible card list shrinks to matches.
    fn search_modal_filters_transcript() {
        let mut s = fresh_state();
        push_user_card(&mut s, "alpha");
        push_user_card(&mut s, "beta");
        push_user_card(&mut s, "alphabet");
        // Open the modal via slash command.
        for c in "/search".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert!(matches!(
            s.layout.current_modal(),
            Some(TuiModal::SearchTranscript)
        ));
        // Type query.
        for c in "alpha".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert_eq!(s.search_input, "alpha");
        // Commit.
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        assert_eq!(s.search_query.as_deref(), Some("alpha"));
        assert!(s.layout.current_modal().is_none());
        let visible = s.visible_card_indices();
        // alpha + alphabet match; beta does not.
        assert_eq!(visible.len(), 2);
        // Selection jumps to first match.
        assert!(s.selected_card_id.is_some());
    }

    #[test]
    // Backspace removes the last char from the search input; Esc
    // cancels the modal and clears the input without committing.
    fn search_modal_backspace_and_cancel() {
        let mut s = fresh_state();
        for c in "/search".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Enter));
        for c in "wrong".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        s.apply(TuiInbound::Key(KeyEvent::Backspace));
        s.apply(TuiInbound::Key(KeyEvent::Backspace));
        assert_eq!(s.search_input, "wro");
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.layout.current_modal().is_none());
        assert!(s.search_input.is_empty());
        assert!(s.search_query.is_none());
    }

    #[test]
    // n / N navigate forward/backward through search matches when
    // transcript is focused. Without an active query they're no-ops.
    fn n_and_capital_n_navigate_matches() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "alpha");
        push_user_card(&mut s, "beta");
        push_user_card(&mut s, "alphabet");
        s.search_query = Some("alpha".to_string());
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // First n from no selection → most recent match (alphabet, id 2).
        s.apply(TuiInbound::Key(KeyEvent::Char('n')));
        assert_eq!(s.selected_card_id, Some(2));
        s.apply(TuiInbound::Key(KeyEvent::Char('N')));
        assert_eq!(s.selected_card_id, Some(0));
    }

    #[test]
    // Typing characters while the transcript pane is focused is
    // swallowed — they must not leak into the composer buffer.
    fn transcript_focused_swallows_unmapped_chars() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "anchor");
        // Focus transcript.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert_eq!(s.layout.focus, FocusPane::Transcript);
        for c in "agent".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert!(
            s.composer.buffer.is_empty(),
            "composer absorbed transcript-focused keystrokes: {:?}",
            s.composer.buffer
        );
    }

    #[test]
    // With the transcript pane focused and an empty visible set
    // (e.g. zero matches), Up/Down still must not leak to composer.
    fn transcript_focused_empty_visible_swallows_arrows() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        // Focus transcript with nothing in it.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Down));
        s.apply(TuiInbound::Key(KeyEvent::Up));
        s.apply(TuiInbound::Key(KeyEvent::Char('j')));
        assert!(s.composer.buffer.is_empty());
    }

    #[test]
    // Ctrl+C and Ctrl+D still escape from transcript focus.
    fn transcript_focused_ctrl_c_and_d_still_global() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // Ctrl+D from transcript focus with empty composer exits.
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlD)),
            vec![TuiAction::Exit]
        );
        // Ctrl+C with a running run cancels rather than exiting.
        s.run_status = RunStatus::Running;
        assert_eq!(
            s.apply(TuiInbound::Key(KeyEvent::CtrlC)),
            vec![TuiAction::CancelRun]
        );
    }

    #[test]
    // Evicting old cards drops their ids from expanded_card_ids and
    // clears the selection cursor if it pointed at one of them.
    fn ring_buffer_eviction_cleans_orphan_ids() {
        let mut s = fresh_state();
        // Fill the transcript right up to its cap and expand the
        // first three cards.
        for i in 0..TRANSCRIPT_CAP {
            push_user_card(&mut s, &format!("c{i}"));
            if i < 3 {
                s.expanded_card_ids.insert(i as u64);
            }
        }
        // Point selection at the card that is about to be evicted.
        s.selected_card_id = Some(0);
        // Push one more card — id 0 is now drained.
        push_user_card(&mut s, "tail");
        assert!(!s.expanded_card_ids.contains(&0));
        assert!(s.selected_card_id.is_none());
        // Push two more to drain ids 1 and 2 → their expansion bits
        // also clear.
        push_user_card(&mut s, "tail2");
        push_user_card(&mut s, "tail3");
        assert!(!s.expanded_card_ids.contains(&1));
        assert!(!s.expanded_card_ids.contains(&2));
    }

    #[test]
    // Pressing `e` while selection points at an evicted card
    // resolves to the visible cursor and toggles its expansion
    // instead.
    fn expand_resolves_evicted_selection_to_cursor() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        push_user_card(&mut s, "a");
        push_user_card(&mut s, "b");
        // Pretend selection points at a card that no longer exists.
        s.selected_card_id = Some(99);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        s.apply(TuiInbound::Key(KeyEvent::Char('e')));
        // Selection now points at the visible cursor (most recent),
        // and that card is expanded.
        let new_selected = s.selected_card_id.unwrap();
        assert!(s.is_card_expanded(new_selected));
        // The vanished id was not toggled.
        assert!(!s.is_card_expanded(99));
    }

    #[test]
    // Typing `/pl` opens a Slash popup with `plan` / `plugins` / etc.
    // among the candidates. Tab accepts the highlighted candidate
    // and replaces the trigger range in the composer buffer.
    fn slash_popup_opens_and_accepts() {
        let mut s = fresh_state();
        for c in "/pl".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let popup = s.composer_popup.as_ref().expect("popup should be open");
        assert_eq!(popup.kind, crate::completion::CompletionKind::Slash);
        assert!(!popup.candidates.is_empty(), "expected candidates");
        // Tab accepts. The first candidate (`/plan`) replaces the
        // trigger range; the popup closes.
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        assert!(s.composer_popup.is_none(), "popup should close on accept");
        assert!(
            s.composer.buffer.starts_with("/p"),
            "buffer should contain inserted candidate: {:?}",
            s.composer.buffer
        );
    }

    #[test]
    // Typing `@cha` opens a ContextToken popup including `@changed`.
    // Esc closes the popup without altering the buffer.
    fn at_context_token_popup_opens_and_closes_on_esc() {
        let mut s = fresh_state();
        for c in "@cha".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let popup = s.composer_popup.as_ref().expect("popup should be open");
        assert_eq!(popup.kind, crate::completion::CompletionKind::ContextToken);
        assert!(popup.candidates.iter().any(|c| c.display == "@changed"));
        s.apply(TuiInbound::Key(KeyEvent::Esc));
        assert!(s.composer_popup.is_none());
        assert_eq!(s.composer.buffer, "@cha");
    }

    #[test]
    // Up/Down navigate the popup selection without touching the buffer.
    fn popup_up_down_navigates_selection() {
        let mut s = fresh_state();
        for c in "/p".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        let first = s.composer_popup.as_ref().unwrap().selected;
        s.apply(TuiInbound::Key(KeyEvent::Down));
        let next = s.composer_popup.as_ref().unwrap().selected;
        assert_ne!(first, next);
        s.apply(TuiInbound::Key(KeyEvent::Up));
        let back = s.composer_popup.as_ref().unwrap().selected;
        assert_eq!(back, first);
    }

    #[test]
    // A small paste is inserted at the cursor and produces no warning.
    fn small_paste_inserts_without_warning() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Paste("hello world".to_string()));
        assert_eq!(s.composer.buffer, "hello world");
        assert_eq!(s.composer.cursor, "hello world".len());
        assert!(s.activity.iter().all(|a| !a.contains("paste")));
    }

    #[test]
    // A paste over PASTE_WARN_BYTES surfaces an activity entry.
    fn large_paste_logs_warning() {
        let mut s = fresh_state();
        let data: String = "x".repeat(PASTE_WARN_BYTES + 100);
        s.apply(TuiInbound::Paste(data));
        assert!(s.activity.iter().any(|a| a.contains("large paste")));
    }

    #[test]
    // A paste over PASTE_HARD_LIMIT is truncated to the cap.
    fn huge_paste_is_truncated() {
        let mut s = fresh_state();
        let data: String = "x".repeat(PASTE_HARD_LIMIT * 2);
        s.apply(TuiInbound::Paste(data));
        assert_eq!(s.composer.buffer.len(), PASTE_HARD_LIMIT);
        assert!(s.activity.iter().any(|a| a.contains("paste truncated")));
    }

    #[test]
    // Accepting a mid-word completion replaces both the prefix and
    // the suffix of the token under the cursor, not just the part
    // before the cursor. Without this fix `/pl|an` + Tab would
    // produce `/planan` (suffix duplication).
    fn accept_completion_replaces_whole_token() {
        let mut s = fresh_state();
        for c in "/plugins".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        // Move the cursor back four characters so it sits between
        // `/pl` and `ugins` — the popup's selected candidate is
        // `/plan` (alphabetically first /pl* match).
        s.composer.cursor = 3;
        s.refresh_composer_popup();
        let popup = s.composer_popup.as_ref().expect("popup");
        assert_eq!(popup.kind, crate::completion::CompletionKind::Slash);
        s.apply(TuiInbound::Key(KeyEvent::Tab));
        // Suffix `ugins` must be replaced, not duplicated.
        assert!(
            !s.composer.buffer.contains("ugins"),
            "suffix duplication: {:?}",
            s.composer.buffer
        );
        // The accepted candidate (whatever the popup picked) is in
        // the buffer.
        assert!(s.composer.buffer.starts_with('/'));
    }

    #[test]
    // Cycling focus away from the composer closes the completion
    // popup, so popup keys (Tab/Up/Down/Esc) don't hijack the new
    // pane. Tab itself is consumed by the popup-accept path when
    // the popup is open, so the cycle is exercised by ShiftTab,
    // which falls through to the layout dispatch.
    fn focus_cycle_clears_completion_popup() {
        let mut s = fresh_state();
        s.apply(TuiInbound::Resize {
            width: 120,
            height: 30,
        });
        for c in "/p".chars() {
            s.apply(TuiInbound::Key(KeyEvent::Char(c)));
        }
        assert!(s.composer_popup.is_some());
        s.apply(TuiInbound::Key(KeyEvent::ShiftTab));
        assert!(
            !matches!(s.layout.focus, FocusPane::Composer),
            "focus should have moved off composer"
        );
        assert!(
            s.composer_popup.is_none(),
            "popup should close when focus leaves composer"
        );
    }

    #[test]
    // Paste is ignored while a modal is open so the underlying
    // composer never gets corrupted by a clipboard payload that was
    // meant for the modal.
    fn paste_under_modal_is_swallowed() {
        let mut s = fresh_state();
        s.layout.push_modal(TuiModal::Help);
        s.apply(TuiInbound::Paste("should be ignored".to_string()));
        assert!(s.composer.buffer.is_empty());
    }

    #[test]
    // Transcript length is bounded to TRANSCRIPT_CAP.
    fn transcript_is_bounded() {
        let mut s = fresh_state();
        for i in 0..(TRANSCRIPT_CAP + 10) {
            s.push_transcript(EventCard {
                kind: "test",
                headline: i.to_string(),
                body: Vec::new(),
                ..Default::default()
            });
        }
        assert_eq!(s.transcript.len(), TRANSCRIPT_CAP);
        // Oldest cards (0..10) were dropped.
        assert_eq!(s.transcript.first().unwrap().headline, "10");
    }
}
