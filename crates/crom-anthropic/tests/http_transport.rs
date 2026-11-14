use std::sync::{Arc, Mutex};
use std::time::Duration;

use crom_anthropic::{AnthropicConfig, AnthropicProvider};
use crom_core::{
    with_retry_observer, Message, ModelEvent, ModelProvider, ModelRequest, ProviderError,
    ProviderRetry, RetryObserver,
};
use futures::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SSE_OK: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":0,\"output_tokens\":1}}\n\n",
);

fn cfg_at(base_url: String, max_retries: u32) -> AnthropicConfig {
    AnthropicConfig {
        api_key: "ak-test".into(),
        base_url,
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(5),
        max_retries,
    }
}

fn request() -> ModelRequest {
    ModelRequest {
        model: "claude-sonnet-4-6".into(),
        system: None,
        messages: vec![Message::user_text("hi")],
        tools: vec![],
        deferred_tools: vec![],
        tool_search_config: None,
        max_tokens: 64,
    }
}

async fn run(provider: &AnthropicProvider) -> Result<Vec<ModelEvent>, ProviderError> {
    let stream = provider.stream_turn(request()).await?;
    Ok(stream.map(|e| e.unwrap()).collect().await)
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
    let provider = AnthropicProvider::new(cfg_at(server.uri(), 0)).unwrap();
    run(&provider).await.expect_err("non-2xx must fail")
}

// A 200 SSE response streams text; the request carries the API key and version headers.
#[tokio::test]
async fn success_streams_sse() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "ak-test"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_OK),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(cfg_at(server.uri(), 0)).unwrap();
    let events = run(&provider).await.expect("success");
    assert!(events
        .iter()
        .any(|e| matches!(e, ModelEvent::TextDelta { text } if text == "hello")));
}

// 401 maps to ProviderError::Auth.
#[tokio::test]
async fn status_401_maps_to_auth() {
    let err = error_for(ResponseTemplate::new(401).set_body_string("invalid x-api-key")).await;
    assert!(
        matches!(&err, ProviderError::Auth(m) if m.contains("invalid x-api-key")),
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
    let err = error_for(ResponseTemplate::new(400).set_body_string("prompt is too long")).await;
    assert!(
        matches!(&err, ProviderError::InvalidRequest { status: 400, message } if message.contains("prompt is too long")),
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
            .insert_header("retry-after", "30")
            .set_body_string("rate_limit_error"),
    )
    .await;
    match err {
        ProviderError::RateLimited {
            retry_after,
            message,
        } => {
            assert_eq!(retry_after, Some(Duration::from_secs(30)));
            assert!(message.contains("rate_limit_error"), "{message}");
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

// 529 (overloaded) and other 5xx map to ProviderError::ServerError.
#[tokio::test]
async fn status_529_maps_to_server_error() {
    let err = error_for(ResponseTemplate::new(529).set_body_string("overloaded_error")).await;
    assert!(
        matches!(&err, ProviderError::ServerError { status: 529, .. }),
        "{err:?}"
    );
}

// A refused TCP connection maps to ProviderError::Connect.
#[tokio::test]
async fn connect_error_maps_to_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let provider = AnthropicProvider::new(cfg_at(format!("http://{addr}"), 0)).unwrap();
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
                .set_body_string(SSE_OK)
                .set_delay(Duration::from_secs(5)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let provider = AnthropicProvider::new(AnthropicConfig {
        read_timeout: Duration::from_millis(200),
        ..cfg_at(server.uri(), 0)
    })
    .unwrap();
    let err = run(&provider).await.expect_err("timeout");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

// A 429 with Retry-After: 0 is retried immediately and reported to the retry observer.
#[tokio::test]
async fn rate_limit_is_retried_and_observed() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "0"))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(SSE_OK),
        )
        .expect(1)
        .mount(&server)
        .await;

    let seen: Arc<Mutex<Vec<ProviderRetry>>> = Arc::default();
    let sink = seen.clone();
    let observer: RetryObserver = Arc::new(move |e| sink.lock().unwrap().push(e.clone()));
    let provider = AnthropicProvider::new(cfg_at(server.uri(), 1)).unwrap();
    with_retry_observer(observer, run(&provider))
        .await
        .expect("retry succeeds");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].provider, "anthropic");
    assert_eq!(seen[0].error_kind, "rate_limited");
}
