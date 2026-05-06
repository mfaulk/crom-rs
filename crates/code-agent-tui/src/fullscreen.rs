//! Fullscreen renderer built on ratatui. Designed so the same `render`
//! function drives both the live `crossterm` backend and the `TestBackend`
//! used by snapshot tests, which is how M9 keeps deterministic coverage of
//! TUI rendering without owning a terminal.

use ratatui::backend::Backend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::Terminal;

use crate::approval::ApprovalContext;
use crate::statusline::format_status_line;
use crate::transcript::EventCard;

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
}

/// Draw one frame onto the supplied backend. Pure rendering: no input,
/// no event reading. Tests pass a `TestBackend`; the live CLI passes a
/// `CrosstermBackend`.
pub fn draw<B: Backend>(terminal: &mut Terminal<B>, frame: UiFrame<'_>) -> Result<(), B::Error> {
    terminal.draw(|f| {
        let area = f.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),                   // status line
                Constraint::Min(3),                      // transcript / approval
                Constraint::Length(diag_height(&frame)), // diagnostics
                Constraint::Length(3),                   // composer
                Constraint::Length(1),                   // footer
            ])
            .split(area);

        let status_line = format_status_line(
            frame.status_template,
            &frame.status_ctx,
            area.width as usize,
        );
        f.render_widget(
            Paragraph::new(status_line).style(Style::default().add_modifier(Modifier::BOLD)),
            layout[0],
        );

        match &frame.approval {
            Some(ctx) => {
                let lines = crate::approval::format_approval_card(ctx);
                let items: Vec<ListItem> = lines.iter().map(|l| ListItem::new(l.clone())).collect();
                f.render_widget(
                    List::new(items)
                        .block(Block::default().borders(Borders::ALL).title("Approval")),
                    layout[1],
                );
            }
            None => {
                let mut all: Vec<Line> = Vec::new();
                for card in &frame.transcript {
                    for l in card.lines() {
                        all.push(Line::from(Span::raw(l)));
                    }
                }
                f.render_widget(
                    Paragraph::new(all)
                        .wrap(Wrap { trim: false })
                        .block(Block::default().borders(Borders::ALL).title("Transcript")),
                    layout[1],
                );
            }
        }

        if diag_height(&frame) > 0 {
            let diag: Vec<ListItem> = frame
                .diagnostics
                .iter()
                .map(|l| ListItem::new(l.clone()))
                .collect();
            f.render_widget(
                List::new(diag).block(Block::default().borders(Borders::ALL).title("Activity")),
                layout[2],
            );
        }

        f.render_widget(
            Paragraph::new(format!("> {}", frame.composer))
                .block(Block::default().borders(Borders::ALL).title("Composer")),
            layout[3],
        );

        f.render_widget(Paragraph::new(frame.footer), layout[4]);

        // Touch the area we calculated to avoid unused-binding warnings on
        // very small terminals; ratatui already clamps for us.
        let _ = Rect::new(0, 0, area.width, area.height);
    })?;
    Ok(())
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
            }],
            composer: "fix the failing tests",
            footer: "[?] help [Ctrl+C] interrupt",
            approval: None,
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
            }],
            composer: "",
            footer: "",
            approval: None,
        };
        draw(&mut term, frame).unwrap();
        let s = buffer_to_string(&term);
        assert!(s.contains("Agent:"));
    }
}
