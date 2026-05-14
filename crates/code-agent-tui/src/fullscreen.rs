//! Fullscreen renderer built on ratatui. Designed so the same `render`
//! function drives both the live `crossterm` backend and the `TestBackend`
//! used by snapshot tests, which is how M9 keeps deterministic coverage of
//! TUI rendering without owning a terminal.
//!
//! M13 Phase B adds a multi-pane layout: a side panel on the right edge
//! of the transcript pane and a detail panel below it, selected by the
//! caller through optional `side_panel` / `detail_panel` fields on the
//! [`UiFrame`]. A topmost modal overlay can be supplied through `modal`.

use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;

use crate::approval::ApprovalContext;
use crate::layout::LayoutMode;
use crate::statusline::format_status_line;
use crate::transcript::EventCard;

/// One pane's title + body for the renderer. Renderer-agnostic so that
/// later phases can supply real content (plan dashboard, inspectors,
/// session list, etc.) without changing the layout code here.
#[derive(Debug, Clone, Default)]
pub struct PaneView {
    pub title: String,
    pub body: Vec<String>,
    /// Whether this pane currently holds keyboard focus. The renderer
    /// uses this to draw a bold/emphasized border.
    pub focused: bool,
}

/// Modal overlay payload. The renderer paints this on top of the rest
/// of the frame in the center of the terminal.
#[derive(Debug, Clone, Default)]
pub struct ModalView {
    pub title: String,
    pub body: Vec<String>,
}

/// Snapshot of everything the fullscreen renderer needs to paint a frame.
/// Built once per redraw by the caller.
#[derive(Debug, Clone, Default)]
pub struct UiFrame<'a> {
    pub status_template: &'a str,
    pub status_ctx: code_agent_core::StatusLineContext,
    pub diagnostics: Vec<String>,
    pub transcript: Vec<EventCard>,
    pub composer: &'a str,
    pub footer: &'a str,
    pub approval: Option<ApprovalContext<'a>>,
    /// M13 Phase B: which responsive layout class to render. `Full`
    /// lays out side + detail panels alongside the transcript;
    /// `Compact` collapses to the M9 single-column layout; `Tiny`
    /// further hides status + footer when terminal real estate is
    /// extremely tight.
    pub layout_mode: LayoutMode,
    /// M13 Phase B: side-panel content. `None` hides the side panel
    /// even when `layout_mode == Full`.
    pub side_panel: Option<PaneView>,
    /// M13 Phase B: detail-panel content. `None` hides the detail panel.
    pub detail_panel: Option<PaneView>,
    /// M13 Phase B: top-of-stack modal overlay. `None` means no modal.
    pub modal: Option<ModalView>,
    /// M13 Phase B: indicate the composer is the focused pane. The
    /// renderer uses this to thicken the composer border.
    pub composer_focused: bool,
    /// M13 Phase B: indicate the transcript is the focused pane.
    pub transcript_focused: bool,
}

