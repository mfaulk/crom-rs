use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crom_core::conversation::{ContentBlock, MessageRole};
use crom_core::{
    is_output_truncated, query, AgentEvent, AgentOptions, AgentResult, Message, ModelEvent,
    ModelProvider, ModelRequest, PermissionCallback, PermissionDecision, PermissionKind,
    PermissionRequest, ProviderError, ResultSubtype, Tool, ToolCall, ToolContext, ToolError,
    ToolRegistry, ToolResult, MAX_OUTPUT_CONTINUATIONS,
};
use futures::stream::{self, BoxStream, StreamExt};
use serde_json::json;

/// Replays scripted turns and records every request's messages.
#[derive(Clone, Default)]
struct Recorder {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl Recorder {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl ModelProvider for Recorder {
    async fn stream_turn(
        &self,
        req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        self.requests.lock().unwrap().push(req.messages.clone());
        let mut turns = self.turns.lock().unwrap();
        let events = if turns.is_empty() {
            final_turn("done")
        } else {
            turns.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn input_schema(&self) -> serde_json::Value {
        json!({"type": "object"})
    }
    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }
    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text(input.to_string()))
    }
}

struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

fn final_turn(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta { text: text.into() },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]
}

fn truncated_turn(text: &str, stop: &str, tool: bool) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    if !text.is_empty() {
        events.push(ModelEvent::TextDelta { text: text.into() });
    }
    if tool {
        events.push(ModelEvent::ToolCall(ToolCall {
            id: "cut".into(),
            name: "Echo".into(),
            input: json!({"partial": true}),
        }));
    }
    events.push(ModelEvent::MessageStop {
        stop_reason: Some(stop.into()),
        usage: None,
    });
    events
}

async fn run(provider: Recorder) -> Vec<AgentEvent> {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(EchoTool));
    let opts = AgentOptions {
        max_turns: Some(10),
        allowed_tools: vec!["Echo".into()],
        permission_callback: Some(Arc::new(AlwaysAllow)),
        ..AgentOptions::default()
    };
    let mut s = query("go", opts, Arc::new(provider), tools).await.unwrap();
    let mut events = Vec::new();
    while let Some(e) = s.next().await {
        events.push(e);
    }
    events
}

fn final_result(events: &[AgentEvent]) -> &AgentResult {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            AgentEvent::Result(r) => Some(r),
            _ => None,
        })
        .expect("no result event")
}

fn tool_starts(events: &[AgentEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, AgentEvent::ToolCallStarted(_)))
        .count()
}

fn last_user_text(messages: &[Message]) -> String {
    let m = messages.last().expect("messages");
    assert!(matches!(m.role, MessageRole::User));
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// Anthropic `max_tokens`, OpenAI/DeepSeek `length`, and Gemini `MAX_TOKENS` count as truncation; normal stops do not.
#[test]
fn truncation_stop_reasons_are_recognized() {
    for s in ["max_tokens", "length", "MAX_TOKENS"] {
        assert!(is_output_truncated(s), "{s}");
    }
    for s in ["end_turn", "stop", "tool_use", "tool_calls", "STOP"] {
        assert!(!is_output_truncated(s), "{s}");
    }
}

// A truncated text-only turn is continued with a notice instead of being taken as the final answer.
#[tokio::test]
async fn truncated_text_turn_is_continued() {
    let provider = Recorder::new(vec![
        truncated_turn("partial answ", "max_tokens", false),
        final_turn("complete answer"),
    ]);
    let events = run(provider.clone()).await;
    let result = final_result(&events);
    assert_eq!(result.subtype, ResultSubtype::Success);
    assert_eq!(result.result.as_deref(), Some("complete answer"));
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let notice = last_user_text(&requests[1]);
    assert!(notice.contains("output token limit"), "{notice}");
    // The truncated text is kept as the assistant turn before the notice.
    let assistant = &requests[1][requests[1].len() - 2];
    assert!(matches!(assistant.role, MessageRole::Assistant));
}

// Tool calls from a truncated turn are discarded, not executed, and the notice says so.
#[tokio::test]
async fn truncated_tool_call_is_not_executed() {
    let provider = Recorder::new(vec![truncated_turn("", "length", true), final_turn("ok")]);
    let events = run(provider.clone()).await;
    assert_eq!(tool_starts(&events), 0);
    assert_eq!(final_result(&events).subtype, ResultSubtype::Success);
    let requests = provider.requests();
    let notice = last_user_text(&requests[1]);
    assert!(notice.contains("1 incomplete tool call"), "{notice}");
    // No dangling tool_use block is sent back to the provider.
    assert!(requests[1].iter().all(|m| m
        .content
        .iter()
        .all(|b| !matches!(b, ContentBlock::ToolUse(_)))));
}

// Repeated truncation beyond the continuation budget ends the run as ErrorOutputTruncated.
#[tokio::test]
async fn repeated_truncation_fails_with_classified_reason() {
    let turns = (0..=MAX_OUTPUT_CONTINUATIONS)
        .map(|_| truncated_turn("more", "MAX_TOKENS", false))
        .collect();
    let provider = Recorder::new(turns);
    let events = run(provider.clone()).await;
    let result = final_result(&events);
    assert_eq!(result.subtype, ResultSubtype::ErrorOutputTruncated);
    assert!(result
        .result
        .as_deref()
        .unwrap_or_default()
        .contains("MAX_TOKENS"));
    assert_eq!(
        provider.requests().len(),
        MAX_OUTPUT_CONTINUATIONS as usize + 1
    );
}

// A normal turn between truncations resets the consecutive-truncation budget.
#[tokio::test]
async fn continuation_budget_resets_after_normal_turn() {
    let mut turns = Vec::new();
    for _ in 0..MAX_OUTPUT_CONTINUATIONS {
        turns.push(truncated_turn("x", "max_tokens", false));
    }
    turns.push(vec![
        ModelEvent::ToolCall(ToolCall {
            id: "t1".into(),
            name: "Echo".into(),
            input: json!({"v": 1}),
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".into()),
            usage: None,
        },
    ]);
    for _ in 0..MAX_OUTPUT_CONTINUATIONS {
        turns.push(truncated_turn("y", "max_tokens", false));
    }
    turns.push(final_turn("finished"));
    let events = run(Recorder::new(turns)).await;
    assert_eq!(tool_starts(&events), 1);
    assert_eq!(final_result(&events).subtype, ResultSubtype::Success);
}
