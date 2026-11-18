use std::time::Duration;

use async_trait::async_trait;
use crom_auth::SecretString;
use crom_core::provider_retry::{parse_retry_after, parse_retry_after_ms};
use crom_core::ProviderError;

use crate::{OpenAiTransport, TransportResponse};

/// `reqwest`-backed [`OpenAiTransport`]. Non-streaming: the chat endpoint
/// returns a complete payload that the provider chunks into `ModelEvent`s.
/// Network failures are classified as `Timeout` / `Connect` (retryable) or
/// `Http`; status classification and retry happen in the provider.
pub struct ReqwestOpenAiTransport {
    client: reqwest::Client,
}

impl ReqwestOpenAiTransport {
    pub fn new(
        connect_timeout: Duration,
        read_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .read_timeout(read_timeout)
            .timeout(request_timeout)
            .build()
            .map_err(|e| ProviderError::Other(format!("openai client build: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl OpenAiTransport for ReqwestOpenAiTransport {
    async fn post_chat(
        &self,
        url: &str,
        api_key: &SecretString,
        body: &str,
    ) -> Result<TransportResponse, ProviderError> {
        let resp = self
            .client
            .post(url)
            .bearer_auth(api_key.expose())
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| classify_reqwest_error("openai post", &e))?;
        read_response(resp).await
    }

    async fn list_models(
        &self,
        url: &str,
        api_key: &SecretString,
    ) -> Result<TransportResponse, ProviderError> {
        let resp = self
            .client
            .get(url)
            .bearer_auth(api_key.expose())
            .send()
            .await
            .map_err(|e| classify_reqwest_error("openai models", &e))?;
        read_response(resp).await
    }
}

fn classify_reqwest_error(context: &str, e: &reqwest::Error) -> ProviderError {
    let message = format!("{context}: {e}");
    if e.is_timeout() {
        ProviderError::Timeout(message)
    } else if e.is_connect() || e.is_request() || e.is_body() || e.is_decode() {
        // Refused, reset, or dropped mid-body (reqwest reports a truncated
        // `.text()` body as a decode error): all transient link failures.
        ProviderError::Connect(message)
    } else {
        ProviderError::Http(message)
    }
}

async fn read_response(resp: reqwest::Response) -> Result<TransportResponse, ProviderError> {
    let status = resp.status().as_u16();
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    // OpenAI sends a millisecond-precision hint alongside the standard header.
    let retry_after = header("retry-after-ms")
        .and_then(|v| parse_retry_after_ms(&v))
        .or_else(|| header("retry-after").and_then(|v| parse_retry_after(&v)));
    let body = resp
        .text()
        .await
        .map_err(|e| classify_reqwest_error("openai body", &e))?;
    Ok(TransportResponse {
        status,
        body,
        retry_after,
    })
}
