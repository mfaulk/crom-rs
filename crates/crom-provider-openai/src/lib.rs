//! OpenAI Chat Completions provider for crom.
//!
//! This is a thin adapter that maps `crom_core::ModelRequest` ->
//! `chat.completions` request body, performs the call (or invokes a
//! pluggable transport for tests), and normalizes the response into
//! `ModelEvent`s. The provider accepts a [`ResolvedCredential`] at call
//! time so credential storage stays out of the hot path.

use std::sync::Arc;

use async_trait::async_trait;
use crom_auth::{AuthError, AuthErrorKind, ProviderId, ResolvedCredential, SecretString};
use crom_core::{
    AssistantContent, ContentBlock, MessageRole, ModelEvent, ModelProvider, ModelRequest,
    ProviderError, TokenUsage, ToolCall, ToolResultContent,
};
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_BASE: &str = "https://api.openai.com";

/// Minimal HTTP transport: separate from the auth crate's transport so
/// model providers can use streaming response bodies later. Tests use a
/// simple mock that returns a complete response body.
#[async_trait]
pub trait OpenAiTransport: Send + Sync {
    async fn post_chat(
        &self,
        url: &str,
        api_key: &SecretString,
        body: &str,
    ) -> Result<TransportResponse, ProviderError>;

    async fn list_models(
        &self,
        url: &str,
        api_key: &SecretString,
    ) -> Result<TransportResponse, ProviderError>;
}

#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    /// Maximum number of retry *attempts* on 408 / 429 / 5xx responses,
    /// matching the Anthropic provider. Total request attempts = 1 +
    /// max_retries. Defaults to 3.
    pub max_retries: u32,
}

impl Default for OpenAiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE.into(),
            max_retries: 3,
        }
    }
}

pub struct OpenAiProvider {
    cfg: OpenAiConfig,
    transport: Arc<dyn OpenAiTransport>,
    credential: Arc<dyn Fn() -> ResolvedCredential + Send + Sync>,
}

