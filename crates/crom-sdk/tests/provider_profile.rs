#![cfg(feature = "openai")]

use std::collections::BTreeMap;

use crom_auth::{CredentialSource, ProviderId, ResolvedCredential};
use crom_core::provider_profile::resolve_profile;
use crom_core::{Message, ModelEvent, ModelRequest};
use crom_sdk::profile::build_profile_provider;
use crom_sdk::{AgentRuntime, ProviderConfig, SecretString};
use futures::StreamExt;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const OK_BODY: &str = r#"{"choices":[{"message":{"content":"hi from deepseek","reasoning_content":"thinking..."},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;

fn deepseek_at(uri: &str) -> crom_core::ProviderProfile {
    let mut overrides = BTreeMap::new();
    overrides.insert(
        "deepseek".to_string(),
        serde_json::from_value(serde_json::json!({ "base_url": uri })).unwrap(),
    );
    resolve_profile("deepseek", &overrides).unwrap()
}

// A deepseek profile builds an openai-chat provider that posts to the profile's
// base_url with the bearer key and `max_tokens`, and drops `reasoning_content`.
#[tokio::test]
async fn deepseek_profile_posts_to_profile_base_url() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer ds-test"))
        .and(body_partial_json(
            serde_json::json!({"model": "deepseek-chat", "max_tokens": 32}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(OK_BODY))
        .expect(1)
        .mount(&server)
        .await;

    let profile = deepseek_at(&server.uri());
    let cred = ResolvedCredential::ApiKey {
        provider: ProviderId::parse("deepseek"),
        source: CredentialSource::TestFake,
        key: SecretString::new("ds-test".into()),
    };
    let provider = build_profile_provider(&profile, cred).unwrap();
    let stream = provider
        .stream_turn(ModelRequest {
            model: "deepseek-chat".into(),
            system: None,
            messages: vec![Message::user_text("hi")],
            tools: vec![],
            deferred_tools: vec![],
            tool_search_config: None,
            max_tokens: 32,
        })
        .await
        .unwrap();
    let events: Vec<ModelEvent> = stream.map(|e| e.unwrap()).collect().await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hi from deepseek");
}

// ProviderConfig::profile reports the profile id and default model and never
// leaks the key through Debug.
#[tokio::test]
async fn profile_provider_config_builds_runtime() {
    let profile = deepseek_at("http://127.0.0.1:9");
    let cfg = ProviderConfig::profile(profile, SecretString::new("ds-secret-key".into()));
    assert!(!format!("{cfg:?}").contains("ds-secret-key"));
    assert_eq!(cfg.provider_id().as_str(), "deepseek");
    let rt = AgentRuntime::builder().provider(cfg).build().await.unwrap();
    assert_eq!(rt.default_model(), "deepseek-chat");
    assert_eq!(rt.provider_id().as_str(), "deepseek");
    assert_eq!(rt.endpoint_host().as_deref(), Some("127.0.0.1:9"));
}

// A profile with no default model and no explicit `.model(..)` is rejected
// rather than silently sending another provider's model id.
#[tokio::test]
async fn profile_without_default_model_requires_explicit_model() {
    let profile = resolve_profile("local", &BTreeMap::new()).unwrap();
    let err = AgentRuntime::builder()
        .provider(ProviderConfig::profile(
            profile.clone(),
            SecretString::new("x".into()),
        ))
        .build()
        .await
        .unwrap_err();
    assert!(err.to_string().contains("model"), "{err}");
    let rt = AgentRuntime::builder()
        .provider(ProviderConfig::profile(
            profile,
            SecretString::new("x".into()),
        ))
        .model("llama3.1")
        .build()
        .await
        .unwrap();
    assert_eq!(rt.default_model(), "llama3.1");
}
