//! Anthropic Messages API streaming provider for code-agent.
//!
//! Implements the public Messages API as described in Anthropic's developer
//! documentation. The provider sends conversations and tool definitions, then
//! parses Server-Sent Events into normalized `ModelEvent`s.

use std::time::Duration;

use async_trait::async_trait;
use code_agent_core::{
    ContentBlock, MessageRole, ModelEvent, ModelProvider, ModelRequest, ProviderError, TokenUsage,
    ToolCall, ToolResultContent,
};
use eventsource_stream::Eventsource;
use futures::stream::{self, BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE: &str = "https://api.anthropic.com";

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub api_key: String,
    pub base_url: String,
    /// Maximum time to establish the TCP/TLS connection. Does not bound the
    /// duration of streaming responses.
    pub connect_timeout: Duration,
    /// Maximum time the client will wait between successive bytes from the
    /// server. Streaming SSE keeps emitting events (or pings) so a stall
    /// longer than this means the server has gone quiet.
    pub read_timeout: Duration,
    pub max_retries: u32,
}

impl AnthropicConfig {
    pub fn from_env() -> Result<Self, ProviderError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| ProviderError::Auth("ANTHROPIC_API_KEY is not set".into()))?;
        let base_url =
            std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE.to_string());
        Ok(Self {
            api_key,
            base_url,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(120),
            max_retries: 2,
        })
    }
}

pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: AnthropicConfig) -> Result<Self, ProviderError> {
        // Deliberately omit `.timeout()`: a single overall request timeout
        // will abort an in-progress streaming response, even when the server
        // is happily emitting events. We bound connection setup and inter-
        // chunk read time instead, which is enough to detect a stalled link
        // without truncating long completions.
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .map_err(|e| ProviderError::Other(format!("client build: {e}")))?;
        Ok(Self { config, client })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let body = build_body(&request);
        let url = format!("{}/v1/messages", self.config.base_url);
        let api_key = self.config.api_key.clone();
        let client = self.client.clone();
        let max_retries = self.config.max_retries;

        let mut attempt: u32 = 0;
        let response = loop {
            let req = client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .json(&body);
            let res = req.send().await;
            match res {
                Ok(r) if r.status().is_success() => break r,
                Ok(r) => {
                    let status = r.status();
                    if !is_retryable(status) || attempt >= max_retries {
                        let text = r.text().await.unwrap_or_else(|_| "<no body>".into());
                        return Err(ProviderError::Http(format!("status {status}: {text}")));
                    }
                }
                Err(e) => {
                    if !e.is_timeout() && !e.is_connect() || attempt >= max_retries {
                        return Err(ProviderError::Http(format!("{e}")));
                    }
                }
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(250 * (1 << attempt))).await;
        };

        let stream = response.bytes_stream().eventsource().map(|res| match res {
            Ok(ev) => Ok(ev),
            Err(e) => Err(ProviderError::Stream(format!("sse: {e}"))),
        });

        let parsed = parse_sse(stream);
        Ok(Box::pin(parsed))
    }
}

fn is_retryable(status: reqwest::StatusCode) -> bool {
    status.as_u16() == 408 || status.as_u16() == 429 || (500..=599).contains(&status.as_u16())
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
}

#[derive(Serialize)]
struct ApiMessage<'a> {
    role: &'a str,
    content: Vec<ApiBlock<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ApiBlock<'a> {
    Text {
        #[serde(rename = "type")]
        ty: &'a str,
        text: &'a str,
    },
    ToolUse {
        #[serde(rename = "type")]
        ty: &'a str,
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    ToolResult {
        #[serde(rename = "type")]
        ty: &'a str,
        tool_use_id: &'a str,
        content: Vec<ApiToolResultContent<'a>>,
        is_error: bool,
    },
}

#[derive(Serialize)]
struct ApiToolResultContent<'a> {
    #[serde(rename = "type")]
    ty: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct RequestBody<'a> {
    model: &'a str,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<ApiMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
}

