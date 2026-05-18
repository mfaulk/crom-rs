//! OpenAI Chat Completions provider for code-agent.
//!
//! This is a thin adapter that maps `code_agent_core::ModelRequest` ->
//! `chat.completions` request body, performs the call (or invokes a
//! pluggable transport for tests), and normalizes the response into
//! `ModelEvent`s. The provider accepts a [`ResolvedCredential`] at call
//! time so credential storage stays out of the hot path.

use std::sync::Arc;

use async_trait::async_trait;
use code_agent_auth::{AuthError, AuthErrorKind, ProviderId, ResolvedCredential, SecretString};
use code_agent_core::{
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

async fn backoff(attempt: u32) {
    // 250ms * 2^attempt, capped to 30s. Matches the Anthropic provider's
    // pattern. Test builds skip the sleep to keep retry tests fast.
    #[cfg(test)]
    let _ = attempt;
    #[cfg(not(test))]
    {
        use std::time::Duration;
        let ms = 250u64.saturating_mul(1u64 << attempt.min(7)).min(30_000);
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
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
            let input: Value = serde_json::from_str(&tc.function.arguments)
                .unwrap_or_else(|_| Value::String(tc.function.arguments.clone()));
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
// keeps the public surface aligned with code-agent-core's Message types,
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
    use code_agent_core::provider::ToolSchema;
    use code_agent_core::Message;
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
            source: code_agent_auth::CredentialSource::TestFake,
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
        use code_agent_core::{
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
    #[tokio::test]
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
    #[tokio::test]
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
    #[tokio::test]
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
        use code_agent_core::{
            ContentBlock, ConvToolResult, Message, MessageRole, ToolResultContent,
        };
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
}
