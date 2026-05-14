//! Adapter that turns a public `code_agent_sdk::SdkEvent` into one or more
//! transcript `EventCard`s plus an optional activity-log entry. The TUI
//! consumes SDK events through this mapper so the renderer stays decoupled
//! from any provider/runtime details and remains snapshot-testable.

use code_agent_sdk::SdkEvent;

use crate::transcript::{preview, EventCard};

/// How a single SDK event should mutate the transcript and activity log.
#[derive(Debug, Clone, Default)]
pub struct SdkEventOutcome {
    /// A new transcript card to append, if any.
    pub card: Option<EventCard>,
    /// If `Some`, the previous card's last body line should be replaced /
    /// extended with this fragment. Used to coalesce streaming text deltas
    /// into a single card instead of producing one card per delta.
    pub coalesce_assistant_delta: Option<String>,
    /// Activity-log line (e.g. "Bash running…"). Bounded by the caller.
    pub activity: Option<String>,
    /// `Some(true)` means the run completed successfully; `Some(false)`
    /// means it failed or was cancelled. `None` means no terminal state.
    pub run_terminal: Option<bool>,
    /// Error message to surface, if the event represents a failure.
    pub error: Option<String>,
}

impl SdkEventOutcome {
    fn with_card(card: EventCard) -> Self {
        Self {
            card: Some(card),
            ..Self::default()
        }
    }
}

/// Map an `SdkEvent` to a transcript outcome.
///
/// Unknown / future variants render as a generic card so the TUI never
/// panics on a forward-compatible enum addition.
pub fn map_sdk_event(event: &SdkEvent) -> SdkEventOutcome {
    match event {
        SdkEvent::RunStarted(e) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "run_started",
                headline: format!("▶ run started ({}/{})", e.provider.as_str(), e.model),
                body: Vec::new(),
                ..Default::default()
            }),
            activity: Some(format!("run: {} {}", e.provider.as_str(), e.model)),
            ..SdkEventOutcome::default()
        },
        SdkEvent::PlanningStarted(_) => SdkEventOutcome::with_card(EventCard {
            kind: "planning_started",
            headline: "▶ planning…".to_string(),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::PlanCreated(_) => SdkEventOutcome::with_card(EventCard {
            kind: "plan_created",
            headline: "plan created".to_string(),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::PlanApproved(_) => SdkEventOutcome::with_card(EventCard {
            kind: "plan_approved",
            headline: "plan approved".to_string(),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::ActStarted(_) => SdkEventOutcome::with_card(EventCard {
            kind: "act_started",
            headline: "▶ act started".to_string(),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::AssistantMessage(m) => SdkEventOutcome::with_card(EventCard {
            kind: "assistant",
            headline: "Agent:".to_string(),
            body: m.text.lines().map(String::from).collect(),
            ..Default::default()
        }),
        SdkEvent::AssistantTextDelta(d) => SdkEventOutcome {
            coalesce_assistant_delta: Some(d.delta.clone()),
            ..SdkEventOutcome::default()
        },
        SdkEvent::ToolCallStarted(t) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "tool_started",
                headline: format!("▶ {}", t.tool_name),
                body: vec![preview(&t.input.to_string(), 200)],
                ..Default::default()
            }),
            activity: Some(format!("tool: {}", t.tool_name)),
            ..SdkEventOutcome::default()
        },
        SdkEvent::ToolCallCompleted(t) => SdkEventOutcome::with_card(EventCard {
            kind: "tool_completed",
            headline: format!(
                "{} {} ({}ms)",
                if t.is_error { "✗" } else { "✓" },
                t.tool_name,
                t.duration_ms
            ),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::ApprovalRequired(a) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "approval_required",
                headline: format!("? approve {}?", a.tool_name),
                body: vec![preview(&a.summary, 200)],
                ..Default::default()
            }),
            activity: Some(format!("approval: {}", a.tool_name)),
            ..SdkEventOutcome::default()
        },
        SdkEvent::ApprovalResolved(a) => SdkEventOutcome::with_card(EventCard {
            kind: "approval_resolved",
            headline: format!(
                "approval: {} → {}",
                a.tool_name,
                approval_label(&a.decision)
            ),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::PatchProposed(p) => SdkEventOutcome::with_card(EventCard {
            kind: "patch_proposed",
            headline: format!("patch: {} file(s)", p.patch.files.len()),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::FileGenerated(f) => SdkEventOutcome::with_card(EventCard {
            kind: "file_generated",
            headline: format!("file: {}", f.path.as_str()),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::ValidationStarted(v) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "validation_started",
                headline: format!("▶ validation: {}", v.command),
                body: Vec::new(),
                ..Default::default()
            }),
            activity: Some(format!("validate: {}", v.command)),
            ..SdkEventOutcome::default()
        },
        SdkEvent::ValidationCompleted(v) => SdkEventOutcome::with_card(EventCard {
            kind: "validation_completed",
            headline: format!(
                "{} validation: {} (exit={:?})",
                if v.report.success { "✓" } else { "✗" },
                v.report.command,
                v.report.exit_code
            ),
            body: Vec::new(),
            ..Default::default()
        }),
        SdkEvent::Diagnostic(d) => SdkEventOutcome::with_card(EventCard {
            kind: "diagnostic",
            headline: format!("⚠ {:?}: {}", d.diagnostic.severity, d.diagnostic.code),
            body: vec![preview(&d.diagnostic.message, 240)],
            ..Default::default()
        }),
        SdkEvent::RunCompleted(e) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "run_completed",
                headline: format!("✓ run {:?}", e.status),
                body: if e.summary.is_empty() {
                    Vec::new()
                } else {
                    vec![preview(&e.summary, 240)]
                },
                ..Default::default()
            }),
            activity: None,
            run_terminal: Some(true),
            ..SdkEventOutcome::default()
        },
        SdkEvent::RunFailed(e) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "run_failed",
                headline: format!("✗ run failed ({:?})", e.status),
                body: vec![preview(&e.message, 240)],
                ..Default::default()
            }),
            run_terminal: Some(false),
            error: Some(preview(&e.message, 240)),
            ..SdkEventOutcome::default()
        },
        SdkEvent::RunCancelled(e) => SdkEventOutcome {
            card: Some(EventCard {
                kind: "run_cancelled",
                headline: "· run cancelled".to_string(),
                body: if e.message.is_empty() {
                    Vec::new()
                } else {
                    vec![preview(&e.message, 240)]
                },
                ..Default::default()
            }),
            run_terminal: Some(false),
            ..SdkEventOutcome::default()
        },
        SdkEvent::Internal(i) => SdkEventOutcome::with_card(EventCard {
            kind: "internal",
            headline: format!("· {}", i.kind),
            body: Vec::new(),
            ..Default::default()
        }),
    }
}

