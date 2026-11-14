#![cfg(feature = "http")]

use std::time::Duration;

use crom_auth::{CredentialSource, ProviderId, ResolvedCredential, SecretString};
use crom_core::{Message, ModelEvent, ModelProvider, ModelRequest, ProviderError};
use crom_provider_gemini::{GeminiConfig, GeminiProvider, GEMINI_API_KEY_HEADER};
use futures::StreamExt;
use wiremock::matchers::{header, method, path, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OK_BODY: &str = r#"{"candidates":[{"content":{"parts":[{"text":"hello"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1}}"#;

fn cred() -> ResolvedCredential {
    ResolvedCredential::ApiKey {
        provider: ProviderId::Gemini,
        source: CredentialSource::TestFake,
        key: SecretString::new("g-test".into()),
    }
}

fn cfg(server: &MockServer, request_timeout: Duration) -> GeminiConfig {
    GeminiConfig {
        base_url: server.uri(),
        request_timeout,
        ..GeminiConfig::default()
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "gemini-1.5-pro".into(),
        system: None,
        messages: vec![Message::user_text("hi")],
        tools: vec![],
        deferred_tools: vec![],
        tool_search_config: None,
        max_tokens: 64,
    }
}

async fn run(provider: &GeminiProvider) -> Result<Vec<ModelEvent>, ProviderError> {
    let stream = provider.stream_turn(request()).await?;
    Ok(stream.map(|e| e.unwrap()).collect().await)
}

const GENERATE_PATH: &str = "/v1beta/models/gemini-1.5-pro:generateContent";

// A 200 response is parsed into events; the key travels in the x-goog-api-key header,
// never as a `?key=` query parameter.
#[tokio::test]
async fn success_sends_key_in_header_not_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GENERATE_PATH))
        .and(header(GEMINI_API_KEY_HEADER, "g-test"))
        .and(header("content-type", "application/json"))
        .and(query_param_is_missing("key"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::from_config(cfg(&server, Duration::from_secs(5)), cred()).unwrap();
    let events = run(&provider).await.expect("success");
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello");
    assert!(matches!(
        events.last(),
        Some(ModelEvent::MessageStop { .. })
    ));
}

// A 429 surfaces immediately as an Http error carrying the status (Gemini does not retry).
#[tokio::test]
async fn rate_limit_surfaces_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("quota"))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::from_config(cfg(&server, Duration::from_secs(5)), cred()).unwrap();
    let err = run(&provider).await.expect_err("429");
    assert!(
        matches!(&err, ProviderError::Http(m) if m.contains("status 429") && m.contains("quota")),
        "{err:?}"
    );
}

// A 5xx surfaces immediately as an Http error carrying the status (Gemini does not retry).
#[tokio::test]
async fn server_error_surfaces_without_retry() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::from_config(cfg(&server, Duration::from_secs(5)), cred()).unwrap();
    let err = run(&provider).await.expect_err("5xx");
    assert!(
        matches!(&err, ProviderError::Http(m) if m.contains("status 500")),
        "{err:?}"
    );
}

// A response slower than the request timeout fails with an Http error from the transport.
#[tokio::test]
async fn request_timeout_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(OK_BODY)
                .set_delay(Duration::from_secs(5)),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::from_config(cfg(&server, Duration::from_millis(200)), cred()).unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(
        matches!(&err, ProviderError::Http(m) if m.starts_with("gemini post:")),
        "{err:?}"
    );
}

// list_models issues GET /v1beta/models with the key header and returns model names.
#[tokio::test]
async fn list_models_uses_key_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1beta/models"))
        .and(header(GEMINI_API_KEY_HEADER, "g-test"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"models":[{"name":"models/gemini-1.5-pro"}]}"#),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        GeminiProvider::from_config(cfg(&server, Duration::from_secs(5)), cred()).unwrap();
    assert_eq!(
        provider.list_models().await.unwrap(),
        vec!["models/gemini-1.5-pro"]
    );
}

// The production defaults keep the 30s connect / 120s request budgets.
#[test]
fn default_config_preserves_timeouts() {
    let cfg = GeminiConfig::default();
    assert_eq!(cfg.connect_timeout, Duration::from_secs(30));
    assert_eq!(cfg.request_timeout, Duration::from_secs(120));
}