impl OpenAiProvider {
    /// Build a provider that re-resolves the credential each turn.
    pub fn new(
        cfg: OpenAiConfig,
        transport: Arc<dyn OpenAiTransport>,
        credential: Arc<dyn Fn() -> ResolvedCredential + Send + Sync>,
    ) -> Self {
        Self {
            cfg,
            transport,
            credential,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let cred = (self.credential)();
        let key = require_api_key(&cred)?;
        let url = format!("{}/v1/models", self.cfg.base_url);
        let resp = self.transport.list_models(&url, &key).await?;
        if resp.status >= 200 && resp.status < 300 {
            let parsed: ModelsList = serde_json::from_str(&resp.body)
                .map_err(|e| ProviderError::InvalidResponse(format!("models: {e}")))?;
            Ok(parsed.data.into_iter().map(|m| m.id).collect())
        } else {
            Err(ProviderError::Http(format!(
                "models: status {}",
                resp.status
            )))
        }
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let cred = (self.credential)();
        let key = require_api_key(&cred)?;
        let url = format!("{}/v1/chat/completions", self.cfg.base_url);
        let body = serde_json::to_string(&build_body(&request))
            .map_err(|e| ProviderError::Other(format!("encode body: {e}")))?;
        let max_retries = self.cfg.max_retries;

        // Retry on 408 / 429 / 5xx with exponential backoff, mirroring the
        // Anthropic provider. Without this a single transient rate-limit
        // kills the turn even though the request would succeed on retry.
        //
        // Scope: we retry status-shaped failures only. Network-shaped
        // failures (connect timeout, body-read error) come back through
        // the trait as `ProviderError::Http` without retryable structure,
        // so they propagate immediately — matching pre-fix behavior. A
        // follow-up could distinguish them by extending the transport
        // error type or moving retry into the reqwest backend, where
        // `reqwest::Error::{is_timeout,is_connect}` is still available.
        let mut attempt: u32 = 0;
        let resp = loop {
            let r = self.transport.post_chat(&url, &key, &body).await?;
            if (200..300).contains(&r.status) {
                break r;
            }
            if !is_retryable_status(r.status) || attempt >= max_retries {
                return Err(ProviderError::Http(format!(
                    "openai: status {}: {}",
                    r.status, r.body
                )));
            }
            attempt += 1;
            backoff(attempt).await;
        };

        let parsed: ChatResponse = serde_json::from_str(&resp.body)
            .map_err(|e| ProviderError::InvalidResponse(format!("openai chat response: {e}")))?;
        let events = chat_response_to_events(parsed);
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

/// 250ms * 2^attempt, capped at 30s. Pure: split out from `backoff` so
/// tests can advance virtual time deterministically with
/// `tokio::time::pause`, matching the Anthropic provider's curve.
fn retry_delay(attempt: u32) -> std::time::Duration {
    let ms = 250u64.saturating_mul(1u64 << attempt.min(7)).min(30_000);
    std::time::Duration::from_millis(ms)
}

async fn backoff(attempt: u32) {
    tokio::time::sleep(retry_delay(attempt)).await;
}

fn require_api_key(cred: &ResolvedCredential) -> Result<SecretString, ProviderError> {
    match cred {
        ResolvedCredential::ApiKey { key, .. } => Ok(key.clone()),
        ResolvedCredential::OAuthBearer { access_token, .. } => Ok(access_token.clone()),
        _ => Err(ProviderError::Auth(
            "OpenAI provider requires an API key or bearer token".into(),
        )),
    }
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool<'a>>,
    max_tokens: u32,
    stream: bool,
}

/// One top-level message in the OpenAI Chat Completions schema.
///
/// OpenAI's shape is *not* the Anthropic-style nested-content array:
///
/// - `system` / `user` messages carry their text in `content` (string).
/// - `assistant` messages carry text in `content` and any tool
///   invocations in a sibling `tool_calls` array.
/// - Tool results are top-level messages with `role: "tool"` and a
///   `tool_call_id` referencing the call they answer.
#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCallOut>,
}

#[derive(Serialize)]
struct ChatToolCallOut {
    id: String,
    #[serde(rename = "type")]
    ty: &'static str,
    function: ChatToolCallFunctionOut,
}

#[derive(Serialize)]
struct ChatToolCallFunctionOut {
    name: String,
    /// OpenAI requires `arguments` to be a JSON-encoded *string*, not an
    /// object — even when the value is structured.
    arguments: String,
}

#[derive(Serialize)]
struct ChatTool<'a> {
    #[serde(rename = "type")]
    ty: &'static str,
    function: ChatFunction<'a>,
}

#[derive(Serialize)]
struct ChatFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

fn build_body(req: &ModelRequest) -> Value {
    let mut messages: Vec<ChatMessage> = Vec::new();
    if let Some(sys) = req.system.as_deref() {
        messages.push(ChatMessage {
            role: "system",
            content: Some(sys.to_string()),
            tool_call_id: None,
            tool_calls: Vec::new(),
        });
    }
    for m in &req.messages {
        let role: &'static str = match m.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::System => continue,
        };
        // Aggregate text + tool_use within a single role-message; tool
        // results are *flattened* into their own top-level `role: "tool"`
        // messages because OpenAI rejects them inside an `assistant`
        // message's content array.
        let mut text_parts: Vec<String> = Vec::new();
        let mut tool_calls: Vec<ChatToolCallOut> = Vec::new();
        let mut tool_result_messages: Vec<ChatMessage> = Vec::new();
        for c in &m.content {
            match c {
                ContentBlock::Text { text } => text_parts.push(text.clone()),
                ContentBlock::ToolUse(c) => tool_calls.push(ChatToolCallOut {
                    id: c.id.clone(),
                    ty: "function",
                    function: ChatToolCallFunctionOut {
                        name: c.name.clone(),
                        arguments: serde_json::to_string(&c.input).unwrap_or_else(|_| "{}".into()),
                    },
                }),
                ContentBlock::ToolResult(r) => {
                    let mut combined = r
                        .content
                        .iter()
                        .map(|c| match c {
                            ToolResultContent::Text { text } => text.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if r.is_error && !combined.starts_with("ERROR:") {
                        // OpenAI's `tool` role does not have a structured
                        // `is_error` flag — surface it inside the content
                        // string so the model still notices.
                        combined = format!("ERROR: {combined}");
                    }
                    tool_result_messages.push(ChatMessage {
                        role: "tool",
                        content: Some(combined),
                        tool_call_id: Some(r.tool_use_id.clone()),
                        tool_calls: Vec::new(),
                    });
                }
            }
        }
        // Only emit the parent message when it carries something.
        if !text_parts.is_empty() || !tool_calls.is_empty() {
            let combined_text = if text_parts.is_empty() {
                None
            } else {
                Some(text_parts.join(""))
            };
            messages.push(ChatMessage {
                role,
                content: combined_text,
                tool_call_id: None,
                tool_calls,
            });
        }
        // Tool results follow the assistant turn that produced them.
        messages.extend(tool_result_messages);
    }
    let tools: Vec<ChatTool> = req
        .tools
        .iter()
        .map(|t| ChatTool {
            ty: "function",
            function: ChatFunction {
                name: &t.name,
                description: &t.description,
                parameters: &t.input_schema,
            },
        })
        .collect();
    let body = ChatRequest {
        model: &req.model,
        messages,
        tools,
        max_tokens: req.max_tokens,
        stream: false,
    };
    serde_json::to_value(&body).unwrap_or(Value::Null)
}

#[derive(Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCall>,
}

#[derive(Deserialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    ty: String,
    function: ChatToolCallFunction,
}

