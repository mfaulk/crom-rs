//! Anthropic Messages API streaming provider for code-agent.
//!
//! Implements the public Messages API as described in Anthropic's developer
//! documentation. The provider sends conversations and tool definitions, then
//! parses Server-Sent Events into normalized `ModelEvent`s.

use std::time::Duration;

use async_trait::async_trait;
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
}
