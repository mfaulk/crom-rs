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
    // M13 Phase E: when the Plan side-panel is selected and a plan
    // exists, render the M9 plan dashboard.
    // M13 Phase F: the five inspector modes (Tools, Mcp, Subagents,
    // Skills, Plugins) render real rows from `state.inspectors`
    // instead of the Phase B placeholder.
    let mode = state.layout.side_panel.mode;
    let body = match mode {
        SidePanelMode::Plan => state
            .current_plan
            .as_ref()
            .map(crate::plan_view::format_plan_panel)
            .unwrap_or_else(|| vec!["No active plan.".to_string()]),
        SidePanelMode::Tools
        | SidePanelMode::Mcp
        | SidePanelMode::Subagents
        | SidePanelMode::Skills
        | SidePanelMode::Plugins
        | SidePanelMode::Sessions
        | SidePanelMode::Auth
        | SidePanelMode::Model
        | SidePanelMode::Provider
        | SidePanelMode::Diagnostics
        | SidePanelMode::Activity => inspector_body(state, mode),
        other => side_panel_body(other),
    };
    PaneView {
        title: state.layout.side_panel.mode.label().to_string(),
        body,
        focused: matches!(state.layout.focus, FocusPane::SidePanel),
    }
}

/// Phase F: render the rows for one inspector side panel mode. The
/// active selection (if any) is marked with `›`; the empty state
/// reads "(none configured)" so a user who turns on `/tools` on a
/// fresh project sees something more useful than placeholder text.
fn inspector_body(state: &TuiAppState, mode: SidePanelMode) -> Vec<String> {
    let global_selected = state.inspector_selected_index(mode);
    // `window_start` tracks the offset applied by per-mode windowing
    // (currently only Activity) so the selection glyph hits the right
    // *rendered* row after slicing. Other modes don't window, so the
    // offset is 0.
    let mut window_start: usize = 0;
    let rows: Vec<String> = match mode {
        SidePanelMode::Tools => state
            .inspectors
            .tools
            .iter()
            .map(|t| {
                if t.permission.is_empty() {
                    format!("{} — {}", t.name, truncate(&t.description, 64))
                } else {
                    format!(
                        "{} [{}] — {}",
                        t.name,
                        t.permission,
                        truncate(&t.description, 56)
                    )
                }
            })
            .collect(),
        SidePanelMode::Mcp => state
            .inspectors
            .mcp
            .iter()
            .map(|m| {
                if m.config_entries == 0 {
                    format!("{} [{}] {}", m.name, m.transport, truncate(&m.endpoint, 48))
                } else {
                    format!(
                        "{} [{}] {} ({} entries)",
                        m.name,
                        m.transport,
                        truncate(&m.endpoint, 40),
                        m.config_entries
                    )
                }
            })
            .collect(),
        SidePanelMode::Subagents => state
            .inspectors
            .subagents
            .iter()
            .map(|s| {
                let model = s.model.as_deref().unwrap_or("(inherit)");
                let tools = s
                    .tool_count
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "(inherit)".to_string());
                format!(
                    "{} [{} | tools: {}] — {}",
                    s.name,
                    model,
                    tools,
                    truncate(&s.description, 40)
                )
            })
            .collect(),
        SidePanelMode::Skills => state
            .inspectors
            .skills
            .iter()
            .map(|sk| {
                format!(
                    "{} [{}] — {} ({} resources)",
                    sk.name,
                    sk.source,
                    truncate(&sk.description, 40),
                    sk.resource_count
                )
            })
            .collect(),
        SidePanelMode::Plugins => state
            .inspectors
            .plugins
            .iter()
            .map(|p| {
                let ver = p.version.as_deref().unwrap_or("?");
                let enabled = if p.enabled { "on" } else { "off" };
                format!(
                    "{} v{} [{} | {} | {}] — {} components",
                    p.name, ver, p.scope, enabled, p.status, p.component_total
                )
            })
            .collect(),
        // M13 Phase G: sessions / accounts / models / providers /
        // diagnostics each get one row per record.
        SidePanelMode::Sessions => state
            .inspectors
            .sessions
            .iter()
            .map(|s| {
                let lock = if s.has_lock { " 🔒" } else { "" };
                let ts = s.last_modified.as_deref().unwrap_or("?");
                format!(
                    "{} [{}]{} — last_modified {}",
                    truncate(&s.id, 28),
                    s.state,
                    lock,
                    ts
                )
            })
            .collect(),
        SidePanelMode::Auth => state
            .inspectors
            .accounts
            .iter()
            .map(|a| {
                let display = a.display_name.as_deref().unwrap_or("");
                let active = if a.active { "active" } else { "stored" };
                let exp = a
                    .expires_at
                    .as_deref()
                    .map(|s| format!(" exp:{s}"))
                    .unwrap_or_default();
                format!(
                    "{} [{} | {}{}] {}",
                    a.provider,
                    a.login_method,
                    active,
                    exp,
                    truncate(display, 32)
                )
            })
            .collect(),
        SidePanelMode::Model => state
            .inspectors
            .models
            .iter()
            .map(|m| {
                let ctx = m
                    .context_window
                    .map(|n| format!(" ctx:{n}"))
                    .unwrap_or_default();
                let flags = match (m.supports_tools, m.supports_reasoning) {
                    (true, true) => " [tools, reasoning]",
                    (true, false) => " [tools]",
                    (false, true) => " [reasoning]",
                    (false, false) => "",
                };
                format!(
                    "{} / {} [{}]{}{} — {}",
                    m.provider,
                    m.model_id,
                    m.availability,
                    ctx,
                    flags,
                    truncate(&m.display_name, 40)
                )
            })
            .collect(),
        SidePanelMode::Provider => state
            .inspectors
            .providers
            .iter()
            .map(|p| {
                let creds = if p.has_credentials {
                    "creds:yes"
                } else {
                    "creds:no"
                };
                let mark = if p.current { "* current" } else { "" };
                format!("{} [{}]{}", p.provider, creds, mark)
            })
            .collect(),
        SidePanelMode::Diagnostics => state
            .diagnostics
            .iter()
            .map(|d| {
                let hint = d
                    .suggestion
                    .as_deref()
                    .map(|s| format!(" — hint: {}", truncate(s, 32)))
                    .unwrap_or_default();
                format!(
                    "[{}] {} — {}{}",
                    d.severity,
                    d.code,
                    truncate(&d.message, 56),
                    hint
                )
            })
            .collect(),
        // M13 Phase H: the activity log is a flat ring buffer of
        // status strings already used in the bottom diagnostics
        // strip. The Activity panel makes it browsable; we window
        // the rendered slice around the current selection so the
        // selection glyph stays visible (the renderer itself is a
        // plain List with no offset).
        SidePanelMode::Activity => {
            const ACTIVITY_WINDOW: usize = 12;
            let len = state.activity.len();
            let sel = global_selected.unwrap_or(0);
            let start = sel.saturating_sub(ACTIVITY_WINDOW / 2);
            let end = (start + ACTIVITY_WINDOW).min(len);
            window_start = start;
            state.activity[start..end]
                .iter()
                .map(|line| truncate(line, 96))
                .collect()
        }
        _ => Vec::new(),
    };
    if rows.is_empty() {
        return vec![
            format!("{} inspector", mode.label()),
            "(none configured)".to_string(),
        ];
    }
    // Translate the global selection index into a per-rendered-row
    // index by subtracting any window offset applied by the per-mode
    // arm above. For non-windowed modes `window_start` stays 0, so
    // this is identity.
    let selected_in_rows = global_selected.and_then(|g| g.checked_sub(window_start));
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        if Some(i) == selected_in_rows {
            out.push(format!("› {row}"));
        } else {
            out.push(format!("  {row}"));
        }
    }
    out
}

