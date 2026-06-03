//! Verifies that a host-supplied system prompt is threaded all the way
//! down to the `ModelRequest.system` field the provider receives.
//!
//! Regression coverage for the SDK builder / request `system_prompt`
//! wiring: before it existed, `AgentOptions` was always built with
//! `..Default::default()`, so the system prompt was silently dropped and
//! every run went to the model with `system: None`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use code_agent_core::{ModelEvent, ModelProvider, ModelRequest, ProviderError, TokenUsage};
use code_agent_sdk::{AgentRequest, AgentRuntime, ProviderConfig};
use futures::stream::{self, BoxStream};

/// Provider that records the `system` field of the first request it sees,
/// then immediately ends the turn so the agent loop terminates.
#[derive(Clone, Default)]
struct CapturingProvider {
    system: Arc<Mutex<Option<Option<String>>>>,
}

#[async_trait]
impl ModelProvider for CapturingProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        // Record only the first turn's system prompt.
        let mut slot = self.system.lock().unwrap();
        if slot.is_none() {
            *slot = Some(request.system.clone());
        }
        Ok(Box::pin(stream::iter(vec![Ok(ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: Some(TokenUsage::default()),
        })])))
    }
}

const SENTINEL: &str = "SYSTEM-PROMPT-SENTINEL-do-not-remove";

#[tokio::test]
async fn runtime_level_system_prompt_reaches_provider() {
    let provider = CapturingProvider::default();
    let captured = provider.system.clone();

    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::external(Arc::new(provider)))
        .system_prompt(SENTINEL)
        .build()
        .await
        .expect("runtime builds");

    let _ = runtime
        .run_once(AgentRequest::new("do something"))
        .await
        .expect("run completes");

    let system = captured
        .lock()
        .unwrap()
        .clone()
        .expect("provider was called");
    let system = system.expect("system prompt should be present");
    assert!(
        system.contains(SENTINEL),
        "runtime system prompt missing from ModelRequest.system: {system:?}"
    );
}

#[tokio::test]
async fn request_level_system_prompt_overrides_runtime_default() {
    let provider = CapturingProvider::default();
    let captured = provider.system.clone();

    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::external(Arc::new(provider)))
        .system_prompt("runtime-default-should-be-overridden")
        .build()
        .await
        .expect("runtime builds");

    let per_request = format!("{SENTINEL}-per-request");
    let _ = runtime
        .run_once(AgentRequest::new("do something").system_prompt(per_request.clone()))
        .await
        .expect("run completes");

    let system = captured
        .lock()
        .unwrap()
        .clone()
        .expect("provider was called")
        .expect("system prompt should be present");
    assert!(
        system.contains(&per_request),
        "per-request system prompt did not win: {system:?}"
    );
    assert!(
        !system.contains("runtime-default-should-be-overridden"),
        "runtime default leaked despite per-request override: {system:?}"
    );
}
