//! Adapter from [`TuiAppState`] (logical state) to [`UiFrame`] (renderer
//! input). Phase B layer that lets the renderer stay agnostic about
//! plan dashboards, inspectors, etc. — later phases fill the placeholder
//! bodies returned here with real content.

use crate::approval::ApprovalContext;
use crate::completion::CompletionKind;
use crate::fullscreen::{CompletionView, ModalView, PaneView, UiFrame};
use crate::layout::{DetailPanelMode, FocusPane, SidePanelMode, TuiModal};
use crate::state::TuiAppState;

/// Build a [`UiFrame`] from the current reducer state. The `footer`
/// string is owned by the caller (typically the [`crate::TuiApp`]) so
/// it can be customized without poking the reducer.
///
/// Phase C extensions:
/// * The transcript is filtered through the active `search_query`.
/// * The selected card is marked with a leading `›` glyph; expanded
///   cards have their `expanded_body` appended inline.
/// * Search-modal input is reflected in the `SearchTranscript` modal
///   body so the user can see what they're typing.
pub fn build_ui_frame<'a>(state: &'a TuiAppState, footer: &'a str) -> UiFrame<'a> {
    let transcript = projected_transcript(state);
    UiFrame {
        status_template: &state.status_template,
        status_ctx: state.status_ctx.clone(),
        diagnostics: state.activity.clone(),
        transcript,
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
        modal: state
            .layout
            .current_modal()
            .map(|m| modal_view_for(state, m)),
        composer_focused: matches!(state.layout.focus, FocusPane::Composer),
        transcript_focused: matches!(state.layout.focus, FocusPane::Transcript),
        completion: state.composer_popup.as_ref().map(completion_view),
    }
}

/// Build a renderer-friendly [`CompletionView`] from the reducer's
/// active popup. The title names the completion kind so the renderer
/// can label the overlay without re-deriving the semantics.
fn completion_view(popup: &crate::completion::CompletionPopup) -> CompletionView {
    let title = match popup.kind {
        CompletionKind::Slash => "Slash commands",
        CompletionKind::ContextToken => "@ context tokens",
        CompletionKind::File => "@ files",
    };
    CompletionView {
        title: title.to_string(),
        entries: popup.candidates.iter().map(|c| c.display.clone()).collect(),
        selected: popup.selected,
    }
}

/// Return the cards the renderer should display. Honors the active
/// search filter, the per-card expansion bit (folds `expanded_body`
/// into the body so the renderer doesn't have to know about expansion
/// state), and the selection marker (a leading `›` on the headline).
fn projected_transcript(state: &TuiAppState) -> Vec<crate::transcript::EventCard> {
    let indices = state.visible_card_indices();
    let mut out = Vec::with_capacity(indices.len());
    for i in indices {
        let src = &state.transcript[i];
        let mut card = src.clone();
        if state.is_card_expanded(card.id) && !card.expanded_body.is_empty() {
            // Fold expanded body into the visible body inline.
            for line in &card.expanded_body {
                card.body.push(line.clone());
            }
        }
        if state.selected_card_id == Some(card.id) {
            // Mark selection inline so the snapshot renderer
            // (which doesn't have per-card styling) shows it.
            card.headline = format!("› {}", card.headline);
        } else {
            card.headline = format!("  {}", card.headline);
        }
        out.push(card);
    }
    out
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

fn modal_view_for(state: &TuiAppState, modal: &TuiModal) -> ModalView {
    let title = modal.title().to_string();
    let body = match modal {
        TuiModal::SearchTranscript => {
            let mut lines = vec![format!("Search: {}_", state.search_input)];
            lines.push(String::new());
            if let Some(q) = &state.search_query {
                lines.push(format!("Current filter: {q}"));
            } else {
                lines.push("(Enter commits, Esc cancels)".to_string());
            }
            let visible = state.visible_card_indices().len();
            let total = state.transcript.len();
            lines.push(format!("{visible} of {total} cards match"));
            return ModalView { title, body: lines };
        }
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
        let state = fresh_state();
        for modal in [
            TuiModal::Help,
            TuiModal::CommandPalette,
            TuiModal::SearchTranscript,
            TuiModal::PromptHistory,
        ] {
            let v = modal_view_for(&state, &modal);
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
