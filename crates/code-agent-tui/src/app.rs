//! Live TUI application shell.
//!
//! `TuiApp` owns the ratatui terminal, the SDK [`AgentRuntime`], and a
//! [`TuiAppState`] reducer. It wires three concurrent inputs into the
//! reducer:
//!
//! 1. keyboard / resize events from `crossterm`,
//! 2. SDK events from the currently-active `AgentRun` (if any),
//! 3. periodic ticks for steady redraws.
//!
//! The reducer is pure; all terminal I/O, runtime calls, and async work
//! happen here. Tests exercise the reducer directly to avoid needing a
//! terminal — see `state.rs` for the bulk of behavioral coverage.

use std::io::{self, Stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, KeyCode, KeyEventKind,
    KeyModifiers,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use futures::stream::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::{mpsc, oneshot};

use code_agent_sdk::callback::ApprovalDecision;
use code_agent_sdk::{AgentRequest, AgentRun, AgentRuntime, AgentSdkError, SdkEvent};

use crate::approval_bridge::BridgeRequest;
use crate::composer::KeyEvent;
use crate::fullscreen::draw;
use crate::state::{ApprovalDecisionAction, TuiAction, TuiAppState, TuiInbound};

/// Errors raised by the live TUI loop.
#[derive(Debug, thiserror::Error)]
pub enum TuiAppError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("sdk: {0}")]
    Sdk(String),
}

impl From<AgentSdkError> for TuiAppError {
    fn from(e: AgentSdkError) -> Self {
        Self::Sdk(e.message)
    }
}

/// Cooperative tick rate for steady redraws while idle.
const TICK_RATE: Duration = Duration::from_millis(120);

/// The live TUI application. Construct with [`TuiApp::new`] then await
/// [`TuiApp::run`] to drive the terminal loop until the user exits.
pub struct TuiApp {
    runtime: Arc<AgentRuntime>,
    state: TuiAppState,
    footer: String,
    /// Phase G: receiver paired with an
    /// [`crate::approval_bridge::ApprovalBridge`] handed to the SDK.
    /// When the SDK calls the bridge's `approve()`, the request lands
    /// here. `None` if the host didn't wire a bridge — in that case
    /// `ResolveApproval` actions only log to the activity panel.
    bridge_rx: Option<mpsc::Receiver<BridgeRequest>>,
    /// Phase G: oneshot resolver for the in-flight approval, if any.
    /// Set when the bridge forwards a request; cleared and consumed
    /// when the user resolves the approval modal.
    pending_resolver: Option<oneshot::Sender<ApprovalDecision>>,
}

impl TuiApp {
    /// Construct a new app. The caller supplies a built [`AgentRuntime`]
    /// (whose provider may be real or fake) and the initial reducer state.
    pub fn new(runtime: Arc<AgentRuntime>, state: TuiAppState) -> Self {
        Self {
            runtime,
            state,
            footer: "[?] help  [Ctrl+C] interrupt  [Ctrl+D] exit".to_string(),
            bridge_rx: None,
            pending_resolver: None,
        }
    }

