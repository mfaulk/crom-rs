use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use crate::conversation::{Message, ToolCall};
use crate::errors::ProviderError;
use crate::tool_catalog::ToolSearchProviderConfig;
use crate::usage::TokenUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    #[serde(default)]
    pub deferred_tools: Vec<ToolSchema>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_search_config: Option<ToolSearchProviderConfig>,
    pub max_tokens: u32,
}

/// Streaming events emitted by a `ModelProvider`. These are normalized so that
/// providers do not leak vendor-specific payloads into the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    /// A run started.
    MessageStart,
    /// Streamed text delta from the assistant.
    TextDelta { text: String },
    /// A complete tool call was produced. Providers may buffer partial JSON.
    ToolCall(ToolCall),
    /// The model finished a turn.
    MessageStop {
        stop_reason: Option<String>,
        usage: Option<TokenUsage>,
    },
}

/// Whether a raw provider stop reason means the turn was cut off at the
/// output token limit: Anthropic `max_tokens`, OpenAI-compatible `length`,
/// Gemini `MAX_TOKENS`.
pub fn is_output_truncated(stop_reason: &str) -> bool {
    matches!(stop_reason, "max_tokens" | "length" | "MAX_TOKENS")
}

/// Consecutive truncated turns the agent loop continues before ending the
/// run with `ResultSubtype::ErrorOutputTruncated`.
pub const MAX_OUTPUT_CONTINUATIONS: u32 = 2;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError>;
}
