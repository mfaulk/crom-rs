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
use tokio::sync::mpsc;

use code_agent_sdk::{AgentRequest, AgentRun, AgentRuntime, AgentSdkError, SdkEvent};

use crate::composer::KeyEvent;
use crate::fullscreen::draw;
use crate::state::{TuiAction, TuiAppState, TuiInbound};

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
}

impl TuiApp {
    /// Construct a new app. The caller supplies a built [`AgentRuntime`]
    /// (whose provider may be real or fake) and the initial reducer state.
    pub fn new(runtime: Arc<AgentRuntime>, state: TuiAppState) -> Self {
        Self {
            runtime,
            state,
            footer: "[?] help  [Ctrl+C] interrupt  [Ctrl+D] exit".to_string(),
        }
    }

    /// Replace the help footer text.
    pub fn with_footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = footer.into();
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
        let input_task = spawn_input_task(input_tx);

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
                            self.state.apply(TuiInbound::sdk(ev));
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
        input_task.abort();
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
            }
        }
    }

    fn draw(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), TuiAppError> {
        let frame = crate::view::build_ui_frame(&self.state, &self.footer);
        draw(terminal, frame)?;
        Ok(())
    }
}

/// Spawn a blocking task that reads crossterm events and forwards them
/// (decoded into our abstract composer `KeyEvent`) to the main loop.
fn spawn_input_task(tx: mpsc::Sender<TuiInbound>) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        loop {
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
