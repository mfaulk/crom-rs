//! Anthropic Messages API streaming provider for code-agent.
//!
//! Implements the public Messages API as described in Anthropic's developer
//! documentation. The provider sends conversations and tool definitions, then
//! parses Server-Sent Events into normalized `ModelEvent`s.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use code_agent_core::{
    ContentBlock, MessageRole, ModelEvent, ModelProvider, ModelRequest, NativeToolSearchMode,
    ProviderError, TokenUsage, ToolCall, ToolResultContent,
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

/// Outcome of a single Messages API POST, normalized so the retry loop in
/// `stream_turn` can decide what to do without depending on `reqwest` types.
///
/// This is the seam that lets tests exercise `stream_turn` (retry policy,
/// error-body capture, SSE plumbing) against canned byte streams, mirroring
/// the `ChatCompletionsClient`/`GenerateContentClient` traits in the sibling
/// OpenAI and Gemini providers. Anthropic differs in that a successful
/// response is a *streaming* SSE body rather than a single JSON blob, so the
/// success variant carries a byte stream instead of a `String`.
pub enum MessagesResponse {
    /// 2xx: the SSE byte stream to feed into `parse_sse`.
    Stream(BoxStream<'static, Result<Bytes, ProviderError>>),
    /// Non-2xx HTTP status with its (already read) response body.
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    /// A network-shaped failure (connect/read timeout, connection error).
    /// `retryable` mirrors `reqwest::Error::{is_timeout, is_connect}`.
    Transport { message: String, retryable: bool },
}

/// Transport seam for the Anthropic Messages API. The production
/// implementation (`ReqwestMessagesClient`) wraps `reqwest`; tests inject a
/// mock that returns canned `MessagesResponse`s.
#[async_trait]
pub trait MessagesClient: Send + Sync {
    async fn post_messages(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<MessagesResponse, ProviderError>;
}

/// Real `reqwest`-backed client used in production.
struct ReqwestMessagesClient {
    client: reqwest::Client,
}

#[async_trait]
impl MessagesClient for ReqwestMessagesClient {
    async fn post_messages(
        &self,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> Result<MessagesResponse, ProviderError> {
        let req = self
            .client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(body);
        match req.send().await {
            Ok(r) if r.status().is_success() => {
                let stream = r
                    .bytes_stream()
                    .map(|res| res.map_err(|e| ProviderError::Stream(format!("body: {e}"))));
                Ok(MessagesResponse::Stream(Box::pin(stream)))
            }
            Ok(r) => {
                let status = r.status();
                let body = r.text().await.unwrap_or_else(|_| "<no body>".into());
                Ok(MessagesResponse::Status { status, body })
            }
            Err(e) => Ok(MessagesResponse::Transport {
                message: format!("{e}"),
                retryable: e.is_timeout() || e.is_connect(),
            }),
        }
    }
}

pub struct AnthropicProvider {
    config: AnthropicConfig,
    client: Arc<dyn MessagesClient>,
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
        Ok(Self {
            config,
            client: Arc::new(ReqwestMessagesClient { client }),
        })
    }

