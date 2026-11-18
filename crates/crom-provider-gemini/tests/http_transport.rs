#![cfg(feature = "http")]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crom_auth::{CredentialSource, ProviderId, ResolvedCredential, SecretString};
use crom_core::{
    with_retry_observer, Message, ModelEvent, ModelProvider, ModelRequest, ProviderError,
    ProviderRetry, RetryObserver,
};
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

/// Config pointed at `base_url` with a 1ms retry backoff so retry tests do
/// not sleep for real.
fn cfg_at(base_url: String, max_retries: u32) -> GeminiConfig {
    GeminiConfig {
        base_url,
        max_retries,
        retry_base_delay: Duration::from_millis(1),
        ..GeminiConfig::default()
    }
}

fn cfg(server: &MockServer, max_retries: u32) -> GeminiConfig {
    cfg_at(server.uri(), max_retries)
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

fn text_of(events: &[ModelEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn recording_observer() -> (RetryObserver, Arc<Mutex<Vec<ProviderRetry>>>) {
    let seen: Arc<Mutex<Vec<ProviderRetry>>> = Arc::default();
    let sink = seen.clone();
    (
        Arc::new(move |e: &ProviderRetry| sink.lock().unwrap().push(e.clone())),
        seen,
    )
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
    let provider = GeminiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    run(&provider).await.expect_err("non-2xx must fail")
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

    let provider = GeminiProvider::from_config(cfg(&server, 0), cred()).unwrap();
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
    let err = error_for(ResponseTemplate::new(401).set_body_string("API key not valid")).await;
    assert!(
        matches!(&err, ProviderError::Auth(m) if m.contains("API key not valid")),
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
    let err = error_for(ResponseTemplate::new(400).set_body_string("INVALID_ARGUMENT")).await;
    assert!(
        matches!(&err, ProviderError::InvalidRequest { status: 400, message } if message.contains("INVALID_ARGUMENT")),
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
            .insert_header("retry-after", "12")
            .set_body_string("quota"),
    )
    .await;
    match err {
        ProviderError::RateLimited {
            retry_after,
            message,
        } => {
            assert_eq!(retry_after, Some(Duration::from_secs(12)));
            assert!(message.contains("quota"), "{message}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// 5xx maps to ProviderError::ServerError carrying the status.
#[tokio::test]
async fn status_500_maps_to_server_error() {
    let err = error_for(ResponseTemplate::new(500).set_body_string("boom")).await;
    assert!(
        matches!(&err, ProviderError::ServerError { status: 500, message } if message.contains("boom")),
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
        GeminiProvider::from_config(cfg_at(format!("http://{addr}"), 0), cred()).unwrap();
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
    let provider = GeminiProvider::from_config(
        GeminiConfig {
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
    let provider = GeminiProvider::from_config(
        GeminiConfig {
            request_timeout: Duration::from_millis(200),
            ..cfg(&server, 0)
        },
        cred(),
    )
    .unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

// Gemini retries a transient 503 and the follow-up 200 succeeds; the retry is observed.
#[tokio::test]
async fn transient_server_error_is_retried_and_observed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_string("unavailable"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let (observer, seen) = recording_observer();
    let provider = GeminiProvider::from_config(cfg(&server, 2), cred()).unwrap();
    let events = with_retry_observer(observer, run(&provider))
        .await
        .expect("retry succeeds");
    assert_eq!(text_of(&events), "hello");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].provider, "gemini");
    assert_eq!(seen[0].error_kind, "server_error");
}

// Gemini retries a 429 honouring a zero Retry-After and then succeeds.
#[tokio::test]
async fn rate_limit_is_retried_then_succeeds() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;
    let provider = GeminiProvider::from_config(cfg(&server, 1), cred()).unwrap();
    assert_eq!(text_of(&run(&provider).await.expect("retry")), "hello");
}

// A persistent 5xx is attempted 1 + max_retries times, then surfaced as ServerError.
#[tokio::test]
async fn server_error_exhausts_retries() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount(&server)
        .await;
    let provider = GeminiProvider::from_config(cfg(&server, 2), cred()).unwrap();
    let err = run(&provider).await.expect_err("5xx");
    assert!(
        matches!(err, ProviderError::ServerError { status: 500, .. }),
        "{err:?}"
    );
}

// A connection failure is retried like any other transient error.
#[tokio::test]
async fn connect_error_is_retried() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let (observer, seen) = recording_observer();
    let provider =
        GeminiProvider::from_config(cfg_at(format!("http://{addr}"), 2), cred()).unwrap();
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
    let provider = GeminiProvider::from_config(cfg(&server, 3), cred()).unwrap();
    let err = run(&provider).await.expect_err("400");
    assert!(
        matches!(err, ProviderError::InvalidRequest { .. }),
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

    let provider = GeminiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    assert_eq!(
        provider.list_models().await.unwrap(),
        vec!["models/gemini-1.5-pro"]
    );
}

// list_models classifies a 403 as ProviderError::Auth.
#[tokio::test]
async fn list_models_classifies_auth_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&server)
        .await;
    let provider = GeminiProvider::from_config(cfg(&server, 0), cred()).unwrap();
    let err = provider.list_models().await.expect_err("403");
    assert!(matches!(err, ProviderError::Auth(_)), "{err:?}");
}

// Defaults: 30s connect, 300s idle read, 600s total, 3 retries from a 250ms base.
#[test]
fn default_config_timeouts_and_retries() {
    let cfg = GeminiConfig::default();
    assert_eq!(cfg.connect_timeout, Duration::from_secs(30));
    assert_eq!(cfg.read_timeout, Duration::from_secs(300));
    assert_eq!(cfg.request_timeout, Duration::from_secs(600));
    assert_eq!(cfg.max_retries, 3);
    assert_eq!(cfg.retry_base_delay, Duration::from_millis(250));
}

/// Answer every connection with `reply` and close it; counts connections.
fn raw_server(reply: &'static [u8]) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            count.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 8192];
            let _ = s.read(&mut buf);
            let _ = s.write_all(reply);
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    });
    (format!("http://{addr}"), hits)
}

// A connection closed before any response is retryable and retried max_retries times.
#[tokio::test]
async fn dropped_connection_before_response_is_retried() {
    let (url, hits) = raw_server(b"");
    let provider = GeminiProvider::from_config(cfg_at(url, 2), cred()).unwrap();
    let err = run(&provider).await.expect_err("connection dropped");
    assert!(err.is_retryable(), "{err:?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
}

// A response body cut short by a dropped connection is retryable and retried.
#[tokio::test]
async fn truncated_response_body_is_retried() {
    let (url, hits) = raw_server(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 500\r\n\r\n{\"candidates\":",
    );
    let provider = GeminiProvider::from_config(cfg_at(url, 2), cred()).unwrap();
    let err = run(&provider).await.expect_err("body truncated");
    assert!(err.is_retryable(), "{err:?}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
}
