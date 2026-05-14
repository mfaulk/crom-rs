//! Adapter from [`TuiAppState`] (logical state) to [`UiFrame`] (renderer
//! input). Phase B layer that lets the renderer stay agnostic about
//! plan dashboards, inspectors, etc. — later phases fill the placeholder
//! bodies returned here with real content.

use crate::approval::ApprovalContext;
use crate::fullscreen::{ModalView, PaneView, UiFrame};
use crate::layout::{DetailPanelMode, FocusPane, SidePanelMode, TuiModal};
use crate::state::TuiAppState;

/// Build a [`UiFrame`] from the current reducer state. The `footer`
/// string is owned by the caller (typically the [`crate::TuiApp`]) so
/// it can be customized without poking the reducer.
pub fn build_ui_frame<'a>(state: &'a TuiAppState, footer: &'a str) -> UiFrame<'a> {
    UiFrame {
        status_template: &state.status_template,
        status_ctx: state.status_ctx.clone(),
        diagnostics: state.activity.clone(),
        transcript: state.transcript.clone(),
        composer: state.composer.buffer.as_str(),
        footer,
        approval: None::<ApprovalContext<'a>>,
        layout_mode: state.layout.mode,
        side_panel: state
            .layout
            .side_panel
            .visible
            .then(|| side_panel_view(state)),
        detail_panel: state
            .layout
            .detail_panel
            .visible
            .then(|| detail_panel_view(state)),
        modal: state.layout.current_modal().map(modal_view),
        composer_focused: matches!(state.layout.focus, FocusPane::Composer),
        transcript_focused: matches!(state.layout.focus, FocusPane::Transcript),
    }
}

fn side_panel_view(state: &TuiAppState) -> PaneView {
    PaneView {
        title: state.layout.side_panel.mode.label().to_string(),
        body: side_panel_body(state.layout.side_panel.mode),
        focused: matches!(state.layout.focus, FocusPane::SidePanel),
    }
}

fn detail_panel_view(state: &TuiAppState) -> PaneView {
    PaneView {
        title: state.layout.detail_panel.mode.label().to_string(),
        body: detail_panel_body(state.layout.detail_panel.mode),
        focused: matches!(state.layout.focus, FocusPane::DetailPanel),
    }
}

fn modal_view(modal: &TuiModal) -> ModalView {
    let title = modal.title().to_string();
    let body = match modal {
        TuiModal::Help => vec![
            "Keyboard shortcuts".to_string(),
            "".to_string(),
            "  Tab / Shift+Tab    cycle pane focus".to_string(),
            "  Esc                close this modal".to_string(),
            "  Ctrl+C             cancel run / exit when idle".to_string(),
            "  Ctrl+D             exit when composer empty".to_string(),
            "".to_string(),
            "Slash commands".to_string(),
            "".to_string(),
            "  /plan /changes /files /tools /sessions".to_string(),
            "  /diagnostics /agents /mcp /plugins /skills".to_string(),
            "  /usage /auth /model /provider".to_string(),
            "  /help /keys /palette /search /history".to_string(),
            "  /toggle-side /toggle-detail".to_string(),
        ],
        TuiModal::CommandPalette => vec![
            "Command palette".to_string(),
            "(content arrives in Phase H)".to_string(),
        ],
        TuiModal::SearchTranscript => vec![
            "Transcript search".to_string(),
            "(content arrives in Phase C)".to_string(),
        ],
        TuiModal::PromptHistory => vec![
            "Prompt history".to_string(),
            "(content arrives in Phase D)".to_string(),
        ],
        TuiModal::Custom { body, .. } => body.clone(),
    };
    ModalView { title, body }
}

