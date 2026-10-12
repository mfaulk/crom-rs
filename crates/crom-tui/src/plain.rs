//! Plain renderer: line-oriented output that mirrors the existing M1-M8
//! CLI behavior. The fullscreen path falls back to this when the terminal
//! cannot host an alternate-screen TUI, so it must remain stable for
//! scripts, pipes, and CI.

use std::io::Write;

use crom_core::AgentEvent;

use crate::transcript::render_event_card;

#[derive(Default)]
pub struct PlainRenderer {
    /// When true, raw JSON details are printed alongside the human-readable
    /// summary so power users can inspect any event.
    pub show_tool_json: bool,
    /// When false, child subagent events are summarized rather than echoed.
    pub show_subagent_events: bool,
}

impl PlainRenderer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Render a single event to the supplied writer. Errors are propagated
    /// so the caller can surface them; partial writes do not corrupt the
    /// event stream because every line is fully written before the next.
    pub fn render(&self, event: &AgentEvent, out: &mut dyn Write) -> std::io::Result<()> {
        if matches!(event, AgentEvent::SubagentEvent(_)) && !self.show_subagent_events {
            return Ok(());
        }
        let card = render_event_card(event);
        for line in card.lines() {
            out.write_all(line.as_bytes())?;
            out.write_all(b"\n")?;
        }
        if self.show_tool_json {
            if let Ok(s) = serde_json::to_string(event) {
                writeln!(out, "    json: {s}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::{AssistantContent, AssistantMessage};

    #[test]
    // Plain renderer emits human-readable lines for an assistant message.
    fn renders_assistant_message() {
        let event = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::Text {
                text: "hello world".to_string(),
            }],
            stop_reason: None,
        });
        let mut buf = Vec::new();
        PlainRenderer::new().render(&event, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("hello world"));
    }

    #[test]
    // show_tool_json adds a json line for any event.
    fn show_tool_json_includes_json() {
        let event = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::Text {
                text: "x".to_string(),
            }],
            stop_reason: None,
        });
        let mut r = PlainRenderer::new();
        r.show_tool_json = true;
        let mut buf = Vec::new();
        r.render(&event, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("json:"));
    }
}
