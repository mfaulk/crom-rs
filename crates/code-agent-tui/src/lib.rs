//! Terminal UI for code-agent.
//!
//! M9 shipped deterministic rendering primitives — status line, transcript
//! cards, approvals, diffs, plan panels — and a `Fullscreen` / `Plain`
//! renderer split. M13 adds the live event-loop application shell
//! ([`app::TuiApp`]) that drives a `code-agent-sdk` runtime: keyboard,
//! resize, and `SdkEvent` streams are fed into a pure reducer
//! ([`state::TuiAppState`]) whose actions ([`state::TuiAction`]) are
//! executed by the host loop.
//!
//! Heavy interactive terminal I/O is kept out of unit tests; ratatui's
//! `TestBackend` is used for snapshot tests and the reducer is exercised
//! directly so behavior can be verified without a real TTY.

pub mod app;
pub mod approval;
pub mod composer;
pub mod diff_view;
pub mod event_adapter;
pub mod fullscreen;
pub mod plain;
pub mod plan_view;
pub mod renderer;
pub mod state;
pub mod statusline;
pub mod transcript;

pub use app::{drive_single_prompt, TuiApp, TuiAppError};
pub use approval::{format_approval_card, ApprovalContext, ApprovalKind};
pub use composer::{
    autocomplete_files, autocomplete_slash, ComposerState, KeyAction, KeyEvent as ComposerKey,
    PromptHistory,
};
pub use diff_view::{render_change_summary, render_diff_card, ChangeSummary, DiffEntry};
pub use event_adapter::{map_sdk_event, SdkEventOutcome};
pub use plain::PlainRenderer;
pub use plan_view::{
    format_deviation_modal, format_plan_approval_modal, format_plan_diff, format_plan_panel,
};
pub use renderer::{detect_capabilities_from_env, fallback_renderer_for, RenderError};
pub use state::{RunStatus, TuiAction, TuiAppState, TuiInbound};
pub use statusline::format_status_line;
pub use transcript::{render_event_card, EventCard};
