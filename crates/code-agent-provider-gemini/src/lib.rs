//! Google AI Studio (Gemini) `generateContent` provider for code-agent.
//!
//! Maps `code_agent_core::ModelRequest` -> `models/{model}:generateContent`
//! payload, performs the call (or invokes a pluggable transport for
//! tests), and normalizes the response into `ModelEvent`s.

use std::sync::Arc;

use async_trait::async_trait;
use code_agent_auth::{ProviderId, ResolvedCredential, SecretString};
use code_agent_core::{
    ContentBlock, MessageRole, ModelEvent, ModelProvider, ModelRequest, ProviderError, TokenUsage,
    ToolCall, ToolResultContent,
};
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

#[async_trait]
pub trait GeminiTransport: Send + Sync {
    async fn post_generate(
        &self,
        url: &str,
        body: &str,
    ) -> Result<TransportResponse, ProviderError>;

    async fn list_models(&self, url: &str) -> Result<TransportResponse, ProviderError>;
}

#[derive(Debug, Clone)]
pub struct TransportResponse {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct GeminiConfig {
    pub base_url: String,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE.into(),
        }
    }
}

pub struct GeminiProvider {
    cfg: GeminiConfig,
    transport: Arc<dyn GeminiTransport>,
    credential: Arc<dyn Fn() -> ResolvedCredential + Send + Sync>,
}

impl GeminiProvider {
    pub fn new(
        cfg: GeminiConfig,
        transport: Arc<dyn GeminiTransport>,
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
        let url = format!("{}/v1beta/models?key={}", self.cfg.base_url, key.expose());
        let resp = self.transport.list_models(&url).await?;
        if resp.status >= 200 && resp.status < 300 {
            let parsed: ModelsList = serde_json::from_str(&resp.body)
                .map_err(|e| ProviderError::InvalidResponse(format!("models: {e}")))?;
            Ok(parsed.models.into_iter().map(|m| m.name).collect())
        } else {
            Err(ProviderError::Http(format!(
                "models: status {}",
                resp.status
            )))
        }
    }
}

#[async_trait]
impl ModelProvider for GeminiProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let cred = (self.credential)();
        let key = require_api_key(&cred)?;
        let url = format!(
            "{}/v1beta/models/{}:generateContent?key={}",
            self.cfg.base_url,
            request.model,
            key.expose()
        );
        let body = serde_json::to_string(&build_body(&request))
            .map_err(|e| ProviderError::Other(format!("encode body: {e}")))?;
        let resp = self.transport.post_generate(&url, &body).await?;
        if resp.status < 200 || resp.status >= 300 {
            return Err(ProviderError::Http(format!(
                "gemini: status {}: {}",
                resp.status, resp.body
            )));
        }
        let parsed: GenerateResponse = serde_json::from_str(&resp.body).map_err(|e| {
            ProviderError::InvalidResponse(format!("gemini generateContent response: {e}"))
        })?;
        let events = generate_response_to_events(parsed);
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn require_api_key(cred: &ResolvedCredential) -> Result<SecretString, ProviderError> {
    match cred {
        ResolvedCredential::ApiKey { key, .. } => Ok(key.clone()),
        ResolvedCredential::OAuthBearer { access_token, .. } => Ok(access_token.clone()),
        _ => Err(ProviderError::Auth(
            "Gemini provider requires an API key".into(),
        )),
    }
}

#[derive(Serialize)]
struct GenerateRequest<'a> {
    contents: Vec<GeminiContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiTool<'a>>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    role: &'a str,
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiPart<'a> {
    Text {
        text: &'a str,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall<'a>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Serialize)]
struct GeminiFunctionCall<'a> {
    name: &'a str,
    args: &'a Value,
}

#[derive(Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: Value,
}

/// Synthetic prefix used in [`generate_response_to_events`] to make the
/// `ToolCall.id` unique when the upstream API does not provide one.
/// The wire form sent back to Gemini must use the original function
/// name without the prefix.
const GEMINI_TOOL_CALL_ID_PREFIX: &str = "gemini-";

fn strip_gemini_id_prefix(id: &str) -> String {
    id.strip_prefix(GEMINI_TOOL_CALL_ID_PREFIX)
        .unwrap_or(id)
        .to_string()
}

#[derive(Serialize)]
struct GeminiTool<'a> {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunction<'a>>,
}