    /// Replace the help footer text.
    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = footer.into();
        self
    }

    /// Phase G: register an approval-bridge receiver so the SDK's
    /// `ApprovalCallback` round-trips through the TUI modal. Pair
    /// with [`crate::approval_bridge::new_bridge_pair`].
    pub fn with_approval_bridge(mut self, rx: mpsc::Receiver<BridgeRequest>) -> Self {
        self.bridge_rx = Some(rx);
        self
    }

    /// Run the live event loop on the host terminal. Returns when the user
    /// exits. The terminal is always restored on return (including on
    /// error paths), so callers can swallow the error and print a normal
    /// message without leaving the terminal in raw mode.
    pub async fn run(mut self) -> Result<(), TuiAppError> {
        let mut guard = TerminalGuard::enter()?;
        let backend = CrosstermBackend::new(guard.stdout_handle());
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;

        let (input_tx, mut input_rx) = mpsc::channel::<TuiInbound>(64);
        // #154: a shared shutdown flag the input thread polls so it can
        // exit cleanly. The thread runs a blocking `crossterm::event::read()`
        // which `abort()` cannot interrupt — a JoinHandle abort only
        // cancels at an await point, and a thread parked inside a blocking
        // syscall has none. By polling with a timeout and checking this
        // flag each tick, the thread observes the shutdown request and
        // returns before the TerminalGuard restores the terminal, so a
        // stray keystroke can't be read against (and corrupt) the
        // post-exit terminal.
        let input_shutdown = Arc::new(AtomicBool::new(false));
        let input_task = spawn_input_task(input_tx, input_shutdown.clone());

        // Active run. None when idle. We pull events off `run.events` in
        // the select! loop and feed them into the reducer.
        let mut active_run: Option<AgentRun> = None;
        let mut cancel: Option<code_agent_sdk::CancellationHandle> = None;

        let mut tick = tokio::time::interval(TICK_RATE);
        // The first tick fires immediately; skip it so we don't spin.
        tick.tick().await;

        self.draw(&mut terminal)?;

        loop {
            tokio::select! {
                biased;

                // Keyboard / resize events. `recv()` returns None if the
                // input task panicked or `crossterm::event::read()` failed
                // — without explicit handling the loop would spin on the
                // remaining branches with no input path. Treat a closed
                // input channel as a fatal stop and surface a diagnostic.
                inbound = input_rx.recv() => {
                    let Some(inbound) = inbound else {
                        self.state.apply(TuiInbound::RunStreamError(
                            "terminal input channel closed".to_string(),
                        ));
                        break;
                    };
                    let actions = self.state.apply(inbound);
                    self.handle_actions(actions, &mut active_run, &mut cancel).await;
                    // Drain any other input events that arrived while we
                    // were processing this one. Without this, a bracketed
                    // paste of N characters triggers N full-frame redraws
                    // and starves the SDK event arm.
                    while let Ok(extra) = input_rx.try_recv() {
                        let extra_actions = self.state.apply(extra);
                        self.handle_actions(extra_actions, &mut active_run, &mut cancel).await;
                        if self.state.should_quit {
                            break;
                        }
                    }
                    if self.state.should_quit {
                        break;
                    }
                    self.draw(&mut terminal)?;
                }

                // SDK events from the currently-active run. The `if` guard
                // disables the branch entirely while idle so we never poll
                // a stream that doesn't exist.
                Some(event) = next_run_event(active_run.as_mut()), if active_run.is_some() => {
                    match event {
                        RunEvent::Sdk(ev) => {
                            // #209: the reducer flips `run_status` back to
                            // `Idle` on a terminal SDK event (RunCompleted /
                            // RunFailed / RunCancelled), but the stream only
                            // yields `None` (→ `RunEvent::Ended`) one tick
                            // later. In that window a freshly-submitted prompt
                            // passes the reducer's Idle guard yet gets dropped
                            // by `handle_actions` because `active_run` is still
                            // `Some`. Clear the run as soon as we observe the
                            // terminal event so the next submit actually
                            // starts — treat it like `Ended`.
                            // Match the terminal variants directly rather
                            // than building a full `SdkEventOutcome` (with its
                            // preview allocations) just to read `run_terminal`
                            // — this runs on every SDK event on the hot path.
                            let terminal = matches!(
                                ev,
                                SdkEvent::RunCompleted(_)
                                    | SdkEvent::RunFailed(_)
                                    | SdkEvent::RunCancelled(_)
                            );
                            self.state.apply(TuiInbound::sdk(ev));
                            if terminal {
                                active_run = None;
                                cancel = None;
                            }
                        }
                        RunEvent::Error(message) => {
                            self.state.apply(TuiInbound::RunStreamError(message));
                            active_run = None;
                            cancel = None;
                        }
                        RunEvent::Ended => {
                            self.state.apply(TuiInbound::RunStreamEnded);
                            active_run = None;
                            cancel = None;
                        }
                    }
                    self.draw(&mut terminal)?;
                }

                // Phase G: bridge-borne approval requests from the SDK.
                // The `if` guard disables the branch when no bridge is
                // wired. On receive we:
                // 1. Synthesize the approval modal directly via
                //    `open_approval_modal` so the user always sees a
                //    prompt — relying on `SdkEvent::ApprovalRequired`
                //    arriving separately would deadlock the SDK if
                //    the runtime suppresses the event (e.g. embedded
                //    hosts that wire only the callback).
                // 2. Stash the resolver so the next
                //    `TuiAction::ResolveApproval` can answer it. With
                //    a capacity-1 bridge channel the SDK back-
                //    pressures on concurrent approvals, but we
                //    defensively deny any stale resolver so a stuck
                //    `pending_resolver` (e.g. after a panic-recovered
                //    handler) never strands the SDK.
                // #152: bind the full `Option` rather than gating on a
                // `Some(..)` pattern. A `Some(_) = ..` arm leaves the `if`
                // guard `true` after the channel closes, so `recv_bridge`
                // returns `Ready(None)` and `select!` re-polls this arm on
                // every loop iteration — a busy spin under `biased`. When
                // we observe the close (`None`), drop the receiver so the
                // guard disables the arm permanently.
                maybe_req = recv_bridge(self.bridge_rx.as_mut()), if self.bridge_rx.is_some() => {
                    let Some(bridge_req) = maybe_req else {
                        self.bridge_rx = None;
                        continue;
                    };
                    if let Some(old) = self.pending_resolver.take() {
                        let _ = old.send(ApprovalDecision::Deny {
                            reason: "superseded by a new approval".to_string(),
                        });
                    }
                    // #177: the bridge is the single source of truth for an
                    // approval that has a live SDK resolver. `open_approval_modal`
                    // is idempotent on the modal stack, so if the
                    // `SdkEvent::ApprovalRequired` reducer arm already opened a
                    // resolver-less modal for this request, this call replaces it
                    // with one whose resolver we own below — no duplicate modal.
                    self.state.open_approval_modal(
                        bridge_req.request.tool_name.clone(),
                        bridge_req.request.summary.clone(),
                    );
                    self.pending_resolver = Some(bridge_req.decision_tx);
                    self.state
                        .push_activity(format!("approval requested: {}", bridge_req.request.tool_name));
                    self.draw(&mut terminal)?;
                }

                _ = tick.tick() => {
                    self.draw(&mut terminal)?;
                }
            }
        }

        // Best-effort cancel of any still-running task before we tear
        // down. The runtime owns the JoinHandle internally; cancelling
        // here lets the background task observe and exit promptly.
        if let Some(c) = cancel.take() {
            c.cancel();
        }
        drop(active_run);
        // #154: signal the input thread to stop, then wait for it to
        // actually return before restoring the terminal. The thread polls
        // `input_shutdown` between event polls, so it observes the request
        // within one poll interval and exits its loop. Joining here (rather
        // than the old `abort()`, which cannot interrupt a thread parked in
        // a blocking `event::read()`) guarantees no input read is in flight
        // when `guard.leave()` puts the terminal back into cooked mode, so
        // a stray keystroke during teardown can't be consumed against the
        // restored terminal.
        input_shutdown.store(true, Ordering::Relaxed);
        // Close the receiver before joining. The input thread sends over a
        // *bounded* channel; if it is parked in `blocking_send` on a full
        // channel during an exit burst it never reaches the `input_shutdown`
        // check, and while `input_rx` is still alive that send never errors —
        // so `input_task.await` would hang forever, leaving the terminal in
        // raw mode. Closing the receiver makes the pending/next send fail so
        // the thread unblocks and exits.
        input_rx.close();
        let _ = input_task.await;
        guard.leave()?;
        Ok(())
    }

    async fn handle_actions(
        &mut self,
        actions: Vec<TuiAction>,
        active_run: &mut Option<AgentRun>,
        cancel: &mut Option<code_agent_sdk::CancellationHandle>,
    ) {
        for action in actions {
            match action {
                TuiAction::SubmitPrompt(prompt) => {
                    if active_run.is_some() {
                        // Already running; ignore. (A future phase can
                        // queue the prompt or refuse with a diagnostic.)
                        continue;
                    }
                    match self.runtime.run(AgentRequest::new(prompt)).await {
                        Ok(run) => {
                            *cancel = Some(run.cancellation.clone());
                            *active_run = Some(run);
                        }
                        Err(e) => {
                            self.state
                                .apply(TuiInbound::RunStreamError(e.message.clone()));
                        }
                    }
                }
                TuiAction::CancelRun => {
                    if let Some(c) = cancel.as_ref() {
                        c.cancel();
                    }
                }
                TuiAction::Exit => {
                    self.state.should_quit = true;
                }
                // M13 Phase G: live wiring of the approval decision
                // back to the SDK callback via the resolver that the
                // bridge stashed on the previous bridge event. If no
                // resolver is stored (no bridge wired, or a synthetic
                // `ApprovalRequired` event without a matching bridge
                // call) we still log the decision so reducer-only
                // tests of the workbench remain useful.
                // #27: the former ApprovePlan/RejectPlan/RevisePlan arms
                // were removed along with their TuiAction variants. They
                // only logged a "[preview]" line while the SDK
                // auto-approved the plan internally, so an operator who
                // "rejected" a plan still saw it run — the UI lied about
                // gating execution. The plan-approval modal is now an
                // informational notice that emits no decision action.
                // Wiring a real host-side PlanDecisionCallback that can
                // actually block a run is the tracked follow-up
                // (issue #27 option 1).
                TuiAction::ResolveApproval(decision) => {
                    self.resolve_approval(decision);
                }
            }
        }
    }

    /// Dispatch a resolved approval decision back to the SDK via the
    /// stashed bridge resolver.
    ///
    /// #177: the approval modal is opened from two uncoordinated paths
    /// (the bridge handler and the `SdkEvent::ApprovalRequired` reducer
    /// arm). Only the bridge handler stashes a `pending_resolver`. When a
    /// resolver is present we send the decision exactly once (the
    /// `take()` guarantees it cannot be sent twice). When it is absent the
    /// decision has nowhere to go — expected for reducer-only hosts with
    /// no bridge, but a bug if a bridge is wired and the request was
    /// already answered or never landed. Either way we surface a visible
    /// diagnostic in the activity log rather than silently dropping the
    /// decision.
    fn resolve_approval(&mut self, decision: ApprovalDecisionAction) {
        let sdk_decision = sdk_decision_from(decision.clone());
        self.state
            .push_activity(format!("approval decision: {decision:?}"));
        if let Some(resolver) = self.pending_resolver.take() {
            let _ = resolver.send(sdk_decision);
        } else {
            self.state.push_activity(format!(
                "warning: approval decision ({decision:?}) had no pending request \
                 to resolve; not sent to the SDK"
            ));
        }
    }

    fn draw(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), TuiAppError> {
        let frame = crate::view::build_ui_frame(&self.state, &self.footer);
        draw(terminal, frame)?;
        Ok(())
    }
}

