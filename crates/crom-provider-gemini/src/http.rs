use std::time::Duration;

use async_trait::async_trait;
use crom_auth::SecretString;
use crom_core::provider_retry::parse_retry_after;
use crom_core::ProviderError;

use crate::{GeminiTransport, TransportResponse, GEMINI_API_KEY_HEADER};

/// `reqwest`-backed [`GeminiTransport`]. The API key is sent via the
/// `x-goog-api-key` header rather than `?key=` so it does not leak into
/// URL-shaped surfaces (logs, traces, panic backtraces, reqwest debug output).
pub struct ReqwestGeminiTransport {
    client: reqwest::Client,
}

impl ReqwestGeminiTransport {
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
            .map_err(|e| ProviderError::Other(format!("gemini client build: {e}")))?;
        Ok(Self { client })
    }
}

#[async_trait]
impl GeminiTransport for ReqwestGeminiTransport {
    async fn post_generate(
        &self,
        url: &str,
        api_key: &SecretString,
        body: &str,
    ) -> Result<TransportResponse, ProviderError> {
        let resp = self
            .client
            .post(url)
            .header(GEMINI_API_KEY_HEADER, api_key.expose())
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| classify_reqwest_error("gemini post", &e))?;
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
            .header(GEMINI_API_KEY_HEADER, api_key.expose())
            .send()
            .await
            .map_err(|e| classify_reqwest_error("gemini models", &e))?;
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
    let retry_after = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_retry_after);
    let body = resp
        .text()
        .await
        .map_err(|e| classify_reqwest_error("gemini body", &e))?;
    Ok(TransportResponse {
        status,
        body,
        retry_after,
    })
}
