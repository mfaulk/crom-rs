//! Provider configuration and credential injection.
//!
//! The SDK accepts pre-resolved credentials (`SecretString` API keys,
//! `StoredCredentialSecret` OAuth tokens) so applications can avoid the
//! global keychain/login flow. Apps that *want* the auth store can opt in
//! explicitly via [`ProviderConfig::FromAuthStore`].

use std::sync::Arc;

use crom_auth::{AccountId, ProviderId, ResolvedCredential, StoredCredentialSecret};
use crom_core::provider_profile::ProviderProfile;
use crom_core::ModelProvider;

pub use crom_auth::SecretString;

/// Source of provider credentials for a run.
pub enum ProviderConfig {
    AnthropicApiKey {
        api_key: SecretString,
        base_url: Option<String>,
    },
    OpenAiApiKey {
        api_key: SecretString,
        base_url: Option<String>,
    },
    GeminiApiKey {
        api_key: SecretString,
        base_url: Option<String>,
    },
    /// A resolved provider profile (see `docs/providers.md`) plus its API
    /// key. Endpoint, request shape, and default model come from the profile.
    Profile {
        profile: ProviderProfile,
        api_key: SecretString,
    },
    OAuthCredential {
        provider: ProviderId,
        credential: StoredCredentialSecret,
    },
    FromAuthStore {
        provider: ProviderId,
        account: Option<AccountId>,
    },
    External {
        provider: Arc<dyn ModelProvider>,
        credential: Option<ResolvedCredential>,
    },
    Fake {
        provider: Arc<dyn ModelProvider>,
    },
}

impl ProviderConfig {
    pub fn anthropic_api_key(key: impl Into<SecretString>) -> Self {
        Self::AnthropicApiKey {
            api_key: key.into(),
            base_url: None,
        }
    }

    pub fn openai_api_key(key: impl Into<SecretString>) -> Self {
        Self::OpenAiApiKey {
            api_key: key.into(),
            base_url: None,
        }
    }

    pub fn gemini_api_key(key: impl Into<SecretString>) -> Self {
        Self::GeminiApiKey {
            api_key: key.into(),
            base_url: None,
        }
    }

    pub fn profile(profile: ProviderProfile, key: impl Into<SecretString>) -> Self {
        Self::Profile {
            profile,
            api_key: key.into(),
        }
    }

    pub fn external(provider: Arc<dyn ModelProvider>) -> Self {
        Self::External {
            provider,
            credential: None,
        }
    }

    pub fn fake(provider: Arc<dyn ModelProvider>) -> Self {
        Self::Fake { provider }
    }

    /// Endpoint host (`host[:port]`) for run metadata, when the config
    /// names an endpoint. Never includes the scheme, path, or userinfo.
    pub fn endpoint_host(&self) -> Option<String> {
        match self {
            Self::Profile { profile, .. } => profile.base_url_host(),
            Self::AnthropicApiKey { base_url, .. }
            | Self::OpenAiApiKey { base_url, .. }
            | Self::GeminiApiKey { base_url, .. } => base_url
                .as_deref()
                .and_then(crom_core::provider_profile::url_host),
            _ => None,
        }
    }

    /// Provider id reported in events and final results.
    pub fn provider_id(&self) -> ProviderId {
        match self {
            Self::AnthropicApiKey { .. } => ProviderId::Anthropic,
            Self::OpenAiApiKey { .. } => ProviderId::OpenAi,
            Self::GeminiApiKey { .. } => ProviderId::Gemini,
            Self::Profile { profile, .. } => ProviderId::parse(&profile.id),
            Self::OAuthCredential { provider, .. } | Self::FromAuthStore { provider, .. } => {
                provider.clone()
            }
            Self::External { credential, .. } => credential
                .as_ref()
                .and_then(|c| c.provider().cloned())
                .unwrap_or(ProviderId::Custom("external".into())),
            Self::Fake { .. } => ProviderId::Fake,
        }
    }
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AnthropicApiKey { .. } => f.write_str("AnthropicApiKey { .. <redacted> }"),
            Self::OpenAiApiKey { .. } => f.write_str("OpenAiApiKey { .. <redacted> }"),
            Self::GeminiApiKey { .. } => f.write_str("GeminiApiKey { .. <redacted> }"),
            Self::Profile { profile, .. } => f
                .debug_struct("Profile")
                .field("profile", profile)
                .field("api_key", &"<redacted>")
                .finish(),
            Self::OAuthCredential { provider, .. } => f
                .debug_struct("OAuthCredential")
                .field("provider", provider)
                .field("credential", &"<redacted>")
                .finish(),
            Self::FromAuthStore { provider, account } => f
                .debug_struct("FromAuthStore")
                .field("provider", provider)
                .field("account", account)
                .finish(),
            Self::External { credential, .. } => f
                .debug_struct("External")
                .field("provider", &"<dyn ModelProvider>")
                .field("credential", credential)
                .finish(),
            Self::Fake { .. } => f.write_str("Fake { provider: <dyn ModelProvider> }"),
        }
    }
}

/// Per-request override of the runtime's default provider configuration.
#[derive(Debug, Clone)]
pub enum ProviderSelection {
    /// Use the runtime's configured default provider.
    Default,
    /// Force a specific provider id (must already be configured).
    Provider(ProviderId),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ProviderConfig::anthropic_api_key produces an Anthropic provider id.
    #[test]
    fn provider_id_anthropic_api_key() {
        let pc = ProviderConfig::anthropic_api_key(SecretString::new("sk-x".into()));
        assert_eq!(pc.provider_id(), ProviderId::Anthropic);
    }

    /// ProviderConfig Debug never includes the api key string.
    #[test]
    fn debug_does_not_leak_api_key() {
        let pc = ProviderConfig::anthropic_api_key(SecretString::new("super-secret".into()));
        let s = format!("{pc:?}");
        assert!(!s.contains("super-secret"));
        assert!(s.contains("redacted"));
    }
}
