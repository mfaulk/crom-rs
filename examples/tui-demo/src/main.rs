//! TUI demo. Two subcommands:
//!
//! * `live` — boot the M13 TUI shell against the SDK's echo fake
//!   provider. Requires a TTY; quit with Ctrl+D when the composer
//!   is empty.
//! * `snapshot` — drive the reducer through a single canned prompt
//!   via `drive_single_prompt` and print the resulting frame to
//!   stdout using ratatui's `TestBackend`. Useful for smoke tests
//!   and demos in non-interactive environments.
//!
//! Usage:
//!     cargo run -p tui-demo -- live
//!     cargo run -p tui-demo -- snapshot
//!     cargo run -p tui-demo -- snapshot "hello"

use std::sync::Arc;

use code_agent_core::{ComposerConfig, StatusLineContext};
use code_agent_sdk::{
    fake_provider::EchoFakeProvider, AgentRuntime, ProviderConfig, WorkspaceSource,
};
use code_agent_tui::{
    build_ui_frame, drive_single_prompt, ComposerState, TuiApp, TuiAppState,
};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "snapshot".into());
    match cmd.as_str() {
        "live" => live().await,
        "snapshot" => {
            let prompt = std::env::args()
                .nth(2)
                .unwrap_or_else(|| "hello from tui-demo".to_string());
            snapshot(&prompt).await
        }
        other => {
            eprintln!("unknown subcommand: {other}\navailable: live, snapshot [prompt]");
            std::process::exit(2);
        }
    }
}

async fn fake_runtime() -> anyhow::Result<Arc<AgentRuntime>> {
    let cwd = std::env::current_dir()?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::filesystem(&cwd))
        .build()
        .await
        .map_err(|e| anyhow::anyhow!("build fake runtime: {}", e.message))?;
    Ok(Arc::new(runtime))
}

fn fresh_state() -> TuiAppState {
    let mut ctx = StatusLineContext::default();
    ctx.set("project", "tui-demo");
    TuiAppState::new(
        ComposerState::new(ComposerConfig::default()),
        "{project}".to_string(),
        ctx,
    )
}

/// Live: boot the M13 TUI shell against the fake provider.
async fn live() -> anyhow::Result<()> {
    let runtime = fake_runtime().await?;
    let state = fresh_state();
    let app = TuiApp::new(runtime, state);
    app.run()
        .await
        .map_err(|e| anyhow::anyhow!("tui exited: {e}"))
}

/// Snapshot: drive one prompt through the reducer and render to a
/// non-interactive ratatui `TestBackend`. Prints the resulting
/// frame to stdout — useful for CI smoke tests.
async fn snapshot(prompt: &str) -> anyhow::Result<()> {
    let runtime = fake_runtime().await?;
    let mut state = fresh_state();
    drive_single_prompt(&runtime, &mut state, prompt)
        .await
        .map_err(|e| anyhow::anyhow!("drive_single_prompt: {e}"))?;
    let frame = build_ui_frame(&state, "");
    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend)?;
    code_agent_tui::fullscreen::draw(&mut terminal, frame)?;
    let buf = terminal.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
    Ok(())
}