/// Draw one frame onto the supplied backend. Pure rendering: no input,
/// no event reading. Tests pass a `TestBackend`; the live CLI passes a
/// `CrosstermBackend`.
///
/// The renderer respects [`UiFrame::layout_mode`]:
/// * `Full` lays out status / transcript+sidebar / diagnostics / composer
///   / footer, with an optional detail pane underneath the transcript+sidebar
///   strip when [`UiFrame::detail_panel`] is `Some`.
/// * `Compact` collapses to the M9 single-column layout — side and detail
///   panels are hidden even if supplied.
/// * `Tiny` drops the status line and footer too: just transcript and
///   composer. Guards against panic on a 20×4 terminal.
pub fn draw<B: Backend>(terminal: &mut Terminal<B>, frame: UiFrame<'_>) -> Result<(), B::Error> {
    terminal.draw(|f| {
        let area = f.area();
        let effective = effective_layout_mode(frame.layout_mode, area);

        // Vertical sectioning depends on responsive mode.
        let sections: Vec<Rect> = match effective {
            LayoutMode::Full | LayoutMode::Compact => {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Length(1),                   // status line
                        Constraint::Min(3),                      // body
                        Constraint::Length(diag_height(&frame)), // diagnostics
                        Constraint::Length(3),                   // composer
                        Constraint::Length(1),                   // footer
                    ])
                    .split(area)
                    .to_vec()
            }
            LayoutMode::Tiny => Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(2),    // body
                    Constraint::Length(3), // composer
                ])
                .split(area)
                .to_vec(),
        };

        if matches!(effective, LayoutMode::Full | LayoutMode::Compact) {
            // Status line.
            let status_line = format_status_line(
                frame.status_template,
                &frame.status_ctx,
                area.width as usize,
            );
            f.render_widget(
                Paragraph::new(status_line).style(Style::default().add_modifier(Modifier::BOLD)),
                sections[0],
            );
        }

        // Body area: transcript +/- sidebar +/- detail.
        let body_area = match effective {
            LayoutMode::Full | LayoutMode::Compact => sections[1],
            LayoutMode::Tiny => sections[0],
        };

        // Approval modal (M9) takes precedence over the multi-pane
        // layout — keeps existing M9 behavior for approval flows so
        // Phase B doesn't regress them.
        if let Some(ctx) = &frame.approval {
            let lines = crate::approval::format_approval_card(ctx);
            let items: Vec<ListItem> = lines.iter().map(|l| ListItem::new(l.clone())).collect();
            f.render_widget(
                List::new(items).block(Block::default().borders(Borders::ALL).title("Approval")),
                body_area,
            );
        } else if matches!(effective, LayoutMode::Full)
            && (frame.side_panel.is_some() || frame.detail_panel.is_some())
        {
            // Full multi-pane layout: vertical split for transcript +
            // detail, horizontal split inside the top half for
            // transcript + sidebar.
            let detail_present = frame.detail_panel.is_some();
            let inner = if detail_present {
                Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(body_area)
                    .to_vec()
            } else {
                vec![body_area]
            };
            let top = inner[0];
            let side_present = frame.side_panel.is_some();
            let cols = if side_present {
                Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
                    .split(top)
                    .to_vec()
            } else {
                vec![top]
            };
            render_transcript(f, &frame, cols[0]);
            if let Some(pane) = &frame.side_panel {
                render_pane(f, pane, cols.get(1).copied().unwrap_or(top));
            }
            if let (true, Some(pane)) = (detail_present, frame.detail_panel.as_ref()) {
                render_pane(f, pane, inner[1]);
            }
        } else {
            // Compact / Tiny / no panels: single-column transcript.
            render_transcript(f, &frame, body_area);
        }

        if matches!(effective, LayoutMode::Full | LayoutMode::Compact) && diag_height(&frame) > 0 {
            let diag: Vec<ListItem> = frame
                .diagnostics
                .iter()
                .map(|l| ListItem::new(l.clone()))
                .collect();
            f.render_widget(
                List::new(diag).block(Block::default().borders(Borders::ALL).title("Activity")),
                sections[2],
            );
        }

        // Composer.
        let composer_area = match effective {
            LayoutMode::Full | LayoutMode::Compact => sections[3],
            LayoutMode::Tiny => sections[1],
        };
        let composer_title = if frame.composer_focused {
            "Composer ▎"
        } else {
            "Composer"
        };
        f.render_widget(
            Paragraph::new(format!("> {}", frame.composer))
                .block(Block::default().borders(Borders::ALL).title(composer_title)),
            composer_area,
        );

        // Footer.
        if matches!(effective, LayoutMode::Full | LayoutMode::Compact) {
            f.render_widget(Paragraph::new(frame.footer), sections[4]);
        }

        // Modal overlay paints last so it sits on top of everything.
        if let Some(modal) = &frame.modal {
            render_modal(f, modal, area);
        }
    })?;
    Ok(())
}

fn render_transcript(f: &mut ratatui::Frame<'_>, frame: &UiFrame<'_>, rect: Rect) {
    let mut all: Vec<Line> = Vec::new();
    for card in &frame.transcript {
        for l in card.lines() {
            all.push(Line::from(Span::raw(l)));
        }
    }
    let title = if frame.transcript_focused {
        "Transcript ▎"
    } else {
        "Transcript"
    };
    f.render_widget(
        Paragraph::new(all)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(title)),
        rect,
    );
}

