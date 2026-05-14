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

use code_agent_core::StatusLineContext;
use code_agent_sdk::SdkEvent;

use crate::composer::{ComposerState, KeyAction, KeyEvent};
use crate::event_adapter::map_sdk_event;
use crate::layout::{DetailPanelMode, FocusPane, LayoutState, SidePanelMode, TuiModal};
use crate::transcript::EventCard;

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

/// Top-level TUI state. Phase B adds `layout` for the multi-pane
/// cockpit. Later phases extend `LayoutState` (plan dashboard payload,
/// inspector data, session list, etc.) without breaking the reducer
/// surface.
#[derive(Debug, Clone)]
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
        }
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

    fn apply_key(&mut self, k: KeyEvent) -> Vec<TuiAction> {
        // Layout-priority keys intercepted before the composer. These
        // are global navigation that does not depend on which pane has
        // focus, so the composer never sees them.
        match k {
            KeyEvent::Tab => {
                self.layout.focus = self.layout.focus.next(
                    self.layout.side_panel_rendered(),
                    self.layout.detail_panel_rendered(),
                );
                return Vec::new();
            }
            KeyEvent::ShiftTab => {
                self.layout.focus = self.layout.focus.prev(
                    self.layout.side_panel_rendered(),
                    self.layout.detail_panel_rendered(),
                );
                return Vec::new();
            }
            _ => {}
        }
        // When a modal is open, swallow most input so a key doesn't
        // accidentally mutate the composer or submit a prompt behind
        // the overlay. Esc still pops the modal; Ctrl+C still cancels
        // the active run or exits.
        if self.layout.current_modal().is_some() {
            return match k {
                KeyEvent::Esc => {
                    let _ = self.layout.pop_modal();
                    Vec::new()
                }
                KeyEvent::CtrlC => match self.run_status {
                    RunStatus::Running => vec![TuiAction::CancelRun],
                    RunStatus::Idle => Vec::new(),
                },
                _ => Vec::new(),
            };
        }
        match self.composer.handle_key(k) {
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
        });
    }

    fn push_transcript(&mut self, card: EventCard) {
        self.transcript.push(card);
        // Bounded ring buffer; oldest cards drop first.
        if self.transcript.len() > TRANSCRIPT_CAP {
            let excess = self.transcript.len() - TRANSCRIPT_CAP;
            self.transcript.drain(..excess);
        }
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

    #[test]
    // Transcript length is bounded to TRANSCRIPT_CAP.
    fn transcript_is_bounded() {
        let mut s = fresh_state();
        for i in 0..(TRANSCRIPT_CAP + 10) {
            s.push_transcript(EventCard {
                kind: "test",
                headline: i.to_string(),
                body: Vec::new(),
            });
        }
        assert_eq!(s.transcript.len(), TRANSCRIPT_CAP);
        // Oldest cards (0..10) were dropped.
        assert_eq!(s.transcript.first().unwrap().headline, "10");
    }
}