#[derive(Deserialize)]
struct ChatToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

fn chat_response_to_events(resp: ChatResponse) -> Vec<ModelEvent> {
    let mut events: Vec<ModelEvent> = vec![ModelEvent::MessageStart];
    let mut stop_reason = None;
    if let Some(choice) = resp.choices.into_iter().next() {
        stop_reason = choice.finish_reason;
        if let Some(text) = choice.message.content {
            if !text.is_empty() {
                events.push(ModelEvent::TextDelta { text });
            }
        }
        for tc in choice.message.tool_calls {
            // OpenAI returns `arguments: ""` for no-arg tool calls; `from_str("")`
            // errors and would fall back to `Value::String("")`, which breaks
            // downstream object deserializers. Treat empty/whitespace-only
            // arguments as an empty JSON object instead.
            let input: Value = if tc.function.arguments.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| Value::String(tc.function.arguments.clone()))
            };
            events.push(ModelEvent::ToolCall(ToolCall {
                id: tc.id,
                name: tc.function.name,
                input,
            }));
        }
    }
    let usage = resp.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    events.push(ModelEvent::MessageStop { stop_reason, usage });
    events
}

#[derive(Deserialize)]
struct ModelsList {
    data: Vec<ModelsItem>,
}

#[derive(Deserialize)]
struct ModelsItem {
    id: String,
}

// Re-exports to avoid the `unused_imports` warning. `AssistantContent`
// keeps the public surface aligned with crom-core's Message types,
// even though we only ever emit `TextDelta` + `ToolCall` events.
#[allow(dead_code)]
fn _api_types_used() -> Option<AssistantContent> {
    None
}