    /// Construct a provider over an arbitrary `MessagesClient`. Used by tests
    /// to drive the retry loop and SSE pipeline against canned byte streams
    /// without a live API.
    #[cfg(test)]
    fn with_client(config: AnthropicConfig, client: Arc<dyn MessagesClient>) -> Self {
        Self { config, client }
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
        let byte_stream = loop {
            match client.post_messages(&url, &api_key, &body).await? {
                MessagesResponse::Stream(s) => break s,
                MessagesResponse::Status { status, body } => {
                    if !is_retryable(status) || attempt >= max_retries {
                        return Err(ProviderError::Http(format!("status {status}: {body}")));
                    }
                }
                MessagesResponse::Transport { message, retryable } => {
                    if !retryable || attempt >= max_retries {
                        return Err(ProviderError::Http(message));
                    }
                }
            }
            attempt += 1;
            tokio::time::sleep(retry_delay(attempt)).await;
        };

        let stream = byte_stream.eventsource().map(|res| match res {
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

/// 250ms * 2^attempt, capped at 30s. Saturating and capped so a high
/// configured `max_retries` cannot overflow the shift, mirroring the
/// OpenAI provider's curve.
fn retry_delay(attempt: u32) -> Duration {
    let ms = 250u64.saturating_mul(1u64 << attempt.min(7)).min(30_000);
    Duration::from_millis(ms)
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a Value,
    #[serde(skip_serializing_if = "is_false")]
    deferred: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_search: Option<serde_json::Value>,
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

    let native_mode = req
        .tool_search_config
        .as_ref()
        .map(|config| config.native_mode)
        .unwrap_or(NativeToolSearchMode::Off);
    let native_deferred_enabled = matches!(
        native_mode,
        NativeToolSearchMode::AnthropicRegex | NativeToolSearchMode::AnthropicBm25
    );

    let mut tools: Vec<AnthropicTool> = req
        .tools
        .iter()
        .map(|t| AnthropicTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
            deferred: false,
        })
        .collect();
    if native_deferred_enabled && !req.tools.is_empty() {
        tools.extend(req.deferred_tools.iter().map(|t| AnthropicTool {
            name: &t.name,
            description: &t.description,
            input_schema: &t.input_schema,
            deferred: true,
        }));
    }
    let tool_search = if native_deferred_enabled {
        Some(serde_json::json!({
            "type": match native_mode {
                NativeToolSearchMode::AnthropicRegex => "regex",
                NativeToolSearchMode::AnthropicBm25 => "bm25",
                _ => "off",
            }
        }))
    } else {
        None
    };

    let body = RequestBody {
        model: &req.model,
        max_tokens: req.max_tokens,
        stream: true,
        system: req.system.as_deref(),
        messages,
        tools,
        tool_search,
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

        // A successful send returns Ok(()); a SendError means the consumer
        // dropped the receiver. Stop draining the upstream SSE stream as
        // soon as that happens — otherwise we keep billing/holding the
        // upstream connection long after the caller cancelled the turn.
        macro_rules! send_or_stop {
            ($msg:expr) => {
                if tx.send($msg).await.is_err() {
                    return;
                }
            };
        }

        let mut pending: HashMap<u32, PendingTool> = HashMap::new();
        let mut usage_accum = TokenUsage::default();
        let mut started = false;

        tokio::pin!(stream);
        loop {
            // Wake on EITHER the next upstream event OR the downstream
            // channel closing. Without this, the task remains blocked in
            // `stream.next().await` while the consumer is gone, and the
            // upstream HTTP connection stays open until the server hangs
            // up. `biased;` makes a ready closed-channel signal win over
            // buffered upstream events.
            let item = tokio::select! {
                biased;
                _ = tx.closed() => return,
                next = stream.next() => match next {
                    Some(x) => x,
                    None => break,
                },
            };
            let ev = match item {
                Ok(e) => e,
                Err(e) => {
                    // Already returning — `let _ = tx.send` is fine here.
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
                        send_or_stop!(Ok(ModelEvent::MessageStart));
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
                        send_or_stop!(Ok(ModelEvent::TextDelta { text }));
                    }
                    SseDelta::InputJsonDelta { partial_json } => {
                        // No channel send here, so `send_or_stop!` cannot
                        // catch a consumer drop while a large tool JSON is
                        // streaming. Probe the channel explicitly so we
                        // don't buffer an unbounded payload after the
                        // caller is gone.
                        if tx.is_closed() {
                            return;
                        }
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
                        send_or_stop!(Ok(ModelEvent::ToolCall(call)));
                    }
                }
                SseEvent::MessageDelta { delta, usage } => {
                    if let Some(u) = usage {
                        usage_accum.input_tokens += u.input_tokens;
                        usage_accum.output_tokens += u.output_tokens;
                        merge_cache(&mut usage_accum, &u);
                    }
                    send_or_stop!(Ok(ModelEvent::MessageStop {
                        stop_reason: delta.stop_reason,
                        usage: Some(usage_accum.clone()),
                    }));
                }
                SseEvent::MessageStop => {
                    // We may have already sent a MessageStop on MessageDelta. The
                    // agent loop tolerates a second one (the receiver simply
                    // updates totals), but to keep events tidy we don't emit it
                    // again here.
                }
                SseEvent::Ping => {}
                SseEvent::Error { error } => {
                    // Already returning — `let _ = tx.send` is fine here.
                    let _ = tx.send(Err(ProviderError::Stream(error.message))).await;
                    return;
                }
            }
        }
        // Stream ended. If we still have pending tool_use blocks
        // (content_block_start observed but no matching content_block_stop),
        // the model's response was truncated mid-stream. Pre-fix, the
        // `pending` map was dropped silently and the agent loop hung
        // waiting for a tool result that never arrived (#170). Surface
        // a structured `ProviderError::Stream` so the caller can retry
        // — preferred over best-effort parsing of `partial_json`
        // because the upstream input may be syntactically incomplete
        // (mid-object, mid-string, mid-escape).
        if !pending.is_empty() {
            let names: Vec<String> = pending.values().map(|p| p.name.clone()).collect();
            let _ = tx
                .send(Err(ProviderError::Stream(format!(
                    "tool_use truncated mid-stream ({} pending: {})",
                    pending.len(),
                    names.join(", ")
                ))))
                .await;
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
    // Backoff grows then saturates at the 30s cap, and a large attempt count
    // does not overflow the `1 << attempt` shift.
    fn retry_delay_caps_and_does_not_overflow() {
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_millis(1000));
        // Cap reached around attempt 7 (250 * 128 = 32_000 -> 30_000).
        assert_eq!(retry_delay(7), Duration::from_millis(30_000));
        // Very high attempt counts saturate rather than overflow/panic.
        assert_eq!(retry_delay(64), Duration::from_millis(30_000));
        assert_eq!(retry_delay(u32::MAX), Duration::from_millis(30_000));
    }

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
            deferred_tools: vec![],
            tool_search_config: None,
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
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 256,
        };
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["text"], "hello");
    }