fn render_pane(f: &mut ratatui::Frame<'_>, pane: &PaneView, rect: Rect) {
    let title = if pane.focused {
        format!("{} ▎", pane.title)
    } else {
        pane.title.clone()
    };
    let body_lines: Vec<ListItem> = pane.body.iter().map(|l| ListItem::new(l.clone())).collect();
    f.render_widget(
        List::new(body_lines).block(Block::default().borders(Borders::ALL).title(title)),
        rect,
    );
}

fn render_modal(f: &mut ratatui::Frame<'_>, modal: &ModalView, area: Rect) {
    let modal_area = centered_rect(70, 60, area);
    f.render_widget(Clear, modal_area);
    let items: Vec<ListItem> = modal
        .body
        .iter()
        .map(|l| ListItem::new(l.clone()))
        .collect();
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(modal.title.clone())
                .style(Style::default().add_modifier(Modifier::BOLD)),
        ),
        modal_area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// Degrade the caller-requested layout mode when the actual terminal is
/// too small to honor it. Treats the renderer's `area` as authoritative
/// because the caller's `LayoutState` may have been computed off an
/// older resize event. Never *expands* beyond the caller's selection.
fn effective_layout_mode(requested: LayoutMode, area: Rect) -> LayoutMode {
    let measured = LayoutMode::from_size(area.width, area.height);
    let order = |m: LayoutMode| -> u8 {
        match m {
            LayoutMode::Tiny => 0,
            LayoutMode::Compact => 1,
            LayoutMode::Full => 2,
        }
    };
    if order(measured) < order(requested) {
        measured
    } else {
        requested
    }
}

