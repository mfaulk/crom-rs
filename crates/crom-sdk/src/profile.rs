//! Build a [`ModelProvider`] from a resolved [`ProviderProfile`]. This is the
//! single construction path shared by the SDK builder
//! ([`crate::ProviderConfig::Profile`]), the `crom` CLI, and the eval runner,
//! so a profile id means the same endpoint, env var, and request shape
//! everywhere.

use std::sync::Arc;

use crom_auth::ResolvedCredential;
#[allow(unused_imports)] // ProviderKind is unused when every provider feature is off.
use crom_core::provider_profile::{ProviderKind, ProviderProfile};
use crom_core::ModelProvider;

use crate::error::AgentSdkError;

/// Construct the provider for `profile` using `credential`.
pub fn build_profile_provider(
    profile: &ProviderProfile,
    credential: ResolvedCredential,
) -> Result<Arc<dyn ModelProvider>, AgentSdkError> {
    let init = |e: String| AgentSdkError::provider("provider.init_failed", e);
    match profile.kind {
        #[cfg(feature = "openai")]
        ProviderKind::OpenAiChat => {
            let provider = crom_provider_openai::OpenAiProvider::from_config(
                openai_config(profile),
                credential,
            )
            .map_err(|e| init(e.to_string()))?;
            Ok(Arc::new(provider))
        }
        #[cfg(feature = "gemini")]
        ProviderKind::Gemini => {
            let mut cfg = crom_provider_gemini::GeminiConfig::default();
            if let Some(url) = &profile.base_url {
                cfg.base_url = url.clone();
            }
            let provider = crom_provider_gemini::GeminiProvider::from_config(cfg, credential)
                .map_err(|e| init(e.to_string()))?;
            Ok(Arc::new(provider))
        }
        #[cfg(feature = "anthropic")]
        ProviderKind::Anthropic => {
            let ResolvedCredential::ApiKey { key, .. } = credential else {
                return Err(AgentSdkError::invalid_request(
                    "provider.credential_unsupported",
                    format!("profile {:?} requires an API key", profile.id),
                ));
            };
            let cfg = crom_anthropic::AnthropicConfig {
                api_key: key.expose().to_string(),
                base_url: profile.base_url.clone().unwrap_or_else(|| {
                    std::env::var("ANTHROPIC_BASE_URL")
                        .unwrap_or_else(|_| "https://api.anthropic.com".into())
                }),
                connect_timeout: std::time::Duration::from_secs(30),
                read_timeout: std::time::Duration::from_secs(120),
                max_retries: 2,
            };
            let provider =
                crom_anthropic::AnthropicProvider::new(cfg).map_err(|e| init(e.to_string()))?;
            Ok(Arc::new(provider))
        }
        #[allow(unreachable_patterns)]
        kind => {
            let _ = (credential, init);
            Err(AgentSdkError::invalid_request(
                "provider.feature_disabled",
                format!(
                    "profile {:?} needs the {kind:?} provider feature, which is disabled",
                    profile.id
                ),
            ))
        }
    }
}

/// OpenAI transport config for an `openai-chat` profile.
#[cfg(feature = "openai")]
pub fn openai_config(profile: &ProviderProfile) -> crom_provider_openai::OpenAiConfig {
    let mut cfg = crom_provider_openai::OpenAiConfig::default();
    if let Some(url) = &profile.base_url {
        cfg.base_url = url.trim_end_matches('/').to_string();
    }
    cfg.max_tokens_field = profile.max_tokens_field;
    cfg
}
