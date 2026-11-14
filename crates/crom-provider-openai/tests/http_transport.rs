#![cfg(feature = "http")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crom_auth::{CredentialSource, ProviderId, ResolvedCredential, SecretString};
use crom_core::{
    with_retry_observer, Message, ModelEvent, ModelProvider, ModelRequest, ProviderError,
    ProviderRetry, RetryObserver,
};
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

/// Config pointed at `base_url` with a 1ms retry backoff so retry tests do
/// not sleep for real.
fn cfg_at(base_url: String, max_retries: u32) -> OpenAiConfig {
    OpenAiConfig {
        base_url,
        max_retries,
        retry_base_delay: Duration::from_millis(1),
        ..OpenAiConfig::default()
    }
}

fn cfg(server: &MockServer, max_retries: u32) -> OpenAiConfig {
    cfg_at(server.uri(), max_retries)
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

/// Serve `template` for every POST and return the error from a
/// no-retry turn.
async fn error_for(template: ResponseTemplate) -> ProviderError {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template)
        .expect(1)
        .mount(&server)
        .await;
    let provider = OpenAiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    run(&provider).await.expect_err("non-2xx must fail")
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

    let provider = OpenAiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    let events = run(&provider).await.expect("success");
    assert_eq!(text_of(&events), "hello");
    assert!(matches!(
        events.last(),
        Some(ModelEvent::MessageStop { .. })
    ));
}

// 401 maps to ProviderError::Auth.
#[tokio::test]
async fn status_401_maps_to_auth() {
    let err = error_for(ResponseTemplate::new(401).set_body_string("bad key")).await;
    assert!(
        matches!(&err, ProviderError::Auth(m) if m.contains("bad key")),
        "{err:?}"
    );
}

// 403 maps to ProviderError::Auth.
#[tokio::test]
async fn status_403_maps_to_auth() {
    let err = error_for(ResponseTemplate::new(403)).await;
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
}

// 400 maps to ProviderError::InvalidRequest carrying the status and body.
#[tokio::test]
async fn status_400_maps_to_invalid_request() {
    let err =
        error_for(ResponseTemplate::new(400).set_body_string("context_length_exceeded")).await;
    assert!(
        matches!(&err, ProviderError::InvalidRequest { status: 400, message } if message.contains("context_length_exceeded")),
        "{err:?}"
    );
}

// 408 maps to ProviderError::Timeout.
#[tokio::test]
async fn status_408_maps_to_timeout() {
    let err = error_for(ResponseTemplate::new(408)).await;
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

// 429 maps to ProviderError::RateLimited with the Retry-After hint.
#[tokio::test]
async fn status_429_maps_to_rate_limited_with_retry_after() {
    let err = error_for(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "7")
            .set_body_string("slow down"),
    )
    .await;
    match err {
        ProviderError::RateLimited {
            retry_after,
            message,
        } => {
            assert_eq!(retry_after, Some(Duration::from_secs(7)));
            assert!(message.contains("slow down"), "{message}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// OpenAI's millisecond retry-after-ms header takes precedence over Retry-After.
#[tokio::test]
async fn status_429_prefers_retry_after_ms() {
    let err = error_for(
        ResponseTemplate::new(429)
            .insert_header("retry-after-ms", "1500")
            .insert_header("retry-after", "9"),
    )
    .await;
    assert_eq!(err.retry_after(), Some(Duration::from_millis(1_500)));
}

// 5xx maps to ProviderError::ServerError carrying the status.
#[tokio::test]
async fn status_503_maps_to_server_error() {
    let err = error_for(ResponseTemplate::new(503).set_body_string("overloaded")).await;
    assert!(
        matches!(&err, ProviderError::ServerError { status: 503, .. }),
        "{err:?}"
    );
}

// A refused TCP connection maps to ProviderError::Connect.
#[tokio::test]
async fn connect_error_maps_to_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let provider =
        OpenAiProvider::from_config(cfg_at(format!("http://{addr}"), 0), cred()).unwrap();
    let err = run(&provider).await.expect_err("connect refused");
    assert!(matches!(err, ProviderError::Connect(_)), "{err:?}");
}

// A server that stalls past the idle read timeout maps to ProviderError::Timeout.
#[tokio::test]
async fn read_timeout_maps_to_timeout() {
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
    let provider = OpenAiProvider::from_config(
        OpenAiConfig {
            read_timeout: Duration::from_millis(200),
            ..cfg(&server, 0)
        },
        cred(),
    )
    .unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

// A total request timeout also maps to ProviderError::Timeout.
#[tokio::test]
async fn total_timeout_maps_to_timeout() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(OK_BODY)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;
    let provider = OpenAiProvider::from_config(
        OpenAiConfig {
            request_timeout: Duration::from_millis(200),
            ..cfg(&server, 0)
        },
        cred(),
    )
    .unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

// A 429 is retried and the follow-up 200 succeeds; the retry is reported to the observer.
#[tokio::test]
async fn rate_limit_is_retried_and_observed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string("slow down"),
        )
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let seen: Arc<Mutex<Vec<ProviderRetry>>> = Arc::default();
    let sink = seen.clone();
    let observer: RetryObserver = Arc::new(move |e| sink.lock().unwrap().push(e.clone()));
    let provider = OpenAiProvider::from_config(cfg(&server, 1), cred()).unwrap();
    let events = with_retry_observer(observer, run(&provider))
        .await
        .expect("retry succeeds");
    assert_eq!(text_of(&events), "hello");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].provider, "openai");
    assert_eq!(seen[0].attempt, 1);
    assert_eq!(seen[0].error_kind, "rate_limited");
}