fn build_body(req: &ModelRequest) -> serde_json::Value {
    // Anthropic's `messages` array only accepts `user` and `assistant` roles
    // (system content goes in the top-level `system` field on
    // `ModelRequest`). Drop any stray system-role messages here rather than
    // silently downgrading them to `user`, which would leak instruction-
    // boundary content into the user channel.
    let messages: Vec<ApiMessage> = req
        .messages
        .iter()
        .filter_map(|m| {
            let role = match m.role {
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::System => return None,
            };
            Some(ApiMessage {
                role,
                content: m.content.iter().map(content_to_api).collect(),
            })
        })
        .collect();

    let tools: Vec<AnthropicTool> = req
        .tools
        .iter()
        .map(|t| AnthropicTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
        })
        .collect();

    let body = RequestBody {
        model: &req.model,
        max_tokens: req.max_tokens,
        stream: true,
        system: req.system.as_deref(),
        messages,
        tools,
    };
    serde_json::to_value(&body).unwrap_or(serde_json::Value::Null)
}

fn content_to_api(b: &ContentBlock) -> ApiBlock<'_> {
    match b {
        ContentBlock::Text { text } => ApiBlock::Text { ty: "text", text },
        ContentBlock::ToolUse(c) => ApiBlock::ToolUse {
            ty: "tool_use",
            id: &c.id,
            name: &c.name,
            input: &c.input,
        },
        ContentBlock::ToolResult(r) => {
            let content: Vec<ApiToolResultContent> = r
                .content
                .iter()
                .map(|c| match c {
                    ToolResultContent::Text { text } => ApiToolResultContent { ty: "text", text },
                })
                .collect();
            ApiBlock::ToolResult {
                ty: "tool_result",
                tool_use_id: &r.tool_use_id,
                content,
                is_error: r.is_error,
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseEvent {
    MessageStart {
        message: SseMessage,
    },
    ContentBlockStart {
        index: u32,
        content_block: SseContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: SseDelta,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: SseMessageDelta,
        #[serde(default)]
        usage: Option<SseUsage>,
    },
    MessageStop,
    Ping,
    Error {
        error: SseError,
    },
}

#[derive(Debug, Deserialize)]
struct SseMessage {
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseContentBlock {
    Text {
        #[allow(dead_code)]
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SseDelta {
    TextDelta { text: String },
    InputJsonDelta { partial_json: String },
}

#[derive(Debug, Deserialize)]
struct SseMessageDelta {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct SseError {
    #[serde(default)]
    message: String,
}

/// Parse SSE events into normalized ModelEvents. Buffers tool_use blocks until
/// they are complete (i.e. until ContentBlockStop), so consumers see fully
/// formed `ToolCall` events rather than partial JSON deltas.
fn parse_sse<S>(stream: S) -> impl futures::Stream<Item = Result<ModelEvent, ProviderError>>
where
    S: futures::Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static,
{
    use std::collections::HashMap;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::ReceiverStream;

    let (tx, rx) = mpsc::channel::<Result<ModelEvent, ProviderError>>(64);

    tokio::spawn(async move {
        struct PendingTool {
            id: String,
            name: String,
            input_initial: Value,
            partial_json: String,
        }

        let mut pending: HashMap<u32, PendingTool> = HashMap::new();
        let mut usage_accum = TokenUsage::default();
        let mut started = false;

        tokio::pin!(stream);
        while let Some(item) = stream.next().await {
            let ev = match item {
                Ok(e) => e,
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    return;
                }
            };
            if ev.data.is_empty() {
                continue;
            }
            let parsed: SseEvent = match serde_json::from_str(&ev.data) {
                Ok(p) => p,
                Err(_) => {
                    // Skip lines we cannot parse; future SDK additions are
                    // tolerated rather than failing the stream.
                    continue;
                }
            };
            match parsed {
                SseEvent::MessageStart { message } => {
                    if !started {
                        started = true;
                        let _ = tx.send(Ok(ModelEvent::MessageStart)).await;
                    }
                    if let Some(u) = message.usage {
                        usage_accum.input_tokens += u.input_tokens;
                        usage_accum.output_tokens += u.output_tokens;
                        merge_cache(&mut usage_accum, &u);
                    }
                }
                SseEvent::ContentBlockStart {
                    index,
                    content_block,
                } => match content_block {
                    SseContentBlock::Text { .. } => {}
                    SseContentBlock::ToolUse { id, name, input } => {
                        pending.insert(
                            index,
                            PendingTool {
                                id,
                                name,
                                input_initial: input,
                                partial_json: String::new(),
                            },
                        );
                    }
                },
                SseEvent::ContentBlockDelta { index, delta } => match delta {
                    SseDelta::TextDelta { text } => {
                        let _ = tx.send(Ok(ModelEvent::TextDelta { text })).await;
                    }
                    SseDelta::InputJsonDelta { partial_json } => {
                        if let Some(p) = pending.get_mut(&index) {
                            p.partial_json.push_str(&partial_json);
                        }
                    }
                },
                SseEvent::ContentBlockStop { index } => {
                    if let Some(p) = pending.remove(&index) {
                        // Reassemble the streamed input. If the model emitted
                        // syntactically invalid JSON, surface that as a
                        // structured envelope so the tool's own input parser
                        // returns a clear `InvalidInput` error back to the
                        // model — instead of silently dropping the partial
                        // payload and dispatching the tool with `{}`.
                        let input: Value = if !p.partial_json.is_empty() {
                            match serde_json::from_str::<Value>(&p.partial_json) {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::warn!(
                                        tool = %p.name,
                                        error = %e,
                                        "tool_use input was not valid JSON; reporting to caller"
                                    );
                                    serde_json::json!({
                                        "_invalid_tool_input": {
                                            "parse_error": e.to_string(),
                                            "partial": p.partial_json,
                                        }
                                    })
                                }
                            }
                        } else {
                            p.input_initial
                        };
                        let call = ToolCall {
                            id: p.id,
                            name: p.name,
                            input,
                        };
                        let _ = tx.send(Ok(ModelEvent::ToolCall(call))).await;
                    }
                }
                SseEvent::MessageDelta { delta, usage } => {
                    if let Some(u) = usage {
                        usage_accum.input_tokens += u.input_tokens;
                        usage_accum.output_tokens += u.output_tokens;
                        merge_cache(&mut usage_accum, &u);
                    }
                    let _ = tx
                        .send(Ok(ModelEvent::MessageStop {
                            stop_reason: delta.stop_reason,
                            usage: Some(usage_accum.clone()),
                        }))
                        .await;
                }
                SseEvent::MessageStop => {
                    // We may have already sent a MessageStop on MessageDelta. The
                    // agent loop tolerates a second one (the receiver simply
                    // updates totals), but to keep events tidy we don't emit it
                    // again here.
                }
                SseEvent::Ping => {}
                SseEvent::Error { error } => {
                    let _ = tx.send(Err(ProviderError::Stream(error.message))).await;
                    return;
                }
            }
        }
    });

    ReceiverStream::new(rx)
}

fn merge_cache(total: &mut TokenUsage, u: &SseUsage) {
    if let Some(v) = u.cache_creation_input_tokens {
        *total.cache_creation_input_tokens.get_or_insert(0) += v;
    }
    if let Some(v) = u.cache_read_input_tokens {
        *total.cache_read_input_tokens.get_or_insert(0) += v;
    }
}

/// Build a provider stream from a fixed sequence of pre-built ModelEvents.
/// Useful for tests when constructing an end-to-end pipeline without a live API.
pub fn fixture_stream(
    events: Vec<ModelEvent>,
) -> BoxStream<'static, Result<ModelEvent, ProviderError>> {
    Box::pin(stream::iter(events.into_iter().map(Ok)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Request body serialization preserves model, system prompt, tools, and message roles.
    fn request_body_serializes() {
        let req = ModelRequest {
            model: "claude-sonnet-4-6".into(),
            system: Some("be helpful".into()),
            messages: vec![code_agent_core::Message::user_text("hi")],
            tools: vec![code_agent_core::provider::ToolSchema {
                name: "Read".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_tokens: 1024,
        };
        let body = build_body(&req);
        assert_eq!(body["model"], "claude-sonnet-4-6");
        assert_eq!(body["system"], "be helpful");
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    // Stray system-role messages are filtered out of the body rather than
    // silently downgraded to user messages.
    fn build_body_drops_system_role_messages() {
        use code_agent_core::{ContentBlock, Message, MessageRole};
        let req = ModelRequest {
            model: "claude-sonnet-4-6".into(),
            system: None,
            messages: vec![
                Message {
                    role: MessageRole::System,
                    content: vec![ContentBlock::Text {
                        text: "hidden".into(),
                    }],
                },
                Message::user_text("hello"),
            ],
            tools: vec![],
            max_tokens: 256,
        };
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hello");
    }
}
