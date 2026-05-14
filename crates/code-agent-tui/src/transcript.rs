//! Event-to-card rendering. Produces a deterministic, plain-text
//! representation of every `AgentEvent` so the same code drives the plain
//! renderer, the fullscreen transcript pane, and tests.

use code_agent_core::{AgentEvent, AssistantContent};

/// One transcript entry. Multi-line cards are split by newlines.
///
/// M13 Phase C extensions:
/// * `id` is assigned by [`crate::state::TuiAppState::push_transcript`]
///   when the card enters state, and is stable for the lifetime of the
///   session. It survives search-filtering and scrolling so the reducer
///   can keep "selected" / "expanded" sets that don't drift when new
///   cards arrive or older cards age out of the bounded transcript.
/// * `expanded_body` is shown only when the user toggles the card open.
///   Most cards leave it empty for Phase C; later phases will populate
///   it with raw JSON / tool stdout / etc.
/// * `source` is optional attribution (plugin name, MCP server,
///   subagent name) rendered as a parenthesized suffix.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct EventCard {
    pub kind: &'static str,
    pub headline: String,
    pub body: Vec<String>,
    pub id: u64,
    pub expanded_body: Vec<String>,
    pub source: Option<String>,
}

impl EventCard {
    /// Iterator over headline + body lines.
    pub fn lines(&self) -> Vec<String> {
        self.lines_with_expansion(false)
    }

    /// Same as [`Self::lines`], but with the `expanded_body` appended
    /// when `expanded` is true.
    pub fn lines_with_expansion(&self, expanded: bool) -> Vec<String> {
        let extra = if expanded {
            self.expanded_body.len()
        } else {
            0
        };
        let mut out = Vec::with_capacity(self.body.len() + 1 + extra);
        let headline = match &self.source {
            Some(s) => format!("{}  ({})", self.headline, s),
            None => self.headline.clone(),
        };
        out.push(headline);
        for line in &self.body {
            out.push(format!("  {line}"));
        }
        if expanded {
            for line in &self.expanded_body {
                out.push(format!("  {line}"));
            }
        }
        out
    }