#[derive(Serialize)]
struct GeminiFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Serialize)]
struct GenerationConfig {
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

fn build_body(req: &ModelRequest) -> Value {
    let mut contents: Vec<GeminiContent> = Vec::new();
    for m in &req.messages {
        let role = match m.role {
            MessageRole::User => "user",
            MessageRole::Assistant => "model",
            MessageRole::System => continue,
        };
        let mut parts: Vec<GeminiPart> = Vec::new();
        for c in &m.content {
            match c {
                ContentBlock::Text { text } => parts.push(GeminiPart::Text { text }),
                ContentBlock::ToolUse(c) => parts.push(GeminiPart::FunctionCall {
                    function_call: GeminiFunctionCall {
                        name: &c.name,
                        args: &c.input,
                    },
                }),
                ContentBlock::ToolResult(r) => {
                    let combined = r
                        .content
                        .iter()
                        .map(|c| match c {
                            ToolResultContent::Text { text } => text.clone(),
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    // Gemini requires the `name` field on a function
                    // response to match the original declared function
                    // name — not the synthesized `gemini-<name>` id we
                    // assigned in `generate_response_to_events`. Strip
                    // the prefix when present.
                    let name = strip_gemini_id_prefix(&r.tool_use_id);
                    parts.push(GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponse {
                            name,
                            response: serde_json::json!({"output": combined, "isError": r.is_error}),
                        },
                    });
                }
            }
        }
        contents.push(GeminiContent { role, parts });
    }

    let system_instruction = req.system.as_deref().map(|sys| GeminiContent {
        role: "system",
        parts: vec![GeminiPart::Text { text: sys }],
    });

    let function_declarations: Vec<GeminiFunction> = req
        .tools
        .iter()
        .map(|t| GeminiFunction {
            name: &t.name,
            description: &t.description,
            parameters: &t.input_schema,
        })
        .collect();

    let tools = if function_declarations.is_empty() {
        Vec::new()
    } else {
        vec![GeminiTool {
            function_declarations,
        }]
    };

    let body = GenerateRequest {
        contents,
        system_instruction,
        tools,
        generation_config: GenerationConfig {
            max_output_tokens: req.max_tokens,
        },
    };
    serde_json::to_value(&body).unwrap_or(Value::Null)
}

#[derive(Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage: Option<UsageMetadata>,
}

#[derive(Deserialize)]
struct GeminiCandidate {
    content: GeminiCandidateContent,
    #[serde(default, rename = "finishReason")]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct GeminiCandidateContent {
    #[serde(default)]
    parts: Vec<GeminiCandidatePart>,
}

#[derive(Deserialize)]
struct GeminiCandidatePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiCandidateFunctionCall>,
}

#[derive(Deserialize)]
struct GeminiCandidateFunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

#[derive(Deserialize, Default)]
struct UsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u64,
}

fn generate_response_to_events(resp: GenerateResponse) -> Vec<ModelEvent> {
    let mut events: Vec<ModelEvent> = vec![ModelEvent::MessageStart];
    let mut stop_reason = None;
    if let Some(candidate) = resp.candidates.into_iter().next() {
        stop_reason = candidate.finish_reason;
        for part in candidate.content.parts {
            if let Some(text) = part.text {
                if !text.is_empty() {
                    events.push(ModelEvent::TextDelta { text });
                }
            }
            if let Some(fc) = part.function_call {
                events.push(ModelEvent::ToolCall(ToolCall {
                    id: format!("{}{}", GEMINI_TOOL_CALL_ID_PREFIX, fc.name),
                    name: fc.name,
                    input: fc.args,
                }));
            }
        }
    }
    let usage = resp.usage.map(|u| TokenUsage {
        input_tokens: u.prompt_token_count,
        output_tokens: u.candidates_token_count,
        cache_creation_input_tokens: None,
        cache_read_input_tokens: None,
    });
    events.push(ModelEvent::MessageStop { stop_reason, usage });
    events
}

#[derive(Deserialize)]
struct ModelsList {
    #[serde(default)]
    models: Vec<ModelsItem>,
}

#[derive(Deserialize)]
struct ModelsItem {
    name: String,
}