fn approval_label(decision: &code_agent_sdk::event::ApprovalDecisionWire) -> &'static str {
    use code_agent_sdk::event::ApprovalDecisionWire;
    match decision {
        ApprovalDecisionWire::Allow => "allow",
        ApprovalDecisionWire::AllowForSession => "allow (session)",
        ApprovalDecisionWire::Deny { .. } => "deny",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_sdk::event::{
        AssistantMessageEvent, AssistantTextDelta, RunCompletedEvent, RunFailedEvent, RunStarted,
        ToolCallCompletedEvent, ToolCallStartedEvent,
    };
    use code_agent_sdk::request::RequestMetadata;
    use code_agent_sdk::result::{AgentRunStatus, EventSummary};
    use code_agent_sdk::{ProviderId, RunId};

    fn rid() -> RunId {
        RunId::new()
    }

    #[test]
    // RunStarted produces a run_started card and an activity entry naming
    // the provider and model.
    fn run_started_card_and_activity() {
        let ev = SdkEvent::RunStarted(RunStarted {
            run_id: rid(),
            session_id: None,
            provider: ProviderId::Fake,
            model: "fake-model".to_string(),
            metadata: RequestMetadata::default(),
        });
        let out = map_sdk_event(&ev);
        let card = out.card.expect("expected card");
        assert_eq!(card.kind, "run_started");
        assert!(card.headline.contains("fake"));
        assert!(card.headline.contains("fake-model"));
        let activity = out.activity.expect("expected activity");
        assert!(activity.contains("fake"));
    }

    #[test]
    // AssistantTextDelta does not produce its own card; it surfaces a
    // coalesce hint so the reducer can extend the previous assistant card.
    fn delta_coalesces_into_previous_assistant_card() {
        let ev = SdkEvent::AssistantTextDelta(AssistantTextDelta {
            run_id: rid(),
            delta: "partial".to_string(),
        });
        let out = map_sdk_event(&ev);
        assert!(out.card.is_none());
        assert_eq!(out.coalesce_assistant_delta.as_deref(), Some("partial"));
    }

    #[test]
    // AssistantMessage emits an assistant card whose body splits on newlines.
    fn assistant_message_splits_lines() {
        let ev = SdkEvent::AssistantMessage(AssistantMessageEvent {
            run_id: rid(),
            text: "line1\nline2".to_string(),
        });
        let card = map_sdk_event(&ev).card.unwrap();
        assert_eq!(card.kind, "assistant");
        assert_eq!(card.body, vec!["line1".to_string(), "line2".to_string()]);
    }

    #[test]
    // ToolCallStarted and ToolCallCompleted each produce cards reflecting
    // tool name and duration.
    fn tool_call_cards() {
        let started = SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: rid(),
            tool_name: "Bash".to_string(),
            tool_use_id: "1".to_string(),
            input: serde_json::json!({"command": "ls"}),
        });
        let completed = SdkEvent::ToolCallCompleted(ToolCallCompletedEvent {
            run_id: rid(),
            tool_name: "Bash".to_string(),
            tool_use_id: "1".to_string(),
            is_error: false,
            duration_ms: 42,
        });
        let s = map_sdk_event(&started).card.unwrap();
        let c = map_sdk_event(&completed).card.unwrap();
        assert!(s.headline.starts_with("▶ Bash"));
        assert!(c.headline.contains("Bash"));
        assert!(c.headline.contains("42ms"));
    }

    #[test]
    // RunCompleted marks the run as a successful terminal state.
    fn run_completed_marks_terminal_success() {
        let ev = SdkEvent::RunCompleted(RunCompletedEvent {
            run_id: rid(),
            status: AgentRunStatus::Completed,
            summary: "ok".to_string(),
            usage: None,
            events_summary: EventSummary::default(),
        });
        let out = map_sdk_event(&ev);
        assert_eq!(out.run_terminal, Some(true));
        assert_eq!(out.card.unwrap().kind, "run_completed");
        assert!(out.error.is_none());
    }

    #[test]
    // RunFailed marks the run as a failed terminal state and surfaces the
    // error message.
    fn run_failed_marks_terminal_failure_with_error() {
        let ev = SdkEvent::RunFailed(RunFailedEvent {
            run_id: rid(),
            status: AgentRunStatus::Failed,
            message: "boom".to_string(),
            diagnostics: Vec::new(),
        });
        let out = map_sdk_event(&ev);
        assert_eq!(out.run_terminal, Some(false));
        assert_eq!(out.error.as_deref(), Some("boom"));
    }
}