    /// Does the card match the given case-insensitive substring? Used
    /// by transcript search to filter the visible set.
    pub fn matches_search(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return true;
        }
        let n = needle.to_lowercase();
        if self.headline.to_lowercase().contains(&n) {
            return true;
        }
        if self.body.iter().any(|l| l.to_lowercase().contains(&n)) {
            return true;
        }
        if self
            .expanded_body
            .iter()
            .any(|l| l.to_lowercase().contains(&n))
        {
            return true;
        }
        if let Some(src) = &self.source {
            if src.to_lowercase().contains(&n) {
                return true;
            }
        }
        false
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
                ..Default::default()
            }
        }
        AgentEvent::AssistantTextDelta(delta) => EventCard {
            kind: "assistant_delta",
            headline: "...".to_string(),
            body: vec![delta.clone()],
            ..Default::default()
        },
        AgentEvent::ToolCallStarted(t) => EventCard {
            kind: "tool_started",
            headline: format!("▶ {}", t.call.name),
            body: vec![preview(&t.call.input.to_string(), 200)],
            ..Default::default()
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
            ..Default::default()
        },
        AgentEvent::ToolCallDenied(t) => EventCard {
            kind: "tool_denied",
            headline: format!("✗ denied: {}", t.tool_name),
            body: vec![t.reason.clone()],
            ..Default::default()
        },
        AgentEvent::UserToolResult(r) => EventCard {
            kind: "tool_result",
            headline: format!(
                "{} {} result",
                if r.is_error { "✗" } else { "·" },
                r.tool_name
            ),
            body: vec![preview(&r.content, 200)],
            ..Default::default()
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
            ..Default::default()
        },
        AgentEvent::Interrupted(i) => EventCard {
            kind: "interrupted",
            headline: format!("Interrupted (turn {})", i.turn_index),
            body: Vec::new(),
            ..Default::default()
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
            ..Default::default()
        },
        AgentEvent::PolicyDecision(p) => EventCard {
            kind: "policy",
            headline: format!("policy: {} {:?}", p.tool_name, p.outcome),
            body: p.reason.clone().into_iter().collect(),
            ..Default::default()
        },
        AgentEvent::Result(r) => EventCard {
            kind: "result",
            headline: format!("Result: {:?}", r.subtype),
            body: r
                .result
                .clone()
                .map(|s| vec![preview(&s, 1000)])
                .unwrap_or_default(),
            ..Default::default()
        },
        AgentEvent::CheckpointEvent(p) => EventCard {
            kind: "checkpoint",
            headline: format!("checkpoint: {}", p.event),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::ApprovalPromptShown(a) => EventCard {
            kind: "approval_shown",
            headline: format!("Approve {}?", a.tool_name),
            body: a
                .source
                .clone()
                .map(|s| vec![format!("source: {s}")])
                .unwrap_or_default(),
            ..Default::default()
        },
        AgentEvent::ApprovalPromptResolved(a) => EventCard {
            kind: "approval_resolved",
            headline: format!("Approval: {} → {:?}", a.tool_name, a.decision),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::ApprovalRevisionRequested(a) => EventCard {
            kind: "approval_revision",
            headline: format!("Revision requested for {}", a.tool_name),
            body: vec![a.message_preview.clone()],
            ..Default::default()
        },
        AgentEvent::DiffViewed(d) => EventCard {
            kind: "diff_viewed",
            headline: format!("diff: {} files, {} bytes", d.files_changed, d.bytes),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::ChangesViewed(c) => EventCard {
            kind: "changes_viewed",
            headline: format!("changes: {} files", c.files_changed),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::DoctorCheckCompleted(d) => EventCard {
            kind: "doctor",
            headline: format!("doctor.{}: {:?}", d.check_id, d.status),
            body: vec![d.message.clone()],
            ..Default::default()
        },
        AgentEvent::SessionRecovered(s) => EventCard {
            kind: "session_recovered",
            headline: format!(
                "recovered: {} ({} records, {} skipped)",
                s.session_id, s.records_recovered, s.records_skipped
            ),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::SessionRecoveryFailed(s) => EventCard {
            kind: "session_recovery_failed",
            headline: format!("recovery failed: {}", s.session_id),
            body: vec![s.reason.clone()],
            ..Default::default()
        },
        AgentEvent::UiStarted(u) => EventCard {
            kind: "ui_started",
            headline: format!(
                "ui: {} ({}x{}, tty={})",
                u.renderer, u.width, u.height, u.is_tty
            ),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::UiRendererChanged(u) => EventCard {
            kind: "ui_renderer_changed",
            headline: format!("ui: {} → {}", u.from, u.to),
            body: vec![u.reason.clone()],
            ..Default::default()
        },
        AgentEvent::StatusLineConfigured(s) => EventCard {
            kind: "statusline",
            headline: "status line updated".to_string(),
            body: vec![s.template.clone()],
            ..Default::default()
        },
        AgentEvent::NotificationSent(n) => EventCard {
            kind: "notification_sent",
            headline: format!("notify: {}/{}", n.channel, n.event),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::NotificationFailed(n) => EventCard {
            kind: "notification_failed",
            headline: format!("notify-fail: {}/{}", n.channel, n.event),
            body: vec![n.reason.clone()],
            ..Default::default()
        },
        AgentEvent::InitStarted(i) => EventCard {
            kind: "init_started",
            headline: format!("init: {} → {}", i.mode, i.cwd.display()),
            body: Vec::new(),
            ..Default::default()
        },
        AgentEvent::InitCompleted(i) => EventCard {
            kind: "init_completed",
            headline: format!("init done: {}", i.mode),
            body: Vec::new(),
            ..Default::default()
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
                ..Default::default()
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
