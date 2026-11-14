#![cfg(feature = "http")]

use std::time::Duration;

use crom_auth::{CredentialSource, ProviderId, ResolvedCredential, SecretString};
use crom_core::{Message, ModelEvent, ModelProvider, ModelRequest, ProviderError};
use crom_provider_openai::{OpenAiConfig, OpenAiProvider};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OK_BODY: &str = r#"{"choices":[{"message":{"content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

fn cred() -> ResolvedCredential {
    ResolvedCredential::ApiKey {
        provider: ProviderId::OpenAi,
        source: CredentialSource::TestFake,
        key: SecretString::new("sk-test".into()),
    }
}

fn cfg(server: &MockServer, max_retries: u32, request_timeout: Duration) -> OpenAiConfig {
    OpenAiConfig {
        base_url: server.uri(),
        max_retries,
        request_timeout,
        ..OpenAiConfig::default()
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "gpt-4.1".into(),
        system: None,
        messages: vec![Message::user_text("hi")],
        tools: vec![],
        deferred_tools: vec![],
        tool_search_config: None,
        max_tokens: 64,
    }
}

async fn run(provider: &OpenAiProvider) -> Result<Vec<ModelEvent>, ProviderError> {
    let stream = provider.stream_turn(request()).await?;
    Ok(stream.map(|e| e.unwrap()).collect().await)
}

fn text_of(events: &[ModelEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// A 200 response is parsed into events, and the request carries the bearer key, JSON
// content type, and model on POST /v1/chat/completions.
#[tokio::test]
async fn success_posts_chat_with_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer sk-test"))
        .and(header("content-type", "application/json"))
        .and(body_partial_json(serde_json::json!({"model": "gpt-4.1"})))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::from_config(cfg(&server, 0, Duration::from_secs(5)), cred()).unwrap();
    let events = run(&provider).await.expect("success");
    assert_eq!(text_of(&events), "hello");
    assert!(matches!(
        events.last(),
        Some(ModelEvent::MessageStop { .. })
    ));
}

// A 429 is retried with backoff and the follow-up 200 succeeds (two requests total).
#[tokio::test]
async fn rate_limit_is_retried_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::from_config(cfg(&server, 1, Duration::from_secs(5)), cred()).unwrap();
    let events = run(&provider).await.expect("retry succeeds");
    assert_eq!(text_of(&events), "hello");
}

// A persistent 429 surfaces an Http error with the status after 1 + max_retries attempts.
#[tokio::test]
async fn rate_limit_exhausts_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .expect(2)
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::from_config(cfg(&server, 1, Duration::from_secs(5)), cred()).unwrap();
    let err = run(&provider).await.expect_err("429 exhausts retries");
    match err {
        ProviderError::Http(msg) => {
            assert!(msg.contains("status 429"), "{msg}");
            assert!(msg.contains("slow down"), "{msg}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// A persistent 5xx is retried and then surfaced as an Http error carrying the status.
#[tokio::test]
async fn server_error_exhausts_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .expect(2)
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::from_config(cfg(&server, 1, Duration::from_secs(5)), cred()).unwrap();
    let err = run(&provider).await.expect_err("5xx exhausts retries");
    assert!(
        matches!(&err, ProviderError::Http(m) if m.contains("status 503")),
        "{err:?}"
    );
}

// A response slower than the request timeout fails with an Http error and is not retried.
#[tokio::test]
async fn request_timeout_fails_without_retry() {
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
        OpenAiProvider::from_config(cfg(&server, 3, Duration::from_millis(200)), cred()).unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(
        matches!(&err, ProviderError::Http(m) if m.starts_with("openai post:")),
        "{err:?}"
    );
}

// list_models issues GET /v1/models with the bearer key and returns model ids.
#[tokio::test]
async fn list_models_uses_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"data":[{"id":"gpt-4.1"}]}"#))
        .expect(1)
        .mount(&server)
        .await;

    let provider =
        OpenAiProvider::from_config(cfg(&server, 0, Duration::from_secs(5)), cred()).unwrap();
    assert_eq!(provider.list_models().await.unwrap(), vec!["gpt-4.1"]);
}

// The production defaults keep the 30s connect / 120s request budgets and 3 retries.
#[test]
fn default_config_preserves_timeouts() {
    let cfg = OpenAiConfig::default();
    assert_eq!(cfg.connect_timeout, Duration::from_secs(30));
    assert_eq!(cfg.request_timeout, Duration::from_secs(120));
    assert_eq!(cfg.max_retries, 3);
}