/// Phase B placeholder content for each [`SidePanelMode`]. Real content
/// arrives in the phase that owns each panel — keeping the body shape
/// stable here lets the renderer be reviewed independently.
fn side_panel_body(mode: SidePanelMode) -> Vec<String> {
    let coming_in = match mode {
        SidePanelMode::Plan => "Phase E",
        SidePanelMode::Changes | SidePanelMode::Files => "Phase F",
        SidePanelMode::Tools
        | SidePanelMode::Subagents
        | SidePanelMode::Mcp
        | SidePanelMode::Plugins
        | SidePanelMode::Skills => "Phase F",
        SidePanelMode::Sessions
        | SidePanelMode::Auth
        | SidePanelMode::Model
        | SidePanelMode::Provider => "Phase G",
        SidePanelMode::Diagnostics => "Phase G",
        SidePanelMode::Usage => "Phase H",
    };
    vec![
        format!("{} panel", mode.label()),
        format!("(content arrives in {coming_in})"),
    ]
}

fn detail_panel_body(mode: DetailPanelMode) -> Vec<String> {
    match mode {
        DetailPanelMode::None => vec!["(no selection)".to_string()],
        DetailPanelMode::EventDetails => vec!["Event details (Phase C)".to_string()],
        DetailPanelMode::Diff | DetailPanelMode::Approval => {
            vec![format!("{} (Phase E)", mode.label())]
        }
        DetailPanelMode::PlanDetails => vec!["Plan details (Phase E)".to_string()],
        DetailPanelMode::FilePreview => vec!["File preview (Phase F)".to_string()],
        DetailPanelMode::ToolDetails => vec!["Tool details (Phase F)".to_string()],
        DetailPanelMode::Diagnostics => vec!["Diagnostics (Phase G)".to_string()],
        DetailPanelMode::SessionDetails => vec!["Session details (Phase G)".to_string()],
        DetailPanelMode::RawJson => vec!["Raw JSON (Phase C)".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer::ComposerState;
    use code_agent_core::{ComposerConfig, StatusLineContext};

    fn fresh_state() -> TuiAppState {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "demo");
        TuiAppState::new(
            ComposerState::new(ComposerConfig::default()),
            "{project}".to_string(),
            ctx,
        )
    }

    #[test]
    // The default state produces a Plan side panel with focused=false
    // and no detail panel.
    fn default_state_yields_plan_side_panel_and_no_detail() {
        let s = fresh_state();
        let f = build_ui_frame(&s, "footer");
        let side = f.side_panel.expect("side panel");
        assert_eq!(side.title, "Plan");
        assert!(!side.focused);
        assert!(f.detail_panel.is_none());
        assert!(f.modal.is_none());
        assert!(f.composer_focused, "composer is default focus");
    }

    #[test]
    // Toggling side panel visibility off hides it in the frame.
    fn hidden_side_panel_omits_pane() {
        let mut s = fresh_state();
        s.layout.side_panel.visible = false;
        let f = build_ui_frame(&s, "footer");
        assert!(f.side_panel.is_none());
    }

    #[test]
    // Each modal variant maps to a non-empty body and stable title.
    fn modal_titles_are_stable() {
        for modal in [
            TuiModal::Help,
            TuiModal::CommandPalette,
            TuiModal::SearchTranscript,
            TuiModal::PromptHistory,
        ] {
            let v = modal_view(&modal);
            assert!(!v.title.is_empty());
            assert!(!v.body.is_empty());
        }
    }

    #[test]
    // Detail panel mode placeholders all return non-empty content.
    fn detail_placeholder_bodies_non_empty() {
        for mode in [
            DetailPanelMode::None,
            DetailPanelMode::EventDetails,
            DetailPanelMode::Diff,
            DetailPanelMode::Approval,
            DetailPanelMode::PlanDetails,
            DetailPanelMode::FilePreview,
            DetailPanelMode::ToolDetails,
            DetailPanelMode::Diagnostics,
            DetailPanelMode::SessionDetails,
            DetailPanelMode::RawJson,
        ] {
            let body = detail_panel_body(mode);
            assert!(!body.is_empty(), "{mode:?} body should be non-empty");
        }
    }
}