/// Trim a string for inline rendering inside a one-line inspector
/// row. Adds an ellipsis when truncated. Conservative budgets per
/// field are chosen so a typical 100-column side panel still fits.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn detail_panel_view(state: &TuiAppState) -> PaneView {
    // M13 Phase E: real content for Approval / Diff modes when the
    // reducer has populated `pending_approval` / `last_patch`.
    let body = match state.layout.detail_panel.mode {
        DetailPanelMode::Approval => state
            .pending_approval
            .as_ref()
            .map(|a| {
                vec![
                    format!("tool: {}", a.tool_name),
                    String::new(),
                    a.summary.clone(),
                    String::new(),
                    "  [y] yes  [s] yes-for-session  [n] no  [r] revise".to_string(),
                ]
            })
            .unwrap_or_else(|| detail_panel_body(state.layout.detail_panel.mode)),
        DetailPanelMode::Diff => state
            .last_patch
            .as_ref()
            .map(diff_body_lines)
            .unwrap_or_else(|| detail_panel_body(state.layout.detail_panel.mode)),
        other => detail_panel_body(other),
    };
    PaneView {
        title: state.layout.detail_panel.mode.label().to_string(),
        body,
        focused: matches!(state.layout.focus, FocusPane::DetailPanel),
    }
}

