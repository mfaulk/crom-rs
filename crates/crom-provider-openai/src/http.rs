use std::time::Duration;

use async_trait::async_trait;
use crom_auth::SecretString;
use crom_core::ProviderError;

use crate::{OpenAiTransport, TransportResponse};

/// `reqwest`-backed [`OpenAiTransport`]. Non-streaming: the chat endpoint
/// returns a complete payload that the provider chunks into `ModelEvent`s.
/// Network failures (including timeouts) surface as `ProviderError::Http`
/// and are not retried by the provider.
pub struct ReqwestOpenAiTransport {
    client: reqwest::Client,
}

impl ReqwestOpenAiTransport {
    pub fn new(
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
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
            .map_err(|e| ProviderError::Http(format!("openai post: {e}")))?;
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
            .map_err(|e| ProviderError::Http(format!("openai models: {e}")))?;
        read_response(resp).await
    }
}

async fn read_response(resp: reqwest::Response) -> Result<TransportResponse, ProviderError> {
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| ProviderError::Http(format!("openai body: {e}")))?;
    Ok(TransportResponse { status, body })
}
