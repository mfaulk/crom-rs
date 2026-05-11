//! Terminal UI (M9) for code-agent.
//!
//! This crate exposes two renderers — `Plain` and `Fullscreen` — plus the
//! deterministic rendering primitives the CLI uses to produce status-line,
//! transcript, approval, and diff output. Heavy interactive terminal I/O is
//! kept out of unit tests; ratatui's `TestBackend` is used for snapshot
//! tests so rendering can be exercised without a real TTY.

pub mod approval;
pub mod composer;
pub mod diff_view;
pub mod fullscreen;
pub mod plain;
pub mod plan_view;
pub mod renderer;
pub mod statusline;
pub mod transcript;

pub use approval::{format_approval_card, ApprovalContext, ApprovalKind};
pub use composer::{
    autocomplete_files, autocomplete_slash, ComposerState, KeyAction, KeyEvent as ComposerKey,
    PromptHistory,
};
pub use diff_view::{render_change_summary, render_diff_card, ChangeSummary, DiffEntry};
pub use plain::PlainRenderer;
pub use plan_view::{
    format_deviation_modal, format_plan_approval_modal, format_plan_diff, format_plan_panel,
};
pub use renderer::{detect_capabilities_from_env, fallback_renderer_for, RenderError};
pub use statusline::format_status_line;
pub use transcript::{render_event_card, EventCard};