/// Render a Phase E diff body from a `PatchSet`. Uses the
/// SDK-provided unified diff verbatim and caps at 200 lines so a
/// massive patch doesn't dominate the screen.
fn diff_body_lines(patch: &crom_sdk::patch::PatchSet) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!(
        "files: +{} ~{} -{} (renamed {})",
        patch.summary.files_created,
        patch.summary.files_modified,
        patch.summary.files_deleted,
        patch.summary.files_renamed,
    ));
    lines.push(format!(
        "lines: +{} -{}",
        patch.summary.added_lines, patch.summary.removed_lines
    ));
    lines.push(String::new());
    // Single-pass truncation: keep iterating after `take(200)` to see
    // if there's at least one more line, then drop the iterator.
    // Avoids a second full scan via `.count()` on huge patches.
    let mut iter = patch.unified_diff.lines();
    for line in iter.by_ref().take(200) {
        lines.push(line.to_string());
    }
    if iter.next().is_some() {
        lines.push("  ... (diff truncated at 200 lines)".to_string());
    }
    lines
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
        TuiModal::Help => help_modal_body(),
        TuiModal::CommandPalette => return palette_modal_view(state),
        TuiModal::PromptHistory => vec![
            "Prompt history".to_string(),
            "(content arrives in Phase D)".to_string(),
        ],
        TuiModal::Custom { body, .. } => body.clone(),
    };
    ModalView { title, body }
}

/// Phase H: build the expanded help-modal body with sectioned
/// shortcuts. Kept as a single flat Vec<String> so the existing
/// modal renderer doesn't need to learn section semantics.
fn help_modal_body() -> Vec<String> {
    // Trimmed to ~16 lines so the body fits within the
    // `centered_rect(70, 60, area)` modal viewport on a 24-row
    // terminal (60% ≈ 14 inner rows after borders + title).
    // `/palette` is the discoverable surface for the full command
    // catalog, so this overlay is a quick-reference card.
    vec![
        "Navigation:   Tab/Shift+Tab cycle panes · Esc close modal".to_string(),
        "              Ctrl+C cancel run / exit · Ctrl+D exit if composer empty".to_string(),
        String::new(),
        "Composer:     Enter submit · Tab accept popup · / @ trigger completions".to_string(),
        "              // sends a literal leading slash".to_string(),
        String::new(),
        "Transcript:   j/k/↓/↑ move · g/G first/last · e/Enter expand · n/N matches".to_string(),
        "Inspectors:   j/k/↓/↑ move · g/G first/last".to_string(),
        String::new(),
        "Approval:     y allow · s allow-session · n deny · r request-revision · Esc dismiss"
            .to_string(),
        String::new(),
        "Slash:        /palette opens the full picker. Common commands:".to_string(),
        "  /plan /changes /files /tools /sessions /diagnostics /activity".to_string(),
        "  /agents /mcp /plugins /skills /usage /auth /model /provider".to_string(),
        "  /help /search /history /toggle-side /toggle-detail".to_string(),
    ]
}