// A persistent 5xx is retried max_retries times and then surfaced as ServerError.
#[tokio::test]
async fn server_error_exhausts_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("overloaded"))
        .expect(3)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::from_config(cfg(&server, 2), cred()).unwrap();
    let err = run(&provider).await.expect_err("5xx exhausts retries");
    assert!(
        matches!(&err, ProviderError::ServerError { status: 503, message } if message.contains("overloaded")),
        "{err:?}"
    );
}

// A connection failure is retried like any other transient error.
#[tokio::test]
async fn connect_error_is_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let seen: Arc<Mutex<Vec<ProviderRetry>>> = Arc::default();
    let sink = seen.clone();
    let observer: RetryObserver = Arc::new(move |e| sink.lock().unwrap().push(e.clone()));
    let provider =
        OpenAiProvider::from_config(cfg_at(format!("http://{addr}"), 2), cred()).unwrap();
    let err = with_retry_observer(observer, run(&provider))
        .await
        .expect_err("connect refused");
    assert!(matches!(err, ProviderError::Connect(_)), "{err:?}");
    assert_eq!(seen.lock().unwrap().len(), 2);
}

// A non-retryable 400 is sent exactly once even with retries configured.
#[tokio::test]
async fn invalid_request_is_not_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400))
        .expect(1)
        .mount(&server)
        .await;
    let provider = OpenAiProvider::from_config(cfg(&server, 3), cred()).unwrap();
    let err = run(&provider).await.expect_err("400");
    assert!(
        matches!(err, ProviderError::InvalidRequest { .. }),
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

    let provider = OpenAiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    assert_eq!(provider.list_models().await.unwrap(), vec!["gpt-4.1"]);
}

// list_models classifies a 401 as ProviderError::Auth.
#[tokio::test]
async fn list_models_classifies_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let provider = OpenAiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    let err = provider.list_models().await.expect_err("401");
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
}

// Defaults: 30s connect, 300s idle read, 600s total, 3 retries from a 250ms base.
#[test]
fn default_config_timeouts_and_retries() {
    let cfg = OpenAiConfig::default();
    assert_eq!(cfg.connect_timeout, Duration::from_secs(30));
    assert_eq!(cfg.read_timeout, Duration::from_secs(300));
    assert_eq!(cfg.request_timeout, Duration::from_secs(600));
    assert_eq!(cfg.max_retries, 3);
    assert_eq!(cfg.retry_base_delay, Duration::from_millis(250));
    assert_eq!(cfg.max_tokens_field, crom_core::MaxTokensField::MaxTokens);
}

// With `max_tokens_field = MaxCompletionTokens` the limit is sent as
// `max_completion_tokens` and `max_tokens` is omitted.
#[tokio::test]
async fn max_tokens_field_is_configurable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(body_partial_json(
            serde_json::json!({"max_completion_tokens": 64}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let mut c = cfg(&server, 0);
    c.max_tokens_field = crom_core::MaxTokensField::MaxCompletionTokens;
    let provider = OpenAiProvider::from_config(c, cred()).unwrap();
    run(&provider).await.expect("success");
    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert!(body.get("max_tokens").is_none(), "{body}");
}
