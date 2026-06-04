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
            AgentEvent::AssistantTextDelta { text: delta } => full.then(|| {
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

/// Scrub secrets from the free-form text payloads of an `SdkEvent` using
/// the configured `SecretRedactor`, returning the redacted copy.
///
/// Covers every variant that can carry model- or command-generated text:
/// assistant text/deltas, diagnostic and run-status messages, tool-call
/// input, validation command text + captured stdout/stderr, and opaque
/// internal-event JSON. Variants that only hold identifiers, enums, counts,
/// token usage, or workspace paths/patches (the host's own on-disk files)
/// are intentionally left untouched.
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
        // Validation runs arbitrary commands whose text and captured
        // stdout/stderr are surfaced to consumers — a prime leak vector.
        SdkEvent::ValidationStarted(e) => {
            e.command = redactor.redact(&e.command);
        }
        SdkEvent::ValidationCompleted(e) => {
            e.report.command = redactor.redact(&e.report.command);
            e.report.stdout = redactor.redact(&e.report.stdout);
            e.report.stderr = redactor.redact(&e.report.stderr);
        }
        // Opaque internal events are emitted verbatim under full verbosity;
        // scrub their JSON string leaves.
        SdkEvent::Internal(e) => {
            redact_json_strings(redactor, &mut e.payload);
        }
        // Remaining variants carry no free-form text that the agent can
        // populate with secret material: identifiers, enums, counts, token
        // usage, and workspace paths/patches (the host's own files, already
        // on disk). Left intact so the structured event shape is preserved.
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
    use crate::event::EventVerbosity;
    use code_agent_core::conversation::{AssistantContent, AssistantMessage, ToolCall};
    use code_agent_core::events::{ToolCallStarted, UserToolResult};

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
        assert!(matches!(out, SdkEvent::AssistantMessage(_)));
    }

    fn mapper(v: EventVerbosity) -> SdkEventMapper {
        SdkEventMapper::new(RunId::new(), v)
    }

    fn tool_started() -> AgentEvent {
        AgentEvent::ToolCallStarted(ToolCallStarted {
            call: ToolCall {
                id: "t1".into(),
                name: "Read".into(),
                input: serde_json::json!({"file_path": "a.txt"}),
            },
        })
    }

    /// AssistantTextDelta is a Full-only event: suppressed under Minimal and
    /// Standard, mapped under Full. Pins the `full.then(..)` gate.
    #[test]
    fn assistant_text_delta_is_full_only() {
        let ev = AgentEvent::AssistantTextDelta {
            text: "chunk".into(),
        };
        assert!(mapper(EventVerbosity::Minimal).map(&ev).is_none());
        assert!(mapper(EventVerbosity::Standard).map(&ev).is_none());
        let out = mapper(EventVerbosity::Full)
            .map(&ev)
            .expect("full maps delta");
        match out {
            SdkEvent::AssistantTextDelta(d) => assert_eq!(d.delta, "chunk"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// ToolCallStarted is a Standard+ event: suppressed under Minimal, mapped
    /// under Standard and Full, carrying the call's name/id/input.
    #[test]
    fn tool_call_started_is_standard_plus() {
        assert!(mapper(EventVerbosity::Minimal)
            .map(&tool_started())
            .is_none());
        for v in [EventVerbosity::Standard, EventVerbosity::Full] {
            let out = mapper(v)
                .map(&tool_started())
                .expect("standard+ maps tool start");
            match out {
                SdkEvent::ToolCallStarted(e) => {
                    assert_eq!(e.tool_name, "Read");
                    assert_eq!(e.tool_use_id, "t1");
                    assert_eq!(
                        e.input.get("file_path").and_then(|v| v.as_str()),
                        Some("a.txt")
                    );
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// ToolCallDenied always maps to a Diagnostic regardless of verbosity —
    /// even under Minimal, where every other mapped event is suppressed. The
    /// arm returns `Some(..)` unconditionally rather than gating on verbosity.
    #[test]
    fn tool_call_denied_maps_under_every_verbosity() {
        let ev = AgentEvent::ToolCallDenied(code_agent_core::events::ToolCallDenied {
            tool_use_id: "t9".into(),
            tool_name: "Bash".into(),
            reason: "blocked by policy".into(),
        });
        for v in [
            EventVerbosity::Minimal,
            EventVerbosity::Standard,
            EventVerbosity::Full,
        ] {
            let out = mapper(v).map(&ev).expect("denied always maps");
            match out {
                SdkEvent::Diagnostic(d) => {
                    assert_eq!(d.diagnostic.code, "tool.permission_denied");
                    assert!(d.diagnostic.message.contains("blocked by policy"));
                }
                other => panic!("unexpected: {other:?}"),
            }
        }
    }

    /// An otherwise-unmapped internal event (UserToolResult) is dropped under
    /// Minimal/Standard but wrapped as an opaque `Internal` SdkEvent under
    /// Full, with `kind` taken from the serialized `type` tag.
    #[test]
    fn unmapped_event_wraps_as_internal_only_under_full() {
        let ev = AgentEvent::UserToolResult(UserToolResult {
            tool_use_id: "t1".into(),
            tool_name: "Read".into(),
            content: "ok".into(),
            is_error: false,
        });
        assert!(mapper(EventVerbosity::Minimal).map(&ev).is_none());
        assert!(mapper(EventVerbosity::Standard).map(&ev).is_none());
        let out = mapper(EventVerbosity::Full)
            .map(&ev)
            .expect("full wraps unmapped events");
        match out {
            SdkEvent::Internal(e) => {
                assert!(!e.kind.is_empty(), "kind should derive from type tag");
                assert!(e.payload.is_object());
            }
            other => panic!("unexpected: {other:?}"),
        }
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

    /// redact_event scrubs validation command output (stdout/stderr), a
    /// prime leak vector since validation runs arbitrary commands (#211).
    #[test]
    fn redact_event_scrubs_validation_output() {
        use crate::callback::DefaultSecretRedactor;
        use crate::event::ValidationCompletedEvent;
        use crate::result::ValidationReport;
        let redactor = DefaultSecretRedactor;
        let secret = "sk-ant-abcdefghijklmnopqrstuvwxyz0123456789";
        let ev = SdkEvent::ValidationCompleted(ValidationCompletedEvent {
            run_id: RunId::new(),
            report: ValidationReport {
                command: format!("echo {secret}"),
                success: true,
                exit_code: Some(0),
                stdout: format!("leaked {secret}"),
                stderr: String::new(),
                duration_ms: 1,
                timed_out: false,
            },
        });
        let out = redact_event(&redactor, ev);
        match out {
            SdkEvent::ValidationCompleted(e) => {
                assert!(!e.report.command.contains(secret), "command leaked");
                assert!(!e.report.stdout.contains(secret), "stdout leaked");
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
