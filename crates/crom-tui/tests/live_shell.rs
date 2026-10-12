//! End-to-end integration tests for the M13 Phase A live TUI shell.
//!
//! These tests deliberately avoid touching a real terminal: they construct
//! an `AgentRuntime` with the SDK's fake provider, drive the reducer
//! through `drive_single_prompt`, and assert against the resulting state.
//! Snapshot rendering is exercised via ratatui's `TestBackend`.

use std::sync::Arc;

use crom_core::{ComposerConfig, StatusLineContext};
use crom_sdk::fake_provider::EchoFakeProvider;
use crom_sdk::{AgentRuntime, ProviderConfig};
use crom_tui::{drive_single_prompt, fullscreen, ComposerState, RunStatus, TuiAppState};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn fresh_state() -> TuiAppState {
    let mut ctx = StatusLineContext::default();
    ctx.set("project", "live-shell-test");
    ctx.set("model", "fake-model");
    ctx.set("run_status", "idle");
    TuiAppState::new(
        ComposerState::new(ComposerConfig::default()),
        "{project} · {model} · {run_status}".to_string(),
        ctx,
    )
}

async fn fake_runtime() -> AgentRuntime {
    AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await
        .expect("runtime")
}

fn buffer_to_string(term: &Terminal<TestBackend>) -> String {
    let buf = term.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            if let Some(cell) = buf.cell((x, y)) {
                out.push_str(cell.symbol());
            }
        }
        out.push('\n');
    }
    out
}

/// Driving a fake-provider runtime through the reducer produces a
/// transcript that contains both the user prompt and the assistant
/// echo reply, and the run returns to Idle.
#[tokio::test]
async fn fake_provider_round_trip() {
    let runtime = fake_runtime().await;
    let mut state = fresh_state();
    drive_single_prompt(&runtime, &mut state, "hello tui")
        .await
        .expect("drive ok");

    assert_eq!(state.run_status, RunStatus::Idle);
    assert!(state.last_error.is_none(), "{:?}", state.last_error);

    let kinds: Vec<&str> = state.transcript.iter().map(|c| c.kind).collect();
    assert!(kinds.contains(&"user"), "no user card: {kinds:?}");
    assert!(
        kinds.contains(&"run_started"),
        "no run_started card: {kinds:?}"
    );
    assert!(
        kinds.contains(&"run_completed"),
        "no run_completed card: {kinds:?}"
    );

    // Echo provider streams the prompt back as deltas, which the reducer
    // coalesces into an assistant card.
    let assistant_text: String = state
        .transcript
        .iter()
        .filter(|c| c.kind == "assistant" || c.kind == "assistant_delta")
        .flat_map(|c| c.body.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        assistant_text.contains("hello tui"),
        "assistant text missing prompt: {assistant_text:?}"
    );
}

/// Rendering the resulting state into a `TestBackend` includes the
/// project label, the prompt the user typed, and the agent reply.
#[tokio::test]
async fn frame_renders_user_and_agent_text() {
    let runtime = fake_runtime().await;
    let mut state = fresh_state();
    drive_single_prompt(&runtime, &mut state, "hello tui")
        .await
        .expect("drive ok");

    let backend = TestBackend::new(80, 16);
    let mut term = Terminal::new(backend).unwrap();
    let frame = crom_tui::build_ui_frame(&state, "[?] help");
    fullscreen::draw(&mut term, frame).unwrap();
    let dump = buffer_to_string(&term);
    assert!(dump.contains("live-shell-test"), "{dump}");
    assert!(dump.contains("You:"), "{dump}");
    assert!(dump.contains("hello tui"), "{dump}");
    // Agent echo text should appear somewhere in the transcript pane.
    assert!(dump.contains("echo: hello tui"), "{dump}");
}

/// Submitting an empty prompt does NOT start a run.
#[tokio::test]
async fn empty_prompt_is_ignored() {
    let runtime = fake_runtime().await;
    let mut state = fresh_state();
    // Press Enter without typing anything.
    let actions = state.apply(crom_tui::TuiInbound::Key(crom_tui::ComposerKey::Enter));
    assert!(actions.is_empty());
    assert_eq!(state.run_status, RunStatus::Idle);
    assert!(state.transcript.is_empty());
    // Sanity-check: a subsequent real prompt still works.
    drive_single_prompt(&runtime, &mut state, "ok")
        .await
        .expect("drive ok");
    assert_eq!(state.run_status, RunStatus::Idle);
}
