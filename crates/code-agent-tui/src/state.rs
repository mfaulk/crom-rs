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
use crate::transcript::EventCard;

/// Maximum number of transcript cards retained in state. Bounded so a
/// long-running session doesn't grow unbounded memory; later phases will
/// virtualize scrollback over a larger backing store.
pub const TRANSCRIPT_CAP: usize = 1024;

/// Maximum number of activity-log entries retained.
pub const ACTIVITY_CAP: usize = 16;

/// Side-effect intents emitted by the reducer for the host loop to handle.
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

/// Top-level TUI state. Intentionally small for Phase A: later phases
/// add side/detail panels, modal stack, plan dashboard, etc., without
/// breaking the reducer surface.
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
}

impl TuiAppState {
    /// Construct a new state from a composer state and an initial status
    /// context.
    pub fn new(
        composer: ComposerState,
        status_template: String,
        status_ctx: StatusLineContext,
    ) -> Self {
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
        }
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
                Vec::new()
            }
        }
    }

    fn apply_key(&mut self, k: KeyEvent) -> Vec<TuiAction> {
        match self.composer.handle_key(k) {
            KeyAction::Submit(prompt) => {
                let trimmed = prompt.trim();
                if trimmed.is_empty() {
                    return Vec::new();
                }
                // Reject prompts while a run is in flight so we don't
                // optimistically append a "You:" card for a prompt that
                // would then be silently dropped by the host loop. The
                // user sees a diagnostic; a future phase can queue.
                if matches!(self.run_status, RunStatus::Running) {
                    self.push_activity("ignored prompt: run already in progress".to_string());
                    return Vec::new();
                }
                let prompt = trimmed.to_string();
                self.push_transcript(EventCard {
                    kind: "user",
                    headline: "You:".to_string(),
                    body: prompt.lines().map(String::from).collect(),
                });
                self.run_status = RunStatus::Running;
                self.last_error = None;
                vec![TuiAction::SubmitPrompt(prompt)]
            }
            KeyAction::Interrupt => match self.run_status {
                RunStatus::Running => vec![TuiAction::CancelRun],
                RunStatus::Idle => vec![TuiAction::Exit],
            },
            KeyAction::Exit => vec![TuiAction::Exit],
            KeyAction::None | KeyAction::HistorySearch => Vec::new(),
        }
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