    /// Once the consumer drops the receiver, the spawned SSE drain task
    /// must stop reading from upstream — otherwise the provider keeps
    /// the connection open and runs up token cost for a cancelled turn.
    /// We prove this by feeding events into an upstream channel and
    /// asserting that, after the downstream receiver is dropped, the
    /// upstream sender starts seeing `SendError` (the spawned task has
    /// dropped its end of the upstream stream).
    #[tokio::test]
    async fn sse_drain_stops_when_consumer_drops_receiver() {
        use futures::StreamExt;
        use tokio::sync::mpsc;
        use tokio::time::{timeout, Duration};
        use tokio_stream::wrappers::UnboundedReceiverStream;

        let (upstream_tx, upstream_rx) =
            mpsc::unbounded_channel::<Result<eventsource_stream::Event, ProviderError>>();
        let upstream: UnboundedReceiverStream<_> = UnboundedReceiverStream::new(upstream_rx);
        let mut downstream = Box::pin(parse_sse(upstream));

        let evt = |s: &str| eventsource_stream::Event {
            event: String::new(),
            data: s.to_string(),
            id: String::new(),
            retry: None,
        };

        // Drive at least one event through so the spawned task is actively
        // pumping the stream.
        upstream_tx
            .send(Ok(evt(
                r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            )))
            .expect("upstream alive before consumer drop");
        let first = timeout(Duration::from_secs(1), downstream.next())
            .await
            .expect("first event arrived")
            .expect("stream not exhausted");
        assert!(first.is_ok());

        // Consumer drops the receiver.
        drop(downstream);

        // Drive content_block_delta events (each triggers send_or_stop!).
        // After at most a few iterations the spawned task must observe
        // the closed downstream channel and return. Once it returns it
        // drops `upstream`, which closes `upstream_rx` — and the next
        // `upstream_tx.send()` then returns `SendError`.
        let mut observed_task_exit = false;
        for _ in 0..32 {
            if upstream_tx
                .send(Ok(evt(
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#,
                )))
                .is_err()
            {
                observed_task_exit = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            observed_task_exit,
            "SSE drain task kept reading after consumer dropped the receiver"
        );
    }

    /// Consumer drop during a long tool_use JSON stream must also stop
    /// the spawned task. `InputJsonDelta` events don't send to the
    /// downstream channel, so they would otherwise keep buffering until
    /// the next event that does (potentially many KB later).
    #[tokio::test]
    async fn sse_drain_stops_during_tool_json_buffering() {
        use futures::StreamExt;
        use tokio::sync::mpsc;
        use tokio::time::{timeout, Duration};
        use tokio_stream::wrappers::UnboundedReceiverStream;

        let (upstream_tx, upstream_rx) =
            mpsc::unbounded_channel::<Result<eventsource_stream::Event, ProviderError>>();
        let upstream: UnboundedReceiverStream<_> = UnboundedReceiverStream::new(upstream_rx);
        let mut downstream = Box::pin(parse_sse(upstream));

        let evt = |s: &str| eventsource_stream::Event {
            event: String::new(),
            data: s.to_string(),
            id: String::new(),
            retry: None,
        };

        // Open a tool_use block so subsequent input_json_delta events
        // route into pending instead of producing channel sends.
        upstream_tx
            .send(Ok(evt(
                r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":1,"output_tokens":0}}}"#,
            )))
            .unwrap();
        upstream_tx
            .send(Ok(evt(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"X","input":{}}}"#,
            )))
            .unwrap();
        let _first = timeout(Duration::from_secs(1), downstream.next())
            .await
            .expect("first event arrived");