/// Poll interval for the input thread's `crossterm::event::poll`. Short
/// enough that a shutdown request is observed promptly (worst-case
/// teardown latency), long enough that an idle terminal doesn't busy-spin.
const INPUT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// #154: decide whether the input thread should keep looping. Pulled out
/// as a free function so the shutdown-flag contract is unit-testable
/// without a terminal: returns `false` (stop) once the shared flag is
/// set, `true` (keep polling) otherwise.
fn input_should_continue(shutdown: &AtomicBool) -> bool {
    !shutdown.load(Ordering::Relaxed)
}

/// Spawn a blocking task that reads crossterm events and forwards them
/// (decoded into our abstract composer `KeyEvent`) to the main loop.
///
/// #154: rather than parking in a blocking `crossterm::event::read()`
/// (which `abort()` cannot interrupt), the thread polls with a timeout
/// and checks `shutdown` each tick. When the main loop sets the flag on
/// teardown, the thread observes it within one `INPUT_POLL_INTERVAL` and
/// returns cleanly — before the terminal is restored.
fn spawn_input_task(
    tx: mpsc::Sender<TuiInbound>,
    shutdown: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while input_should_continue(&shutdown) {
            // Wait up to INPUT_POLL_INTERVAL for an event. On timeout
            // (`Ok(false)`) loop back to re-check the shutdown flag. A
            // poll error means the terminal is gone, so stop.
            match crossterm::event::poll(INPUT_POLL_INTERVAL) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(_) => break,
            }
            match crossterm::event::read() {
                Ok(CtEvent::Key(k)) => {
                    if matches!(k.kind, KeyEventKind::Release) {
                        continue;
                    }
                    if let Some(decoded) = decode_key(k) {
                        if tx.blocking_send(TuiInbound::Key(decoded)).is_err() {
                            break;
                        }
                    }
                }
                Ok(CtEvent::Resize(w, h)) => {
                    if tx
                        .blocking_send(TuiInbound::Resize {
                            width: w,
                            height: h,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(CtEvent::Paste(data)) => {
                    // M13 Phase D: emit a single Paste event so the
                    // reducer can compute size, warn / truncate on
                    // large payloads, and bulk-insert in one go. The
                    // old per-character forwarding starved the
                    // SDK arm on multi-KB pastes.
                    if tx.blocking_send(TuiInbound::Paste(data)).is_err() {
                        return;
                    }
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
    })
}

fn decode_key(k: crossterm::event::KeyEvent) -> Option<KeyEvent> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    match k.code {
        KeyCode::Char(c) => {
            if ctrl {
                match c.to_ascii_lowercase() {
                    'c' => Some(KeyEvent::CtrlC),
                    'd' => Some(KeyEvent::CtrlD),
                    'r' => Some(KeyEvent::CtrlR),
                    _ => None,
                }
            } else {
                Some(KeyEvent::Char(c))
            }
        }
        KeyCode::Enter => {
            if shift {
                Some(KeyEvent::ShiftEnter)
            } else if alt {
                Some(KeyEvent::AltEnter)
            } else if ctrl {
                Some(KeyEvent::CtrlEnter)
            } else {
                Some(KeyEvent::Enter)
            }
        }
        KeyCode::Backspace => Some(KeyEvent::Backspace),
        KeyCode::Tab => {
            if shift {
                Some(KeyEvent::ShiftTab)
            } else {
                Some(KeyEvent::Tab)
            }
        }
        KeyCode::BackTab => Some(KeyEvent::ShiftTab),
        KeyCode::Up => Some(KeyEvent::Up),
        KeyCode::Down => Some(KeyEvent::Down),
        KeyCode::Left => Some(KeyEvent::Left),
        KeyCode::Right => Some(KeyEvent::Right),
        KeyCode::Esc => Some(KeyEvent::Esc),
        _ => None,
    }
}

// `Sdk` is materially larger than the other variants because `SdkEvent`
// is large; we deliberately keep it on the stack here since the value is
// created once per stream tick and immediately matched on, with no
// long-lived storage.
#[allow(clippy::large_enum_variant)]
enum RunEvent {
    Sdk(SdkEvent),
    Error(String),
    Ended,
}

async fn next_run_event(run: Option<&mut AgentRun>) -> Option<RunEvent> {
    let run = run?;
    match run.events.next().await {
        Some(Ok(ev)) => Some(RunEvent::Sdk(ev)),
        Some(Err(e)) => Some(RunEvent::Error(e.message)),
        None => Some(RunEvent::Ended),
    }
}

/// Phase G: receive the next approval bridge request, gated on the
/// optional receiver. Returning `None` indicates the channel was
/// closed (the bridge `Arc` was dropped), which the caller treats as
/// a permanent disable.
async fn recv_bridge(rx: Option<&mut mpsc::Receiver<BridgeRequest>>) -> Option<BridgeRequest> {
    rx?.recv().await
}

/// Phase G: translate the reducer's transport-agnostic
/// [`ApprovalDecisionAction`] into the SDK's
/// [`code_agent_sdk::callback::ApprovalDecision`]. The reducer keeps
/// `RequestRevision` as its own variant; the SDK doesn't model it
/// directly, so we map it onto `Deny` with a reason that explains the
/// intent. Future SDK extension can fold revision in as a first-class
/// decision.
fn sdk_decision_from(d: ApprovalDecisionAction) -> ApprovalDecision {
    match d {
        ApprovalDecisionAction::Allow => ApprovalDecision::Allow,
        ApprovalDecisionAction::AllowForSession => ApprovalDecision::AllowForSession,
        ApprovalDecisionAction::Deny { reason } => ApprovalDecision::Deny { reason },
        ApprovalDecisionAction::RequestRevision => ApprovalDecision::Deny {
            reason: "user requested revision".to_string(),
        },
    }
}

/// RAII terminal guard: enables raw mode + alternate screen on
/// construction, restores them on drop. Idempotent on leave so the
/// caller can call `leave()` explicitly to surface I/O errors before
/// the drop runs.
struct TerminalGuard {
    entered: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        // Instantiate the guard immediately after raw mode is enabled so
        // that any subsequent setup failure still triggers the Drop-based
        // restoration. Without this, a failure in `queue!` or `flush`
        // would leak raw mode permanently.
        enable_raw_mode()?;
        let mut guard = Self { entered: true };
        let mut stdout = io::stdout();
        if let Err(e) = queue!(stdout, EnterAlternateScreen, EnableBracketedPaste) {
            let _ = guard.leave();
            return Err(e);
        }
        if let Err(e) = stdout.flush() {
            let _ = guard.leave();
            return Err(e);
        }
        Ok(guard)
    }

    fn stdout_handle(&self) -> Stdout {
        io::stdout()
    }

    /// Tear the terminal back down. Runs every step even if an earlier
    /// one fails — disabling raw mode is the most important step and
    /// must not be skipped just because `LeaveAlternateScreen` returned
    /// an I/O error. Returns the first error encountered, if any.
    fn leave(&mut self) -> io::Result<()> {
        if !self.entered {
            return Ok(());
        }
        let mut first_err: Option<io::Error> = None;
        let mut stdout = io::stdout();
        if let Err(e) = execute!(stdout, DisableBracketedPaste, LeaveAlternateScreen) {
            first_err.get_or_insert(e);
        }
        if let Err(e) = disable_raw_mode() {
            first_err.get_or_insert(e);
        }
        // Only mark the guard as left after teardown has been attempted
        // for every step, so a partial failure cannot trick Drop into
        // skipping the remaining cleanup.
        self.entered = false;
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.entered {
            // Best-effort cleanup on panic. Errors are unrecoverable here.
            let _ = self.leave();
        }
    }
}

/// Convenience for tests and integration callers: simulate the user
/// typing `prompt` into the composer and pressing Enter, then drain
/// the resulting `AgentRun` event stream into the reducer. Returns
/// when the run stream ends or errors. No terminal I/O.
pub async fn drive_single_prompt(
    runtime: &AgentRuntime,
    state: &mut TuiAppState,
    prompt: impl Into<String>,
) -> Result<(), AgentSdkError> {
    let prompt = prompt.into();
    state.composer.buffer.clear();
    state.composer.cursor = 0;
    for c in prompt.chars() {
        state.apply(TuiInbound::Key(KeyEvent::Char(c)));
    }
    let actions = state.apply(TuiInbound::Key(KeyEvent::Enter));
    let prompt_for_run = match actions.into_iter().next() {
        Some(TuiAction::SubmitPrompt(p)) => p,
        _ => prompt,
    };
    let mut run = runtime.run(AgentRequest::new(prompt_for_run)).await?;
    while let Some(ev) = run.events.next().await {
        match ev {
            Ok(sdk) => {
                state.apply(TuiInbound::sdk(sdk));
            }
            Err(e) => {
                state.apply(TuiInbound::RunStreamError(e.message));
                return Ok(());
            }
        }
    }
    state.apply(TuiInbound::RunStreamEnded);
    let _ = run.final_result().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // #154: the input thread keeps polling until the shared shutdown
    // flag is set, then stops. The blocking `event::read()` loop can't be
    // exercised without a terminal, but this predicate is the load-bearing
    // contract that lets the thread exit cleanly on teardown.
    #[test]
    fn input_thread_stops_when_shutdown_flag_set() {
        let flag = AtomicBool::new(false);
        assert!(
            input_should_continue(&flag),
            "should keep polling while flag is clear"
        );
        flag.store(true, Ordering::Relaxed);
        assert!(
            !input_should_continue(&flag),
            "should stop once flag is set"
        );
    }

    use crate::composer::ComposerState;
    use code_agent_core::{ComposerConfig, StatusLineContext};
    use code_agent_sdk::fake_provider::EchoFakeProvider;
    use code_agent_sdk::ProviderConfig;

    async fn test_app() -> TuiApp {
        let runtime = AgentRuntime::builder()
            .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
            .build()
            .await
            .expect("runtime");
        let state = TuiAppState::new(
            ComposerState::new(ComposerConfig::default()),
            "{run_status}".to_string(),
            StatusLineContext::default(),
        );
        TuiApp::new(Arc::new(runtime), state)
    }

    // #177: a resolved decision with a stashed resolver is sent exactly
    // once, and the resolver is consumed (so it can never fire twice).
    #[tokio::test]
    async fn resolve_approval_signals_resolver_once() {
        let mut app = test_app().await;
        let (tx, rx) = oneshot::channel::<ApprovalDecision>();
        app.pending_resolver = Some(tx);
        app.resolve_approval(ApprovalDecisionAction::Allow);
        // Decision reached the SDK side exactly once.
        assert_eq!(rx.await, Ok(ApprovalDecision::Allow));
        // Resolver was consumed — a second resolve cannot double-send.
        assert!(app.pending_resolver.is_none());
        // No "no pending request" warning when a resolver was present.
        assert!(
            !app.state
                .activity
                .iter()
                .any(|l| l.contains("no pending request")),
            "should not warn when a resolver is present: {:?}",
            app.state.activity
        );
    }

    // #177: with no stashed resolver the decision is never silently
    // dropped — a visible diagnostic is pushed to the activity log.
    #[tokio::test]
    async fn resolve_approval_without_resolver_warns_visibly() {
        let mut app = test_app().await;
        assert!(app.pending_resolver.is_none());
        app.resolve_approval(ApprovalDecisionAction::Deny {
            reason: "no".to_string(),
        });
        assert!(
            app.state
                .activity
                .iter()
                .any(|l| l.contains("no pending request to resolve")),
            "missing-resolver decision must surface a diagnostic: {:?}",
            app.state.activity
        );
    }
}