#[allow(dead_code)]
fn _auth_types_used() -> Option<(ProviderId, AuthError, AuthErrorKind)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::provider::ToolSchema;
    use crom_core::Message;
    use futures::StreamExt;

    #[derive(Default)]
    struct MockTransport {
        chat_response: Option<TransportResponse>,
        /// FIFO queue of responses for retry tests. Each call to
        /// `post_chat` consumes one entry; if empty, falls back to
        /// `chat_response` / the default success body.
        chat_queue: std::sync::Mutex<Vec<TransportResponse>>,
        models_response: Option<TransportResponse>,
        captured_body: std::sync::Mutex<Option<String>>,
        captured_key: std::sync::Mutex<Option<String>>,
        chat_call_count: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl OpenAiTransport for MockTransport {
        async fn post_chat(
            &self,
            _url: &str,
            api_key: &SecretString,
            body: &str,
        ) -> Result<TransportResponse, ProviderError> {
            *self.captured_body.lock().unwrap() = Some(body.to_string());
            *self.captured_key.lock().unwrap() = Some(api_key.expose().to_string());
            self.chat_call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut queue = self.chat_queue.lock().unwrap();
            if !queue.is_empty() {
                return Ok(queue.remove(0));
            }
            drop(queue);
            Ok(self.chat_response.clone().unwrap_or(TransportResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#.into(),
            }))
        }

        async fn list_models(
            &self,
            _url: &str,
            _api_key: &SecretString,
        ) -> Result<TransportResponse, ProviderError> {
            Ok(self.models_response.clone().unwrap_or(TransportResponse {
                status: 200,
                body: r#"{"data":[{"id":"gpt-4.1"},{"id":"gpt-4o"}]}"#.into(),
            }))
        }
    }

    fn cred(key: &str) -> ResolvedCredential {
        ResolvedCredential::ApiKey {
            provider: ProviderId::OpenAi,
            source: crom_auth::CredentialSource::TestFake,
            key: SecretString::new(key.into()),
        }
    }

    #[tokio::test]
    // Streaming a chat-completion produces a MessageStart -> TextDelta -> MessageStop sequence
    // and forwards the API key to the transport.
    async fn streams_text_delta_and_message_stop() {
        let transport = Arc::new(MockTransport::default());
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport.clone(),
            Arc::new(|| cred("sk-1")),
        );
        let req = ModelRequest {
            model: "gpt-4.1".into(),
            system: Some("be helpful".into()),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSchema {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 64,
        };
        let mut stream = provider.stream_turn(req).await.unwrap();
        let mut got = Vec::new();
        while let Some(ev) = stream.next().await {
            got.push(ev.unwrap());
        }
        assert!(matches!(got.first(), Some(ModelEvent::MessageStart)));
        assert!(got
            .iter()
            .any(|e| matches!(e, ModelEvent::TextDelta { text } if text == "hello")));
        assert!(got
            .iter()
            .any(|e| matches!(e, ModelEvent::MessageStop { .. })));
        assert_eq!(
            transport.captured_key.lock().unwrap().as_deref(),
            Some("sk-1")
        );
        let body = transport.captured_body.lock().unwrap().clone().unwrap();
        assert!(body.contains("\"model\":\"gpt-4.1\""));
        assert!(body.contains("\"role\":\"system\""));
    }

    #[tokio::test]
    // list_models parses the standard /v1/models response shape.
    async fn list_models_parses_standard_shape() {
        let transport = Arc::new(MockTransport::default());
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport,
            Arc::new(|| cred("sk-1")),
        );
        let models = provider.list_models().await.unwrap();
        assert_eq!(models, vec!["gpt-4.1".to_string(), "gpt-4o".to_string()]);
    }

    #[tokio::test]
    // Missing credential surfaces ProviderError::Auth without hitting the transport.
    async fn missing_credential_returns_auth_error() {
        let transport = Arc::new(MockTransport::default());
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport,
            Arc::new(|| ResolvedCredential::None),
        );
        let req = ModelRequest {
            model: "gpt-4.1".into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        };
        let result = provider.stream_turn(req).await;
        match result {
            Err(ProviderError::Auth(_)) => {}
            Err(other) => panic!("expected Auth error, got: {other:?}"),
            Ok(_) => panic!("expected Err(Auth), got Ok"),
        }
    }

    #[tokio::test]
    // Tool-call output is mapped to ModelEvent::ToolCall with parsed JSON arguments.
    async fn tool_call_arguments_parsed_into_modelevent() {
        let chat_body = r#"{
            "choices":[{
                "message":{
                    "content":null,
                    "tool_calls":[{
                        "id":"call_1",
                        "type":"function",
                        "function":{"name":"Read","arguments":"{\"path\":\"foo\"}"}
                    }]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
        let transport = Arc::new(MockTransport {
            chat_response: Some(TransportResponse {
                status: 200,
                body: chat_body.into(),
            }),
            ..Default::default()
        });
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport,
            Arc::new(|| cred("sk-1")),
        );
        let req = ModelRequest {
            model: "gpt-4.1".into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        };
        let mut stream = provider.stream_turn(req).await.unwrap();
        let mut tool_calls = Vec::new();
        while let Some(ev) = stream.next().await {
            if let Ok(ModelEvent::ToolCall(tc)) = ev {
                tool_calls.push(tc);
            }
        }
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "Read");
        assert_eq!(tool_calls[0].input["path"], "foo");
    }

    #[test]
    // Assistant turn carrying a ToolUse + a follow-up ToolResult is
    // serialized into:
    //   {role:"assistant", tool_calls:[{id, type:"function", function:{name,arguments}}]}
    //   {role:"tool", tool_call_id:<id>, content:"<result text>"}
    // — not the Anthropic-style nested content array. Regression test
    // for the second-opinion review finding that OpenAI rejects
    // tool_use/tool_result blocks inside the content array.
    fn build_body_emits_chat_completions_tool_shape() {
        use crom_core::{
            ContentBlock, ConvToolResult, Message, MessageRole, ToolCall, ToolResultContent,
        };
        let req = ModelRequest {
            model: "gpt-4.1".into(),
            system: Some("be helpful".into()),
            messages: vec![
                Message::user_text("read foo"),
                Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::ToolUse(ToolCall {
                        id: "call_42".into(),
                        name: "Read".into(),
                        input: serde_json::json!({"path": "foo"}),
                    })],
                },
                Message {
                    role: MessageRole::User,
                    content: vec![ContentBlock::ToolResult(ConvToolResult {
                        tool_use_id: "call_42".into(),
                        content: vec![ToolResultContent::Text {
                            text: "hello world".into(),
                        }],
                        is_error: false,
                    })],
                },
            ],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        };
        let body = build_body(&req);
        let messages = body["messages"].as_array().unwrap();
        // 1) system    2) user     3) assistant w/ tool_calls    4) tool result
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "read foo");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call_42");
        assert_eq!(messages[2]["tool_calls"][0]["type"], "function");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "Read");
        // arguments is a JSON string, not an object.
        assert_eq!(
            messages[2]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"foo\"}"
        );
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_42");
        assert_eq!(messages[3]["content"], "hello world");
        // No leftover Anthropic-style content blocks.
        let s = serde_json::to_string(&body).unwrap();
        assert!(!s.contains("\"type\":\"tool_use\""));
        assert!(!s.contains("\"type\":\"tool_result\""));
    }

    /// The `retry_delay` curve is exactly `250ms * 2^attempt`, capped
    /// at 30s. This locks in the schedule independently of any test
    /// that exercises `stream_turn` under paused time.
    #[test]
    fn retry_delay_curve_matches_anthropic() {
        use std::time::Duration;
        assert_eq!(retry_delay(0), Duration::from_millis(250));
        assert_eq!(retry_delay(1), Duration::from_millis(500));
        assert_eq!(retry_delay(2), Duration::from_millis(1_000));
        assert_eq!(retry_delay(3), Duration::from_millis(2_000));
        // Cap kicks in at ~30s.
        assert_eq!(retry_delay(20), Duration::from_millis(30_000));
    }

    fn req(model: &str) -> ModelRequest {
        ModelRequest {
            model: model.into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        }
    }

    /// A transient 429 is retried and the eventual 200 succeeds. Matches
    /// the Anthropic provider's behavior; the OpenAI variant previously
    /// converted the 429 directly into a fatal turn error.
    #[tokio::test(start_paused = true)]
    async fn stream_turn_retries_on_429_then_succeeds() {
        let transport = Arc::new(MockTransport::default());
        transport.chat_queue.lock().unwrap().extend(vec![
            TransportResponse {
                status: 429,
                body: r#"{"error":{"message":"rate limited"}}"#.into(),
            },
            TransportResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#.into(),
            },
        ]);
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport.clone(),
            Arc::new(|| cred("sk-1")),
        );
        let mut s = provider.stream_turn(req("gpt-4.1")).await.unwrap();
        let mut got = Vec::new();
        while let Some(ev) = s.next().await {
            got.push(ev.unwrap());
        }
        assert!(got
            .iter()
            .any(|e| matches!(e, ModelEvent::TextDelta { text } if text == "ok")));
        assert_eq!(
            transport
                .chat_call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    /// 5xx server errors are retryable too.
    #[tokio::test(start_paused = true)]
    async fn stream_turn_retries_on_503_then_succeeds() {
        let transport = Arc::new(MockTransport::default());
        transport.chat_queue.lock().unwrap().extend(vec![
            TransportResponse {
                status: 503,
                body: "service unavailable".into(),
            },
            TransportResponse {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#.into(),
            },
        ]);
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport.clone(),
            Arc::new(|| cred("sk-1")),
        );
        let _ = provider.stream_turn(req("gpt-4.1")).await.unwrap();
        assert_eq!(
            transport
                .chat_call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2
        );
    }

    /// 4xx non-retryable errors abort immediately (single call).
    #[tokio::test]
    async fn stream_turn_does_not_retry_on_400() {
        let transport = Arc::new(MockTransport::default());
        transport
            .chat_queue
            .lock()
            .unwrap()
            .push(TransportResponse {
                status: 400,
                body: r#"{"error":"bad request"}"#.into(),
            });
        let provider = OpenAiProvider::new(
            OpenAiConfig::default(),
            transport.clone(),
            Arc::new(|| cred("sk-1")),
        );
        let result = provider.stream_turn(req("gpt-4.1")).await;
        assert!(matches!(result, Err(ProviderError::Http(_))));
        assert_eq!(
            transport
                .chat_call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    /// After `max_retries` retryable failures, the last error is surfaced.
    #[tokio::test(start_paused = true)]
    async fn stream_turn_gives_up_after_max_retries() {
        let transport = Arc::new(MockTransport::default());
        for _ in 0..6 {
            transport
                .chat_queue
                .lock()
                .unwrap()
                .push(TransportResponse {
                    status: 429,
                    body: "rate limited".into(),
                });
        }
        let provider = OpenAiProvider::new(
            OpenAiConfig {
                max_retries: 2,
                ..OpenAiConfig::default()
            },
            transport.clone(),
            Arc::new(|| cred("sk-1")),
        );
        let result = provider.stream_turn(req("gpt-4.1")).await;
        assert!(matches!(result, Err(ProviderError::Http(_))));
        // 1 initial attempt + 2 retries = 3 total.
        assert_eq!(
            transport
                .chat_call_count
                .load(std::sync::atomic::Ordering::SeqCst),
            3
        );
    }

    #[test]
    // is_error tool-result is prefixed with "ERROR:" since OpenAI's
    // `tool` role has no structured error flag.
    fn build_body_marks_error_tool_results() {
        use crom_core::{ContentBlock, ConvToolResult, Message, MessageRole, ToolResultContent};
        let req = ModelRequest {
            model: "gpt-4.1".into(),
            system: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult(ConvToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ToolResultContent::Text {
                        text: "permission denied".into(),
                    }],
                    is_error: true,
                })],
            }],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        };
        let body = build_body(&req);
        let s = body["messages"][0]["content"].as_str().unwrap().to_string();
        assert!(s.starts_with("ERROR:"));
        assert!(s.contains("permission denied"));
    }

    #[test]
    // OpenAI sends `arguments: ""` for no-arg tool calls; the converted
    // ToolCall input must be an empty JSON object, not an empty string,
    // so downstream object deserializers don't choke (#44).
    fn empty_tool_call_arguments_become_empty_object() {
        let resp: ChatResponse = serde_json::from_value(serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "now", "arguments": "" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }))
        .unwrap();
        let events = chat_response_to_events(resp);
        let tool_call = events
            .iter()
            .find_map(|e| match e {
                ModelEvent::ToolCall(tc) => Some(tc),
                _ => None,
            })
            .expect("a ToolCall event");
        assert_eq!(tool_call.input, Value::Object(Default::default()));
        assert!(!matches!(tool_call.input, Value::String(_)));
    }

    // ------------------------------------------------------------------
    // Snapshot tests (insta): lock the FULL serialized Chat Completions
    // request body wire shape.
    // ------------------------------------------------------------------

    /// Representative request exercising every `build_body` branch:
    /// system prompt, a user prompt, an assistant turn carrying a
    /// tool_use, a tool result (flattened into a `role:"tool"` message),
    /// an error tool result (ERROR: prefix), and declared tools.
    fn representative_request() -> ModelRequest {
        use crom_core::{ContentBlock, ConvToolResult, Message, MessageRole};
        ModelRequest {
            model: "gpt-4.1".into(),
            system: Some("You are a careful coding agent.".into()),
            messages: vec![
                Message::user_text("read foo and bar"),
                Message {
                    role: MessageRole::Assistant,
                    content: vec![
                        ContentBlock::Text {
                            text: "On it.".into(),
                        },
                        ContentBlock::ToolUse(ToolCall {
                            id: "call_42".into(),
                            name: "Read".into(),
                            input: serde_json::json!({"path": "foo"}),
                        }),
                    ],
                },
                Message {
                    role: MessageRole::User,
                    content: vec![
                        ContentBlock::ToolResult(ConvToolResult {
                            tool_use_id: "call_42".into(),
                            content: vec![ToolResultContent::Text {
                                text: "hello world".into(),
                            }],
                            is_error: false,
                        }),
                        ContentBlock::ToolResult(ConvToolResult {
                            tool_use_id: "call_43".into(),
                            content: vec![ToolResultContent::Text {
                                text: "permission denied".into(),
                            }],
                            is_error: true,
                        }),
                    ],
                },
            ],
            tools: vec![ToolSchema {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"],
                }),
            }],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 512,
        }
    }

    #[test]
    // Snapshot the entire serialized OpenAI Chat Completions request body.
    fn snapshot_build_body() {
        let body = build_body(&representative_request());
        insta::assert_json_snapshot!(body);
    }

    // ------------------------------------------------------------------
    // Property tests (proptest): `chat_response_to_events` must never
    // panic and must produce a coherent event stream on arbitrary input.
    // ------------------------------------------------------------------

    use proptest::prelude::*;

    /// Strategy producing arbitrary-but-plausible Chat Completions JSON:
    /// missing fields, multiple choices, null content, varied tool-call
    /// argument strings (valid JSON, empty, garbage), and optional usage.
    fn chat_response_json_strategy() -> impl Strategy<Value = Value> {
        let tool_call = (
            "[a-z_0-9]{1,8}",
            "[A-Za-z_]{1,8}",
            prop_oneof![
                Just("{\"path\":\"x\"}".to_string()),
                Just("".to_string()),
                Just("   ".to_string()),
                Just("not json".to_string()),
                Just("[1,2,3]".to_string()),
            ],
        )
            .prop_map(|(id, name, args)| {
                serde_json::json!({
                    "id": id,
                    "type": "function",
                    "function": {"name": name, "arguments": args},
                })
            });
        let message = (
            prop_oneof![
                Just(serde_json::Value::Null),
                ".{0,16}".prop_map(serde_json::Value::String),
            ],
            proptest::collection::vec(tool_call, 0..4),
        )
            .prop_map(|(content, tool_calls)| {
                serde_json::json!({"content": content, "tool_calls": tool_calls})
            });
        let choice = (
            message,
            prop_oneof![
                Just(serde_json::Value::Null),
                Just(serde_json::json!("stop")),
                Just(serde_json::json!("tool_calls")),
            ],
        )
            .prop_map(|(message, finish_reason)| {
                serde_json::json!({"message": message, "finish_reason": finish_reason})
            });
        (
            proptest::collection::vec(choice, 0..3),
            prop_oneof![
                Just(serde_json::Value::Null),
                (0u64..1000, 0u64..1000).prop_map(|(p, c)| {
                    serde_json::json!({"prompt_tokens": p, "completion_tokens": c})
                }),
            ],
        )
            .prop_map(|(choices, usage)| serde_json::json!({"choices": choices, "usage": usage}))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        // chat_response_to_events must NEVER panic on arbitrary
        // (well-typed-but-unusual) response bodies and must produce a
        // coherent event stream. Invariants:
        //  - the first event is always MessageStart and the last is
        //    always MessageStop (well-formed envelope);
        //  - every ToolCall preserves a non-empty id/name and its input
        //    is never a bare empty string (#44: empty args -> {});
        //  - no panic from JSON re-parsing of arbitrary argument strings.
        #[test]
        fn chat_response_to_events_never_panics_and_is_coherent(json in chat_response_json_strategy()) {
            // The deserialize itself must not panic; skip inputs that do
            // not fit the typed schema (the production path returns an
            // error there, which is out of scope for this parser fn).
            let resp: ChatResponse = match serde_json::from_value(json) {
                Ok(r) => r,
                Err(_) => return Ok(()),
            };
            let events = chat_response_to_events(resp);

            let first_is_start = matches!(events.first(), Some(ModelEvent::MessageStart));
            let last_is_stop = matches!(events.last(), Some(ModelEvent::MessageStop { .. }));
            prop_assert!(first_is_start, "first event must be MessageStart");
            prop_assert!(last_is_stop, "last event must be MessageStop");
            for e in &events {
                if let ModelEvent::ToolCall(tc) = e {
                    prop_assert!(!tc.id.is_empty(), "tool call id preserved");
                    prop_assert!(!tc.name.is_empty(), "tool call name preserved");
                    // Empty/whitespace arguments must map to {}, never a
                    // bare String("") that breaks object deserializers.
                    prop_assert!(
                        !matches!(&tc.input, Value::String(s) if s.trim().is_empty()),
                        "empty args must become an object, not an empty string"
                    );
                }
            }
        }
    }
}