        // Consumer drops the receiver mid-tool_use.
        drop(downstream);

        // Without the is_closed() probe in the InputJsonDelta arm, the
        // task would keep accumulating partial_json silently for as long
        // as the upstream produced deltas. Drive several input_json_delta
        // events and assert the task observed the drop.
        let mut observed_task_exit = false;
        for _ in 0..32 {
            if upstream_tx
                .send(Ok(evt(
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"abc"}}"#,
                )))
                .is_err()
            {
                observed_task_exit = true;
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            observed_task_exit,
            "SSE drain task kept buffering tool_use JSON after consumer dropped the receiver"
        );
    }

    /// The task must wake on consumer drop even when blocked in
    /// `stream.next().await` with no buffered events upstream. Without
    /// the `tokio::select!` on `tx.closed()`, the task would block
    /// indefinitely on the upstream HTTP connection.
    #[tokio::test]
    async fn sse_drain_stops_when_idle_and_consumer_drops() {
        use tokio::sync::mpsc;
        use tokio::time::{timeout, Duration};
        use tokio_stream::wrappers::UnboundedReceiverStream;

        let (upstream_tx, upstream_rx) =
            mpsc::unbounded_channel::<Result<eventsource_stream::Event, ProviderError>>();
        let upstream: UnboundedReceiverStream<_> = UnboundedReceiverStream::new(upstream_rx);
        let downstream = Box::pin(parse_sse(upstream));

        // Don't send any events. Task is blocked in stream.next(). Drop
        // the receiver — the task must wake via tx.closed() and exit,
        // which drops the upstream stream and closes upstream_rx.
        drop(downstream);

        // Wait for the task to release upstream_rx. Without the select!,
        // this would hang forever since nothing is poking the loop.
        let result = timeout(Duration::from_secs(2), async {
            while !upstream_tx.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "SSE drain task did not wake on consumer drop while idle"
        );
    }

    #[test]
    // Explicit Anthropic-native BM25 mode marks deferred tools separately in the request body.
    fn native_bm25_marks_deferred_tools() {
        let req = ModelRequest {
            model: "claude-sonnet-4-6".into(),
            system: None,
            messages: vec![code_agent_core::Message::user_text("hi")],
            tools: vec![code_agent_core::provider::ToolSchema {
                name: "SearchTools".into(),
                description: "search tools".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            deferred_tools: vec![code_agent_core::provider::ToolSchema {
                name: "mcp__github__list_issues".into(),
                description: "List GitHub issues".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            tool_search_config: Some(code_agent_core::ToolSearchProviderConfig {
                native_mode: NativeToolSearchMode::AnthropicBm25,
                local_search_enabled: true,
                strategy: code_agent_core::ToolSearchStrategy::Hybrid,
            }),
            max_tokens: 256,
        };
        let body = build_body(&req);
        assert_eq!(body["tool_search"]["type"], "bm25");
        assert_eq!(body["tools"].as_array().unwrap().len(), 2);
        assert_eq!(body["tools"][1]["deferred"], true);
    }

    // Regression for #170: a truncated SSE stream that started a
    // `tool_use` block but never reached `content_block_stop` must
    // surface a `ProviderError::Stream` so the agent loop can retry.
    // Pre-fix the `pending` map was dropped silently and the
    // downstream loop hung waiting for a `ToolCall` event that never
    // arrived.
    #[tokio::test]
    async fn truncated_tool_use_stream_emits_error() {
        use eventsource_stream::Event;
        use futures::stream::StreamExt;

        fn ev(data: &str) -> Result<Event, ProviderError> {
            Ok(Event {
                event: String::new(),
                data: data.to_string(),
                id: String::new(),
                retry: None,
            })
        }

        // MessageStart, then ContentBlockStart for a tool_use, then a
        // partial InputJsonDelta, then EOF — no ContentBlockStop, no
        // MessageStop, no error.
        let events = vec![
            ev(
                r#"{"type":"message_start","message":{"id":"m1","model":"x","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            ),
            ev(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
            ),
            ev(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"foo"}}"#,
            ),
        ];
        let stream = futures::stream::iter(events);
        let mut out = parse_sse(stream).boxed();

        let mut saw_error = false;
        let mut saw_tool_call = false;
        while let Some(item) = out.next().await {
            match item {
                Err(ProviderError::Stream(msg)) => {
                    assert!(
                        msg.contains("tool_use truncated"),
                        "unexpected error message: {msg}"
                    );
                    saw_error = true;
                }
                Ok(ModelEvent::ToolCall(_)) => saw_tool_call = true,
                _ => {}
            }
        }
        assert!(
            saw_error,
            "truncated tool_use stream must surface ProviderError::Stream (#170)"
        );
        assert!(
            !saw_tool_call,
            "no ToolCall should be emitted from a truncated stream"
        );
    }

    // A clean stream that reaches `content_block_stop` for the
    // tool_use is unaffected by the fix — no synthetic error.
    #[tokio::test]
    async fn complete_tool_use_stream_does_not_synthesize_error() {
        use eventsource_stream::Event;
        use futures::stream::StreamExt;

        fn ev(data: &str) -> Result<Event, ProviderError> {
            Ok(Event {
                event: String::new(),
                data: data.to_string(),
                id: String::new(),
                retry: None,
            })
        }
        let events = vec![
            ev(
                r#"{"type":"message_start","message":{"id":"m1","model":"x","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            ),
            ev(
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
            ),
            ev(
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"foo\"}"}}"#,
            ),
            ev(r#"{"type":"content_block_stop","index":0}"#),
            ev(
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"input_tokens":0,"output_tokens":4}}"#,
            ),
        ];
        let stream = futures::stream::iter(events);
        let mut out = parse_sse(stream).boxed();
        let mut saw_tool_call = false;
        let mut saw_error = false;
        while let Some(item) = out.next().await {
            match item {
                Ok(ModelEvent::ToolCall(tc)) => {
                    assert_eq!(tc.name, "Read");
                    saw_tool_call = true;
                }
                Err(_) => saw_error = true,
                _ => {}
            }
        }
        assert!(saw_tool_call, "tool call should be emitted on clean stream");
        assert!(
            !saw_error,
            "no synthetic error for a complete stream (#170 regression)"
        );
    }

    // ------------------------------------------------------------------
    // parse_sse fixture tests
    //
    // Drive `parse_sse` directly with canned SSE `data:` frames and assert
    // the produced `ModelEvent`s. Each `&str` is one SSE event's `data`
    // payload (already split into frames), matching what
    // `bytes_stream().eventsource()` yields upstream of `parse_sse`.
    // ------------------------------------------------------------------

    fn sse_event(data: &str) -> Result<eventsource_stream::Event, ProviderError> {
        Ok(eventsource_stream::Event {
            event: String::new(),
            data: data.to_string(),
            id: String::new(),
            retry: None,
        })
    }

    /// Collect every `ModelEvent`/error produced by `parse_sse` over a fixed
    /// sequence of SSE `data:` frames.
    async fn collect_events(frames: &[&str]) -> Vec<Result<ModelEvent, ProviderError>> {
        let events: Vec<_> = frames.iter().map(|f| sse_event(f)).collect();
        let stream = futures::stream::iter(events);
        parse_sse(stream).collect().await
    }

    #[tokio::test]
    // A text-delta-only stream produces MessageStart, the text deltas in
    // order, and a final MessageStop carrying accumulated usage.
    async fn parse_sse_text_delta_only() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":", world"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":5}}"#,
            r#"{"type":"message_stop"}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        assert!(matches!(events[0], ModelEvent::MessageStart));
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["Hello", ", world"]);
        match events.last().unwrap() {
            ModelEvent::MessageStop { stop_reason, usage } => {
                assert_eq!(stop_reason.as_deref(), Some("end_turn"));
                let u = usage.as_ref().unwrap();
                // input from message_start (10) + message_delta (0); output 0 + 5.
                assert_eq!(u.input_tokens, 10);
                assert_eq!(u.output_tokens, 5);
            }
            other => panic!("expected MessageStop, got {other:?}"),
        }
    }

    #[tokio::test]
    // input_json_delta fragments are reassembled into the final ToolCall input
    // only on content_block_stop; no partial ToolCall is emitted mid-stream.
    async fn parse_sse_tool_call_json_reassembly() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":3,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"\"/etc/hosts\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        let calls: Vec<&ToolCall> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "exactly one fully-formed ToolCall");
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].input["path"], "/etc/hosts");
    }

    #[tokio::test]
    // Interleaved text and tool-call deltas across distinct block indices are
    // each reassembled correctly; the ToolCall is emitted at its block stop.
    async fn parse_sse_interleaved_text_and_tool_call() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Let me read it."}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_2","name":"Grep","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"foo\"}"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        // Order: MessageStart, TextDelta, ToolCall, MessageStop.
        assert!(matches!(events[0], ModelEvent::MessageStart));
        assert!(matches!(&events[1], ModelEvent::TextDelta { text } if text == "Let me read it."));
        match &events[2] {
            ModelEvent::ToolCall(c) => {
                assert_eq!(c.name, "Grep");
                assert_eq!(c.input["pattern"], "foo");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
        assert!(matches!(events[3], ModelEvent::MessageStop { .. }));
    }

    #[tokio::test]
    // message_delta usage is merged with message_start usage (including
    // cache tokens via merge_cache) into the MessageStop's TokenUsage.
    async fn parse_sse_usage_and_cache_merge() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":40,"cache_read_input_tokens":10}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":25,"cache_read_input_tokens":5}}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        match events.last().unwrap() {
            ModelEvent::MessageStop { usage, .. } => {
                let u = usage.as_ref().unwrap();
                assert_eq!(u.input_tokens, 100);
                assert_eq!(u.output_tokens, 25);
                assert_eq!(u.cache_creation_input_tokens, Some(40));
                // cache_read merged across both frames: 10 + 5.
                assert_eq!(u.cache_read_input_tokens, Some(15));
            }
            other => panic!("expected MessageStop, got {other:?}"),
        }
    }

    #[tokio::test]
    // An `error` event terminates the stream with ProviderError::Stream
    // carrying the server message, and nothing is emitted afterward.
    async fn parse_sse_error_event_terminates() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            // Anything after the error must not be observed.
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"late"}}"#,
        ])
        .await;

        // MessageStart, then the error; the trailing text_delta is dropped.
        assert!(matches!(out[0], Ok(ModelEvent::MessageStart)));
        match &out[1] {
            Err(ProviderError::Stream(msg)) => assert_eq!(msg, "Overloaded"),
            other => panic!("expected Stream error, got {other:?}"),
        }
        assert_eq!(out.len(), 2, "no events after the error frame");
    }

    #[tokio::test]
    // A stream that opens a tool_use block but ends before content_block_stop
    // surfaces a truncation error (regression-aligned with #170), and no
    // ToolCall is produced.
    async fn parse_sse_truncated_stream() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_9","name":"Bash","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"l"}}"#,
        ])
        .await;

        assert!(
            out.iter().any(|r| matches!(
                r,
                Err(ProviderError::Stream(m)) if m.contains("tool_use truncated")
            )),
            "truncated tool_use must surface a Stream error"
        );
        assert!(
            !out.iter().any(|r| matches!(r, Ok(ModelEvent::ToolCall(_)))),
            "no ToolCall from a truncated stream"
        );
    }

    #[tokio::test]
    // A malformed JSON `data:` frame is skipped (forward-compat tolerance):
    // surrounding well-formed frames are still parsed normally.
    async fn parse_sse_malformed_json_frame_is_skipped() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"a"}}"#,
            r#"{ this is not valid json at all "#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"b"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                ModelEvent::TextDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            texts,
            vec!["a", "b"],
            "malformed frame skipped, rest parsed"
        );
        assert!(matches!(
            events.last().unwrap(),
            ModelEvent::MessageStop { .. }
        ));
    }

    #[tokio::test]
    // Syntactically-invalid reassembled tool_use JSON is surfaced as a
    // structured `_invalid_tool_input` envelope rather than dropped.
    async fn parse_sse_invalid_tool_json_envelope() {
        let out = collect_events(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"Read","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{not json"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ])
        .await;

        let events: Vec<ModelEvent> = out.into_iter().map(|r| r.unwrap()).collect();
        let call = events
            .iter()
            .find_map(|e| match e {
                ModelEvent::ToolCall(c) => Some(c),
                _ => None,
            })
            .expect("a ToolCall is still emitted");
        assert!(
            call.input.get("_invalid_tool_input").is_some(),
            "invalid tool JSON wrapped in _invalid_tool_input envelope, got: {}",
            call.input
        );
    }

    // ------------------------------------------------------------------
    // stream_turn retry tests via the MessagesClient seam
    // ------------------------------------------------------------------

    /// A mock client that returns a queued sequence of `MessagesResponse`s,
    /// recording how many times it was called. Each success frame becomes a
    /// single-chunk SSE byte stream.
    #[derive(Default)]
    struct MockMessagesClient {
        queue: std::sync::Mutex<std::collections::VecDeque<MockOutcome>>,
        calls: std::sync::atomic::AtomicUsize,
        captured_body: std::sync::Mutex<Option<Value>>,
    }

    enum MockOutcome {
        /// 2xx: the SSE body, supplied as a list of `data:` frame payloads.
        Ok(Vec<String>),
        Status {
            status: u16,
            body: String,
        },
        Transport {
            message: String,
            retryable: bool,
        },
    }

    #[async_trait]
    impl MessagesClient for MockMessagesClient {
        async fn post_messages(
            &self,
            _url: &str,
            _api_key: &str,
            body: &Value,
        ) -> Result<MessagesResponse, ProviderError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.captured_body.lock().unwrap() = Some(body.clone());
            let outcome = self
                .queue
                .lock()
                .unwrap()
                .pop_front()
                .expect("mock client called more times than queued outcomes");
            match outcome {
                MockOutcome::Ok(frames) => {
                    // Encode each frame as a proper SSE `data:` line so the
                    // `eventsource()` adapter in stream_turn reconstructs the
                    // events exactly as it would from a real response body.
                    let mut buf = String::new();
                    for f in frames {
                        buf.push_str("data: ");
                        buf.push_str(&f);
                        buf.push_str("\n\n");
                    }
                    let chunk: Result<Bytes, ProviderError> = Ok(Bytes::from(buf));
                    let stream = futures::stream::iter(vec![chunk]);
                    Ok(MessagesResponse::Stream(Box::pin(stream)))
                }
                MockOutcome::Status { status, body } => Ok(MessagesResponse::Status {
                    status: reqwest::StatusCode::from_u16(status).unwrap(),
                    body,
                }),
                MockOutcome::Transport { message, retryable } => {
                    Ok(MessagesResponse::Transport { message, retryable })
                }
            }
        }
    }

    fn test_config(max_retries: u32) -> AnthropicConfig {
        AnthropicConfig {
            api_key: "test-key".into(),
            base_url: DEFAULT_BASE.into(),
            connect_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
            max_retries,
        }
    }

    fn simple_request() -> ModelRequest {
        ModelRequest {
            model: "claude-sonnet-4-6".into(),
            system: None,
            messages: vec![code_agent_core::Message::user_text("hi")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 16,
        }
    }

    fn ok_frames() -> Vec<String> {
        vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1,"output_tokens":0}}}"#
                .to_string(),
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#
                .to_string(),
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":0,"output_tokens":1}}"#
                .to_string(),
        ]
    }

    #[tokio::test(start_paused = true)]
    // A transient 429 is retried and the subsequent 200 streams normally.
    async fn stream_turn_retries_on_429_then_succeeds() {
        let client = Arc::new(MockMessagesClient::default());
        client.queue.lock().unwrap().extend([
            MockOutcome::Status {
                status: 429,
                body: "rate limited".into(),
            },
            MockOutcome::Ok(ok_frames()),
        ]);
        let provider = AnthropicProvider::with_client(test_config(2), client.clone());
        let mut stream = provider.stream_turn(simple_request()).await.unwrap();
        let mut got = Vec::new();
        while let Some(ev) = stream.next().await {
            got.push(ev.unwrap());
        }
        assert!(got
            .iter()
            .any(|e| matches!(e, ModelEvent::TextDelta { text } if text == "ok")));
        assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    // 5xx is retryable; exhausting max_retries surfaces the final status body.
    async fn stream_turn_500_retries_exhausted() {
        let client = Arc::new(MockMessagesClient::default());
        for _ in 0..4 {
            client.queue.lock().unwrap().push_back(MockOutcome::Status {
                status: 500,
                body: "boom".into(),
            });
        }
        let provider = AnthropicProvider::with_client(test_config(2), client.clone());
        let result = provider.stream_turn(simple_request()).await;
        match result {
            Err(ProviderError::Http(msg)) => assert!(msg.contains("500") && msg.contains("boom")),
            Err(other) => panic!("expected Http error, got {other:?}"),
            Ok(_) => panic!("expected Http error, got Ok"),
        }
        // 1 initial + 2 retries = 3 attempts.
        assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    // A non-retryable 4xx (400) aborts immediately with no retry.
    async fn stream_turn_does_not_retry_on_400() {
        let client = Arc::new(MockMessagesClient::default());
        client.queue.lock().unwrap().push_back(MockOutcome::Status {
            status: 400,
            body: "bad request".into(),
        });
        let provider = AnthropicProvider::with_client(test_config(3), client.clone());
        let result = provider.stream_turn(simple_request()).await;
        assert!(matches!(result, Err(ProviderError::Http(_))));
        assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    // A retryable transport error (timeout/connect) is retried; a
    // non-retryable transport error aborts immediately.
    async fn stream_turn_retries_retryable_transport_error() {
        let client = Arc::new(MockMessagesClient::default());
        client.queue.lock().unwrap().extend([
            MockOutcome::Transport {
                message: "connect timeout".into(),
                retryable: true,
            },
            MockOutcome::Ok(ok_frames()),
        ]);
        let provider = AnthropicProvider::with_client(test_config(2), client.clone());
        let mut stream = provider.stream_turn(simple_request()).await.unwrap();
        while let Some(ev) = stream.next().await {
            ev.unwrap();
        }
        assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    // A non-retryable transport error is surfaced immediately (single call).
    async fn stream_turn_non_retryable_transport_error_aborts() {
        let client = Arc::new(MockMessagesClient::default());
        client
            .queue
            .lock()
            .unwrap()
            .push_back(MockOutcome::Transport {
                message: "tls handshake failed".into(),
                retryable: false,
            });
        let provider = AnthropicProvider::with_client(test_config(3), client.clone());
        let result = provider.stream_turn(simple_request()).await;
        match result {
            Err(ProviderError::Http(msg)) => assert!(msg.contains("tls handshake failed")),
            Err(other) => panic!("expected Http error, got {other:?}"),
            Ok(_) => panic!("expected Http error, got Ok"),
        }
        assert_eq!(client.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    // is_retryable classifies 408/429/5xx as retryable and others not.
    fn is_retryable_classifies_statuses() {
        use reqwest::StatusCode;
        assert!(is_retryable(StatusCode::REQUEST_TIMEOUT)); // 408
        assert!(is_retryable(StatusCode::TOO_MANY_REQUESTS)); // 429
        assert!(is_retryable(StatusCode::INTERNAL_SERVER_ERROR)); // 500
        assert!(is_retryable(StatusCode::SERVICE_UNAVAILABLE)); // 503
        assert!(!is_retryable(StatusCode::BAD_REQUEST)); // 400
        assert!(!is_retryable(StatusCode::UNAUTHORIZED)); // 401
        assert!(!is_retryable(StatusCode::NOT_FOUND)); // 404
    }

    #[tokio::test]
    // fixture_stream yields exactly the events it is given, in order.
    async fn fixture_stream_replays_events() {
        let events = vec![
            ModelEvent::MessageStart,
            ModelEvent::TextDelta { text: "hi".into() },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            },
        ];
        let got: Vec<ModelEvent> = fixture_stream(events.clone())
            .map(|r| r.unwrap())
            .collect()
            .await;
        assert_eq!(got.len(), 3);
        assert!(matches!(got[0], ModelEvent::MessageStart));
        assert!(matches!(&got[1], ModelEvent::TextDelta { text } if text == "hi"));
        assert!(matches!(got[2], ModelEvent::MessageStop { .. }));
    }
}
