//! Deterministic fake provider used by examples and tests.
//!
//! Available behind the `fake-provider` feature flag (NOT enabled by default).
//! Applications that ship to production should never rely on this provider —
//! it is intentionally simple and does not call any real model.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_agent_core::{
    AssistantContent, ContentBlock, MessageRole, ModelEvent, ModelProvider, ModelRequest,
    ProviderError, TokenUsage, ToolCall,
};
use futures::stream::{self, BoxStream};

/// A scripted fake provider: every call to `stream_turn` pops the next
/// scripted turn off a queue. When the queue is empty it returns a single
/// `MessageStop`. Cloning a `ScriptedFakeProvider` shares the script — the
/// same underlying queue is used.
#[derive(Clone, Default)]
pub struct ScriptedFakeProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
}

impl ScriptedFakeProvider {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
        }
    }

    pub fn from_scripts(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self::new(turns)
    }

    /// Convenience: a single-turn provider that emits a tool call followed
    /// by an assistant text and stop.
    pub fn one_shot_tool_call(tool: &str, args: serde_json::Value) -> Self {
        Self::new(vec![
            vec![
                ModelEvent::ToolCall(ToolCall {
                    id: "fake-1".to_string(),
                    name: tool.to_string(),
                    input: args,
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: Some(TokenUsage::default()),
                },
            ],
            // After the tool result, the model produces a final text and stops.
            vec![
                ModelEvent::TextDelta {
                    text: "done".to_string(),
                },
                ModelEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: Some(TokenUsage::default()),
                },
            ],
        ])
    }

    /// Convenience: a single-turn provider that emits a single assistant
    /// text and stops.
    pub fn text_response(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::new(vec![vec![
            ModelEvent::TextDelta { text },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: Some(TokenUsage::default()),
            },
        ]])
    }
}

#[async_trait]
impl ModelProvider for ScriptedFakeProvider {
    async fn stream_turn(
        &self,
        _request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let mut turns = self.turns.lock().unwrap();
        let events = if turns.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            turns.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

/// A trivial echo fake: it reads the last user message and replies with it
/// verbatim. Useful for "Hello world" examples that don't exercise tool
/// calls.
#[derive(Default, Clone)]
pub struct EchoFakeProvider;

#[async_trait]
impl ModelProvider for EchoFakeProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let last_user = request
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::User))
            .and_then(|m| {
                m.content.iter().find_map(|c| match c {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_else(|| "echo".to_string());
        let response = format!("echo: {last_user}");
        Ok(Box::pin(stream::iter(vec![
            Ok(ModelEvent::TextDelta { text: response }),
            Ok(ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: Some(TokenUsage::default()),
            }),
        ])))
    }
}

/// Helper: drop the deferred `AssistantContent` and `ContentBlock` warnings
/// that arise when nothing in the file actually constructs them but the
/// imports are present.
#[allow(dead_code)]
fn _used(_: AssistantContent) {}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::conversation::Message;
    use futures::StreamExt;

    /// EchoFakeProvider streams back the user's last message.
    #[tokio::test]
    async fn echo_provider_echoes() {
        let p = EchoFakeProvider::default();
        let req = ModelRequest {
            model: "fake".to_string(),
            system: None,
            messages: vec![Message::user_text("hello")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 1024,
        };
        let mut stream = p.stream_turn(req).await.unwrap();
        let mut text = String::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ModelEvent::TextDelta { text: t }) = ev {
                text.push_str(&t);
            }
        }
        assert!(text.contains("hello"));
    }
}