/// Phase H: render the command palette modal. The body shows the
/// in-progress input, the filtered candidate list (capped so a
/// short terminal doesn't overflow), a `›` selection marker, and a
/// short footer key legend.
fn palette_modal_view(state: &TuiAppState) -> ModalView {
    let input = state
        .palette
        .as_ref()
        .map(|p| p.input.as_str())
        .unwrap_or("");
    let selected = state.palette.as_ref().map(|p| p.selected).unwrap_or(0);
    let candidates = state.palette_candidates();
    let mut body = vec![format!("> {input}_")];
    body.push(String::new());
    if candidates.is_empty() {
        body.push("  (no matches)".to_string());
    } else {
        // Cap to a sensible window so a terminal as small as ~20 rows
        // still shows the input + footer. 15 leaves room for the
        // bordered modal chrome.
        const PALETTE_WINDOW: usize = 15;
        let view_start = selected.saturating_sub(PALETTE_WINDOW / 2);
        for (i, cmd) in candidates
            .iter()
            .enumerate()
            .skip(view_start)
            .take(PALETTE_WINDOW)
        {
            if i == selected {
                body.push(format!("› /{cmd}"));
            } else {
                body.push(format!("  /{cmd}"));
            }
        }
        if candidates.len() > view_start + PALETTE_WINDOW {
            body.push(format!(
                "  … ({} more)",
                candidates.len() - view_start - PALETTE_WINDOW
            ));
        }
    }
    body.push(String::new());
    body.push("  [Enter] run  [↑/↓] navigate  [Esc] cancel".to_string());
    ModalView {
        title: format!("Command palette · {} match(es)", candidates.len()),
        body,
    }
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
        // Activity is handled by inspector_body; this placeholder
        // path is never reached for it. Keep the arm to satisfy
        // exhaustiveness so future variants surface as compile
        // errors.
        SidePanelMode::Activity => "Phase H",
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
    use crom_core::{ComposerConfig, StatusLineContext};

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
    // Phase F: inspector panels render real rows from the snapshot
    // and mark the selected one with `›`.
    fn inspector_body_renders_real_rows_with_selection_glyph() {
        use crate::inspectors::{InspectorTool, Inspectors};
        let mut s = fresh_state().with_inspectors(Inspectors {
            tools: vec![
                InspectorTool {
                    name: "Bash".into(),
                    description: "shell".into(),
                    permission: "execute_bash".into(),
                },
                InspectorTool {
                    name: "Read".into(),
                    description: "read a file".into(),
                    permission: "read_only".into(),
                },
            ],
            ..Inspectors::default()
        });
        s.set_side_panel(SidePanelMode::Tools, true);
        s.inspector_selection.tools = Some(1);
        let body = inspector_body(&s, SidePanelMode::Tools);
        assert_eq!(body.len(), 2);
        assert!(body[0].starts_with("  Bash"), "first row not selected");
        assert!(
            body[1].starts_with("› Read"),
            "second row should carry the selection glyph"
        );
        // Permission tag is interpolated into the row text.
        assert!(body[1].contains("[read_only]"));
    }

    #[test]
    // Phase F: when an inspector mode has no entries, the panel
    // renders an informative empty-state body (not a Phase B
    // placeholder).
    fn inspector_body_empty_state_is_informative() {
        let s = fresh_state(); // empty Inspectors
        let body = inspector_body(&s, SidePanelMode::Plugins);
        assert!(body.iter().any(|l| l.contains("Plugins inspector")));
        assert!(body.iter().any(|l| l.contains("(none configured)")));
    }

    #[test]
    // Phase F: each of the five inspector modes uses the inspector
    // renderer (not the Phase B placeholder) so populated snapshots
    // produce the real row format.
    fn all_inspector_modes_render_real_rows_when_populated() {
        use crate::inspectors::{
            InspectorMcp, InspectorPlugin, InspectorSkill, InspectorSubagent, InspectorTool,
            Inspectors,
        };
        let s = fresh_state().with_inspectors(Inspectors {
            tools: vec![InspectorTool {
                name: "T".into(),
                ..Default::default()
            }],
            mcp: vec![InspectorMcp {
                name: "M".into(),
                transport: "stdio".into(),
                endpoint: "echo".into(),
                config_entries: 0,
            }],
            subagents: vec![InspectorSubagent {
                name: "A".into(),
                description: "desc".into(),
                ..Default::default()
            }],
            skills: vec![InspectorSkill {
                name: "S".into(),
                description: "desc".into(),
                source: "project".into(),
                resource_count: 2,
            }],
            plugins: vec![InspectorPlugin {
                name: "P".into(),
                enabled: true,
                scope: "user".into(),
                status: "loaded".into(),
                component_total: 4,
                ..Default::default()
            }],
            ..Inspectors::default()
        });
        for mode in [
            SidePanelMode::Tools,
            SidePanelMode::Mcp,
            SidePanelMode::Subagents,
            SidePanelMode::Skills,
            SidePanelMode::Plugins,
        ] {
            let body = inspector_body(&s, mode);
            assert_eq!(body.len(), 1, "{mode:?} should render 1 row");
            // Empty-state placeholder would say "(none configured)".
            assert!(
                !body.iter().any(|l| l.contains("(none configured)")),
                "{mode:?} should not show empty state"
            );
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
