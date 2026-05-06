//! Event-to-card rendering. Produces a deterministic, plain-text
//! representation of every `AgentEvent` so the same code drives the plain
//! renderer, the fullscreen transcript pane, and tests.

use code_agent_core::{AgentEvent, AssistantContent};

/// One transcript entry. Multi-line cards are split by newlines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventCard {
    pub kind: &'static str,
    pub headline: String,
    pub body: Vec<String>,
}

impl EventCard {
    /// Iterator over headline + body lines.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::with_capacity(self.body.len() + 1);
        out.push(self.headline.clone());
        for line in &self.body {
            out.push(format!("  {line}"));
        }
        out
    }
}

/// Render any AgentEvent to a card. Unknown events get a generic card so
/// the renderer never panics on a future event type.
pub fn render_event_card(event: &AgentEvent) -> EventCard {
    match event {
        AgentEvent::AssistantMessage(am) => {
            let text = am
                .content
                .iter()
                .filter_map(|b| match b {
                    AssistantContent::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            EventCard {
                kind: "assistant",
                headline: "Agent:".to_string(),
                body: text.lines().map(String::from).collect(),
            }
        }
        AgentEvent::AssistantTextDelta(delta) => EventCard {
            kind: "assistant_delta",
            headline: "...".to_string(),
            body: vec![delta.clone()],
        },
        AgentEvent::ToolCallStarted(t) => EventCard {
            kind: "tool_started",
            headline: format!("▶ {}", t.call.name),
            body: vec![preview(&t.call.input.to_string(), 200)],
        },
        AgentEvent::ToolCallCompleted(t) => EventCard {
            kind: "tool_completed",
            headline: format!(
                "{} {} ({}ms)",
                if t.is_error { "✗" } else { "✓" },
                t.tool_name,
                t.duration_ms
            ),
            body: Vec::new(),
        },
        AgentEvent::ToolCallDenied(t) => EventCard {
            kind: "tool_denied",
            headline: format!("✗ denied: {}", t.tool_name),
            body: vec![t.reason.clone()],
        },
        AgentEvent::UserToolResult(r) => EventCard {
            kind: "tool_result",
            headline: format!(
                "{} {} result",
                if r.is_error { "✗" } else { "·" },
                r.tool_name
            ),
            body: vec![preview(&r.content, 200)],
        },
        AgentEvent::TodoUpdate(t) => EventCard {
            kind: "todo",
            headline: format!("Todos: {} item(s)", t.list.todos.len()),
            body: t
                .list
                .todos
                .iter()
                .map(|i| format!("[{:?}] {}", i.status, i.content))
                .collect(),
        },
        AgentEvent::Interrupted(i) => EventCard {
            kind: "interrupted",
            headline: format!("Interrupted (turn {})", i.turn_index),
            body: Vec::new(),
        },
        AgentEvent::HookEvaluated(h) => EventCard {
            kind: "hook",
            headline: format!("hook: {:?}", h.outcome),
            body: h
                .reason
                .clone()
                .into_iter()
                .chain(h.tool_name.clone())
                .collect(),
        },
        AgentEvent::PolicyDecision(p) => EventCard {
            kind: "policy",
            headline: format!("policy: {} {:?}", p.tool_name, p.outcome),
            body: p.reason.clone().into_iter().collect(),
        },
        AgentEvent::Result(r) => EventCard {
            kind: "result",
            headline: format!("Result: {:?}", r.subtype),
            body: r
                .result
                .clone()
                .map(|s| vec![preview(&s, 1000)])
                .unwrap_or_default(),
        },
        AgentEvent::CheckpointEvent(p) => EventCard {
            kind: "checkpoint",
            headline: format!("checkpoint: {}", p.event),
            body: Vec::new(),
        },
        AgentEvent::ApprovalPromptShown(a) => EventCard {
            kind: "approval_shown",
            headline: format!("Approve {}?", a.tool_name),
            body: a
                .source
                .clone()
                .map(|s| vec![format!("source: {s}")])
                .unwrap_or_default(),
        },
        AgentEvent::ApprovalPromptResolved(a) => EventCard {
            kind: "approval_resolved",
            headline: format!("Approval: {} → {:?}", a.tool_name, a.decision),
            body: Vec::new(),
        },
        AgentEvent::ApprovalRevisionRequested(a) => EventCard {
            kind: "approval_revision",
            headline: format!("Revision requested for {}", a.tool_name),
            body: vec![a.message_preview.clone()],
        },
        AgentEvent::DiffViewed(d) => EventCard {
            kind: "diff_viewed",
            headline: format!("diff: {} files, {} bytes", d.files_changed, d.bytes),
            body: Vec::new(),
        },
        AgentEvent::ChangesViewed(c) => EventCard {
            kind: "changes_viewed",
            headline: format!("changes: {} files", c.files_changed),
            body: Vec::new(),
        },
        AgentEvent::DoctorCheckCompleted(d) => EventCard {
            kind: "doctor",
            headline: format!("doctor.{}: {:?}", d.check_id, d.status),
            body: vec![d.message.clone()],
        },
        AgentEvent::SessionRecovered(s) => EventCard {
            kind: "session_recovered",
            headline: format!(
                "recovered: {} ({} records, {} skipped)",
                s.session_id, s.records_recovered, s.records_skipped
            ),
            body: Vec::new(),
        },
        AgentEvent::SessionRecoveryFailed(s) => EventCard {
            kind: "session_recovery_failed",
            headline: format!("recovery failed: {}", s.session_id),
            body: vec![s.reason.clone()],
        },
        AgentEvent::UiStarted(u) => EventCard {
            kind: "ui_started",
            headline: format!(
                "ui: {} ({}x{}, tty={})",
                u.renderer, u.width, u.height, u.is_tty
            ),
            body: Vec::new(),
        },
        AgentEvent::UiRendererChanged(u) => EventCard {
            kind: "ui_renderer_changed",
            headline: format!("ui: {} → {}", u.from, u.to),
            body: vec![u.reason.clone()],
        },
        AgentEvent::StatusLineConfigured(s) => EventCard {
            kind: "statusline",
            headline: "status line updated".to_string(),
            body: vec![s.template.clone()],
        },
        AgentEvent::NotificationSent(n) => EventCard {
            kind: "notification_sent",
            headline: format!("notify: {}/{}", n.channel, n.event),
            body: Vec::new(),
        },
        AgentEvent::NotificationFailed(n) => EventCard {
            kind: "notification_failed",
            headline: format!("notify-fail: {}/{}", n.channel, n.event),
            body: vec![n.reason.clone()],
        },
        AgentEvent::InitStarted(i) => EventCard {
            kind: "init_started",
            headline: format!("init: {} → {}", i.mode, i.cwd.display()),
            body: Vec::new(),
        },
        AgentEvent::InitCompleted(i) => EventCard {
            kind: "init_completed",
            headline: format!("init done: {}", i.mode),
            body: Vec::new(),
        },
        // Generic fallback: never panic on a future-added variant. Use
        // the JSON tag if available so the user can still see what kind
        // of event it was.
        other => {
            let kind: &'static str = "event";
            let v = serde_json::to_value(other).unwrap_or(serde_json::Value::Null);
            let tag = v
                .get("type")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string();
            EventCard {
                kind,
                headline: format!("· {tag}"),
                body: Vec::new(),
            }
        }
    }
}

/// Truncate a long string with an ellipsis at character boundaries.
pub(crate) fn preview(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for (count, ch) in s.chars().enumerate() {
        if count >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::{AgentResult, AssistantMessage, ResultSubtype, SessionId};

    #[test]
    // Long assistant text wraps into multiple body lines.
    fn long_assistant_message_wraps() {
        let event = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::Text {
                text: "line1\nline2\nline3".into(),
            }],
            stop_reason: None,
        });
        let card = render_event_card(&event);
        assert_eq!(card.body, vec!["line1", "line2", "line3"]);
    }

    #[test]
    // Result events render their subtype.
    fn result_card_includes_subtype() {
        let event = AgentEvent::Result(AgentResult {
            subtype: ResultSubtype::Success,
            result: Some("ok".into()),
            session_id: SessionId::new(),
            run_id: None,
            num_turns: 1,
            usage: None,
            total_cost_usd: None,
        });
        let card = render_event_card(&event);
        assert!(card.headline.contains("Success"));
        assert_eq!(card.body, vec!["ok".to_string()]);
    }

    #[test]
    // Unknown text deltas render without panicking.
    fn delta_card() {
        let event = AgentEvent::AssistantTextDelta("partial".into());
        let card = render_event_card(&event);
        assert_eq!(card.kind, "assistant_delta");
    }

    #[test]
    // preview() truncates with an ellipsis past max_chars.
    fn preview_truncates() {
        assert_eq!(preview("abcdef", 3), "abc…");
    }
}
