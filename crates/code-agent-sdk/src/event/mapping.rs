//! Map internal `AgentEvent` values into public `SdkEvent` values.

use code_agent_core::events::AgentEvent;

use crate::event::{
    AssistantMessageEvent, AssistantTextDelta, DiagnosticEvent, InternalEvent, SdkEvent,
    ToolCallCompletedEvent, ToolCallStartedEvent,
};
use crate::RunId;

/// Stateful mapper that knows the active `run_id`. Allows the runtime to
/// produce `SdkEvent`s without duplicating context across call sites.
pub struct SdkEventMapper {
    run_id: RunId,
    verbosity: crate::event::EventVerbosity,
}

impl SdkEventMapper {
    pub fn new(run_id: RunId, verbosity: crate::event::EventVerbosity) -> Self {
        Self { run_id, verbosity }
    }

    pub fn map(&self, ev: &AgentEvent) -> Option<SdkEvent> {
        use crate::event::EventVerbosity::*;
        let standard = matches!(self.verbosity, Standard | Full);
        let full = matches!(self.verbosity, Full);
        match ev {
            AgentEvent::AssistantMessage(am) => standard.then(|| {
                let mut text = String::new();
                for c in &am.content {
                    if let code_agent_core::conversation::AssistantContent::Text { text: t } = c {
                        text.push_str(t);
                    }
                }
                SdkEvent::AssistantMessage(AssistantMessageEvent {
                    run_id: self.run_id.clone(),
                    text,
                })
            }),
            AgentEvent::AssistantTextDelta(delta) => full.then(|| {
                SdkEvent::AssistantTextDelta(AssistantTextDelta {
                    run_id: self.run_id.clone(),
                    delta: delta.clone(),
                })
            }),
            AgentEvent::ToolCallStarted(s) => standard.then(|| {
                SdkEvent::ToolCallStarted(ToolCallStartedEvent {
                    run_id: self.run_id.clone(),
                    tool_name: s.call.name.clone(),
                    tool_use_id: s.call.id.clone(),
                    input: s.call.input.clone(),
                })
            }),
            AgentEvent::ToolCallCompleted(c) => standard.then(|| {
                SdkEvent::ToolCallCompleted(ToolCallCompletedEvent {
                    run_id: self.run_id.clone(),
                    tool_name: c.tool_name.clone(),
                    tool_use_id: c.tool_use_id.clone(),
                    is_error: c.is_error,
                    duration_ms: c.duration_ms,
                })
            }),
            AgentEvent::ToolCallDenied(d) => Some(SdkEvent::Diagnostic(DiagnosticEvent {
                run_id: self.run_id.clone(),
                diagnostic: crate::error::Diagnostic::warning(
                    "tool.permission_denied",
                    d.reason.clone(),
                )
                .with_location(crate::error::DiagnosticLocation::Tool {
                    name: d.tool_name.clone(),
                }),
            })),
            other if full => {
                // Wrap any other internal event opaquely.
                let payload = serde_json::to_value(other).unwrap_or_default();
                let kind = payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("internal")
                    .to_string();
                Some(SdkEvent::Internal(InternalEvent {
                    run_id: self.run_id.clone(),
                    kind,
                    payload,
                }))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::conversation::{AssistantContent, AssistantMessage};

    /// Minimal verbosity suppresses assistant messages.
    #[test]
    fn minimal_suppresses_assistant_messages() {
        let mapper = SdkEventMapper::new(RunId::new(), crate::event::EventVerbosity::Minimal);
        let ev = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::Text {
                text: "hello".into(),
            }],
            stop_reason: None,
        });
        assert!(mapper.map(&ev).is_none());
    }

    /// Standard verbosity emits an AssistantMessage SdkEvent.
    #[test]
    fn standard_emits_assistant() {
        let mapper = SdkEventMapper::new(RunId::new(), crate::event::EventVerbosity::Standard);
        let ev = AgentEvent::AssistantMessage(AssistantMessage {
            content: vec![AssistantContent::Text {
                text: "hello".into(),
            }],
            stop_reason: None,
        });
        let out = mapper.map(&ev).unwrap();
        matches!(out, SdkEvent::AssistantMessage(_));
    }
}