fn diag_height(frame: &UiFrame<'_>) -> u16 {
    if frame.diagnostics.is_empty() {
        0
    } else {
        (frame.diagnostics.len() as u16 + 2).min(6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::EventCard;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn buffer_to_string(t: &Terminal<TestBackend>) -> String {
        let backend = t.backend();
        let buf = backend.buffer();
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

    #[test]
    // Fullscreen renderer paints status, transcript, composer, and footer
    // onto a deterministic test backend.
    fn renders_with_test_backend() {
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let mut ctx = code_agent_core::StatusLineContext::default();
        ctx.set("project", "demo");
        ctx.set("git_branch", "main");
        ctx.set("git_dirty", "");
        ctx.set("model", "Sonnet");
        ctx.set("context_percent", "5");
        ctx.set("estimated_cost", "$0.01");
        let frame = UiFrame {
            status_template: code_agent_core::DEFAULT_STATUSLINE_TEMPLATE,
            status_ctx: ctx,
            diagnostics: vec!["MCP: connected".into()],
            transcript: vec![EventCard {
                kind: "assistant",
                headline: "Agent:".into(),
                body: vec!["hello".into()],
                ..Default::default()
            }],
            composer: "fix the failing tests",
            footer: "[?] help [Ctrl+C] interrupt",
            approval: None,
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("demo"));
        assert!(s.contains("Sonnet"));
        assert!(s.contains("Agent:"));
        assert!(s.contains("fix the failing tests"));
    }

    #[test]
    // Approval modal replaces the transcript pane when present.
    fn renders_approval_modal() {
        let backend = TestBackend::new(120, 14);
        let mut term = Terminal::new(backend).unwrap();
        let input = serde_json::json!({"command": "rm -rf /"});
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "demo");
                c
            },
            diagnostics: Vec::new(),
            transcript: Vec::new(),
            composer: "",
            footer: "",
            approval: Some(ApprovalContext {
                tool_name: "Bash",
                input: &input,
                source: None,
                risk: Some("high"),
                checkpoint_id: None,
                checkpoint_note: None,
                diff_preview: None,
            }),
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("Approval"));
        assert!(s.contains("rm -rf /"));
        assert!(s.contains("[r] revise"));
    }

    #[test]
    // Narrow terminal does not panic.
    fn narrow_terminal_does_not_panic() {
        let backend = TestBackend::new(20, 6);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "tiny");
                c
            },
            diagnostics: Vec::new(),
            transcript: Vec::new(),
            composer: "",
            footer: "",
            approval: None,
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
    }

    #[test]
    // Wide unicode in the transcript does not break layout.
    fn wide_unicode_in_transcript() {
        let backend = TestBackend::new(40, 10);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "ユニコード");
                c
            },
            diagnostics: Vec::new(),
            transcript: vec![EventCard {
                kind: "assistant",
                headline: "Agent:".into(),
                body: vec!["你好世界 🌏".into()],
                ..Default::default()
            }],
            composer: "",
            footer: "",
            approval: None,
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("Agent:"));
    }

    #[test]
    // Full-layout multi-pane render places the side panel alongside
    // the transcript and the detail panel below them.
    fn full_layout_multi_pane_render() {
        let backend = TestBackend::new(120, 24);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "multi-pane-demo");
                c
            },
            diagnostics: Vec::new(),
            transcript: vec![EventCard {
                kind: "assistant",
                headline: "Agent:".into(),
                body: vec!["transcript line".into()],
                ..Default::default()
            }],
            composer: "type here",
            footer: "[?] help",
            approval: None,
            layout_mode: LayoutMode::Full,
            side_panel: Some(PaneView {
                title: "Plan".to_string(),
                body: vec!["1 ✓ run tests".into(), "2 → patch".into()],
                focused: true,
            }),
            detail_panel: Some(PaneView {
                title: "Diff".to_string(),
                body: vec!["+ added line".into(), "- removed line".into()],
                focused: false,
            }),
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("multi-pane-demo"), "{s}");
        assert!(s.contains("Plan ▎"), "focused side panel title missing");
        assert!(s.contains("Diff"), "detail panel title missing");
        assert!(s.contains("transcript line"), "transcript missing");
        assert!(s.contains("type here"), "composer missing");
    }

    #[test]
    // A Modal overlay paints on top of the rest of the frame with a
    // bold-styled title.
    fn modal_overlay_renders_on_top() {
        let backend = TestBackend::new(80, 16);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "demo");
                c
            },
            diagnostics: Vec::new(),
            transcript: Vec::new(),
            composer: "",
            footer: "",
            approval: None,
            modal: Some(ModalView {
                title: "Help".to_string(),
                body: vec!["Tab cycles panes".into(), "Esc closes modal".into()],
            }),
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("Help"), "modal title missing");
        assert!(s.contains("Tab cycles panes"), "modal body missing");
        assert!(s.contains("Esc closes modal"), "modal body missing");
    }

    #[test]
    // Tiny layout drops the status line and footer, keeping just
    // transcript and composer. Must not panic on a 20×6 terminal.
    fn tiny_layout_drops_chrome() {
        let backend = TestBackend::new(20, 6);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "tiny");
                c
            },
            diagnostics: Vec::new(),
            transcript: vec![EventCard {
                kind: "assistant",
                headline: "A:".into(),
                body: vec!["hi".into()],
                ..Default::default()
            }],
            composer: "x",
            footer: "ignored",
            approval: None,
            layout_mode: LayoutMode::Tiny,
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        // Footer text is dropped under Tiny.
        assert!(!s.contains("ignored"), "footer should be dropped: {s}");
        // Composer still appears.
        assert!(s.contains("> x"), "composer prompt missing: {s}");
    }

    #[test]
    // When the caller requests Full but the actual terminal is too
    // small, the renderer degrades to whatever the terminal can fit
    // and does not panic.
    fn full_request_on_tiny_terminal_degrades() {
        let backend = TestBackend::new(20, 6);
        let mut term = Terminal::new(backend).unwrap();
        let frame = UiFrame {
            status_template: "{project}",
            status_ctx: {
                let mut c = code_agent_core::StatusLineContext::default();
                c.set("project", "x");
                c
            },
            diagnostics: Vec::new(),
            transcript: Vec::new(),
            composer: "",
            footer: "",
            approval: None,
            layout_mode: LayoutMode::Full,
            side_panel: Some(PaneView {
                title: "Plan".to_string(),
                body: Vec::new(),
                focused: false,
            }),
            ..UiFrame::default()
        };
        draw(&mut term, frame).unwrap();
        // Nothing to assert beyond non-panic + visible chrome. The Plan
        // title should NOT appear because Full degrades to Compact /
        // Tiny on a 20x6 backend.
        let s = buffer_to_string(&term);
        assert!(
            !s.contains("Plan ▎"),
            "Plan should not be focused on tiny: {s}"
        );
    }
}
