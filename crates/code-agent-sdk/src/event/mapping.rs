//! Map internal `AgentEvent` values into public `SdkEvent` values.

use code_agent_core::events::AgentEvent;

use crate::callback::SecretRedactor;
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

/// Scrub secrets from the user-visible text payloads of an `SdkEvent`
/// using the configured `SecretRedactor`, returning the redacted copy.
///
/// This is applied to the copy that is emitted to consumers / persisted
/// to storage — never to the value handed to tool execution, which still
/// needs the real input. We only touch string fields and leave the
/// structured event shape intact so downstream consumers and storage
/// formats are unaffected (#211).
pub fn redact_event(redactor: &dyn SecretRedactor, mut event: SdkEvent) -> SdkEvent {
    match &mut event {
        SdkEvent::AssistantMessage(e) => {
            e.text = redactor.redact(&e.text);
        }
        SdkEvent::AssistantTextDelta(e) => {
            e.delta = redactor.redact(&e.delta);
        }
        SdkEvent::Diagnostic(e) => {
            e.diagnostic.message = redactor.redact(&e.diagnostic.message);
        }
        SdkEvent::RunFailed(e) => {
            e.message = redactor.redact(&e.message);
            for d in &mut e.diagnostics {
                d.message = redactor.redact(&d.message);
            }
        }
        SdkEvent::RunCancelled(e) => {
            e.message = redactor.redact(&e.message);
        }
        SdkEvent::RunCompleted(e) => {
            e.summary = redactor.redact(&e.summary);
        }
        SdkEvent::ToolCallStarted(e) => {
            redact_json_strings(redactor, &mut e.input);
        }
        _ => {}
    }
    event
}

/// Recursively redact every string leaf of a JSON value in place. Object
/// keys are left untouched (they are field names, not values); only
/// string values and array string elements are scrubbed.
fn redact_json_strings(redactor: &dyn SecretRedactor, value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(s) => {
            let redacted = redactor.redact(s);
            if redacted != *s {
                *s = redacted;
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                redact_json_strings(redactor, item);
            }
        }
        serde_json::Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                redact_json_strings(redactor, v);
            }
        }
        _ => {}
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

    /// redact_event scrubs a known secret pattern out of assistant text
    /// using the DefaultSecretRedactor before the event is emitted /
    /// persisted (#211).
    #[test]
    fn redact_event_scrubs_assistant_text() {
        use crate::callback::DefaultSecretRedactor;
        let redactor = DefaultSecretRedactor;
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz0123456789";
        let ev = SdkEvent::AssistantMessage(AssistantMessageEvent {
            run_id: RunId::new(),
            text: format!("here is the key {secret} use it"),
        });
        let out = redact_event(&redactor, ev);
        match out {
            SdkEvent::AssistantMessage(e) => {
                assert!(!e.text.contains(secret), "secret leaked: {}", e.text);
                assert!(
                    e.text.contains("<redacted"),
                    "no redaction marker: {}",
                    e.text
                );
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// redact_event scrubs secrets out of tool-call input string values
    /// (the emit/persist copy) without disturbing the JSON shape (#211).
    #[test]
    fn redact_event_scrubs_tool_input_strings() {
        use crate::callback::DefaultSecretRedactor;
        let redactor = DefaultSecretRedactor;
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz0123456789";
        let ev = SdkEvent::ToolCallStarted(ToolCallStartedEvent {
            run_id: RunId::new(),
            tool_name: "bash".into(),
            tool_use_id: "t1".into(),
            input: serde_json::json!({ "command": format!("export KEY={secret}") }),
        });
        let out = redact_event(&redactor, ev);
        match out {
            SdkEvent::ToolCallStarted(e) => {
                let cmd = e.input.get("command").and_then(|v| v.as_str()).unwrap();
                assert!(!cmd.contains(secret), "secret leaked: {cmd}");
                assert!(e.input.get("command").is_some(), "shape changed");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// redact_event scrubs diagnostic message strings (#211).
    #[test]
    fn redact_event_scrubs_diagnostic_message() {
        use crate::callback::DefaultSecretRedactor;
        let redactor = DefaultSecretRedactor;
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz0123456789";
        let ev = SdkEvent::Diagnostic(DiagnosticEvent {
            run_id: RunId::new(),
            diagnostic: crate::error::Diagnostic::error(
                "run.boom",
                format!("failed with {secret}"),
            ),
        });
        let out = redact_event(&redactor, ev);
        match out {
            SdkEvent::Diagnostic(e) => {
                assert!(!e.diagnostic.message.contains(secret));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