#[allow(dead_code)]
fn _provider_id_used() -> Option<ProviderId> {
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
        gen_response: Option<TransportResponse>,
        models_response: Option<TransportResponse>,
        captured: std::sync::Mutex<Option<String>>,
    }

    #[async_trait]
    impl GeminiTransport for MockTransport {
        async fn post_generate(
            &self,
            _url: &str,
            body: &str,
        ) -> Result<TransportResponse, ProviderError> {
            *self.captured.lock().unwrap() = Some(body.to_string());
            Ok(self.gen_response.clone().unwrap_or(TransportResponse {
                status: 200,
                body: r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}"#.into(),
            }))
        }

        async fn list_models(&self, _url: &str) -> Result<TransportResponse, ProviderError> {
            Ok(self.models_response.clone().unwrap_or(TransportResponse {
                status: 200,
                body: r#"{"models":[{"name":"models/gemini-1.5-pro"},{"name":"models/gemini-1.5-flash"}]}"#.into(),
            }))
        }
    }

    fn cred(key: &str) -> ResolvedCredential {
        ResolvedCredential::ApiKey {
            provider: ProviderId::Gemini,
            source: code_agent_auth::CredentialSource::TestFake,
            key: SecretString::new(key.into()),
        }
    }

    #[tokio::test]
    // Streaming a generateContent call yields MessageStart, TextDelta, MessageStop.
    async fn streams_text_delta_and_message_stop() {
        let transport = Arc::new(MockTransport::default());
        let provider = GeminiProvider::new(
            GeminiConfig::default(),
            transport.clone(),
            Arc::new(|| cred("AI-1")),
        );
        let req = ModelRequest {
            model: "gemini-1.5-pro".into(),
            system: Some("be helpful".into()),
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolSchema {
                name: "Read".into(),
                description: "read".into(),
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
            .any(|e| matches!(e, ModelEvent::TextDelta { text } if text == "hi")));
        assert!(got
            .iter()
            .any(|e| matches!(e, ModelEvent::MessageStop { .. })));
        let body = transport.captured.lock().unwrap().clone().unwrap();
        assert!(body.contains("\"system_instruction\""));
        assert!(body.contains("be helpful"));
    }

    #[tokio::test]
    // list_models lists model names from the standard /v1beta/models response.
    async fn list_models_parses_standard_shape() {
        let transport = Arc::new(MockTransport::default());
        let provider = GeminiProvider::new(
            GeminiConfig::default(),
            transport,
            Arc::new(|| cred("AI-1")),
        );
        let names = provider.list_models().await.unwrap();
        assert_eq!(
            names,
            vec![
                "models/gemini-1.5-pro".to_string(),
                "models/gemini-1.5-flash".to_string()
            ]
        );
    }

    #[tokio::test]
    // Missing credential surfaces ProviderError::Auth without hitting the transport.
    async fn missing_credential_returns_auth_error() {
        let transport = Arc::new(MockTransport::default());
        let provider = GeminiProvider::new(
            GeminiConfig::default(),
            transport,
            Arc::new(|| ResolvedCredential::None),
        );
        let req = ModelRequest {
            model: "gemini-1.5-pro".into(),
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
    // Function-call response is turned into a ModelEvent::ToolCall with parsed args.
    async fn function_call_is_emitted_as_tool_call() {
        let body = r#"{
            "candidates":[{
                "content":{"parts":[
                    {"functionCall":{"name":"Read","args":{"path":"foo"}}}
                ]},
                "finishReason":"TOOL_CALL"
            }]
        }"#;
        let transport = Arc::new(MockTransport {
            gen_response: Some(TransportResponse {
                status: 200,
                body: body.into(),
            }),
            ..Default::default()
        });
        let provider = GeminiProvider::new(
            GeminiConfig::default(),
            transport,
            Arc::new(|| cred("AI-1")),
        );
        let req = ModelRequest {
            model: "gemini-1.5-pro".into(),
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
        // The synthesized id MUST carry the prefix so subsequent turns
        // can round-trip back to the original function name.
        assert!(tool_calls[0].id.starts_with(GEMINI_TOOL_CALL_ID_PREFIX));
    }

    #[test]
    // strip_gemini_id_prefix peels the synthetic prefix when present
    // and returns the input verbatim otherwise.
    fn strip_gemini_id_prefix_round_trips() {
        assert_eq!(strip_gemini_id_prefix("gemini-Read"), "Read");
        assert_eq!(strip_gemini_id_prefix("Read"), "Read");
        assert_eq!(strip_gemini_id_prefix(""), "");
    }

    #[test]
    // build_body emits the original function name in the FunctionResponse,
    // not the gemini-prefixed tool_use id we set in the prior turn's event.
    // Regression test for the second-opinion review finding that Gemini
    // would reject `name: "gemini-Read"`.
    fn function_response_strips_gemini_prefix() {
        use code_agent_core::{
            ContentBlock, ConvToolResult, Message, MessageRole, ToolResultContent,
        };
        let req = ModelRequest {
            model: "gemini-1.5-pro".into(),
            system: None,
            messages: vec![Message {
                role: MessageRole::User,
                content: vec![ContentBlock::ToolResult(ConvToolResult {
                    tool_use_id: "gemini-Read".into(),
                    content: vec![ToolResultContent::Text { text: "ok".into() }],
                    is_error: false,
                })],
            }],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 8,
        };
        let body = build_body(&req);
        let s = serde_json::to_string(&body).unwrap();
        assert!(s.contains("\"name\":\"Read\""));
        assert!(!s.contains("\"name\":\"gemini-Read\""));
    }
}
