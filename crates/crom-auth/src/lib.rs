//! Provider login, credential storage, OAuth (browser + device-code),
//! credential resolution, and a unified model registry for crom (M10).
//!
//! The crate is dependency-light: it only pulls in the workspace's
//! standard async + serde stack, plus `sha2`/`hex` for PKCE and the
//! redacted account-id hash. No HTTP client is bundled — providers and
//! the OAuth flows accept an [`oauth::browser::HttpTransport`] that the host
//! crate is free to back with `reqwest`, a fake transport, or anything
//! else.

pub mod ci;
pub mod error;
pub mod events;
pub mod model_registry;
pub mod oauth;
pub mod precedence;
pub mod provider;
pub mod providers;
pub mod store;
pub mod types;

pub use error::{AuthError, AuthErrorKind};
pub use events::{
    account_summary, AuthProviderRegistryLoaded, AuthProviderSelected, AuthStatusChecked,
    CredentialRefreshFailed, CredentialRefreshed, CredentialStored, LoginBrowserOpened,
    LoginCompleted, LoginDeviceCodeDisplayed, LoginFailed, LoginStarted, LogoutCompleted,
    ModelListLoaded, ModelSelected,
};
pub use model_registry::{starter_registry, ModelAvailability, ModelInfo, ModelRegistry};
pub use precedence::{provider_env_var, resolve_credential, AuthResolverInputs, ResolvedAuth};
pub use provider::{
    AuthContext, AuthProvider, AuthProviderRegistry, BrowserOpener, FailingBrowserOpener,
    RecordingBrowserOpener,
};
pub use providers::{
    AnthropicAuthProvider, CodexAuthProvider, FakeAuthProvider, FakeProviderState,
    GeminiAuthProvider, OpenAiAuthProvider,
};
pub use store::{
    AccountIndex, CredentialStore, FakeKeychainBackend, FileCredentialBackend, NoopBackend,
    SecretBackend, SystemKeychainBackend,
};
pub use types::{
    AccountId, AuthPrecedenceMode, AuthStatus, BillingMode, CredentialSecretKind, CredentialSource,
    CredentialStoreMode, LoginAlternative, LoginMethod, LoginMethodKind, LoginRequest, LoginResult,
    ProviderId, ProviderRateLimit, ProviderUsageStatus, RefreshResult, ResolvedCredential,
    SecretString, StoredAccount, StoredCredentialSecret,
};

/// Build the default registry: API-key auth providers for Anthropic,
/// OpenAI, Gemini; the Codex provider in its safe default mode; and the
/// Fake provider for tests. Hosts can call [`AuthProviderRegistry::register`]
/// to add Bedrock/Vertex/Foundry once those become available.
pub fn default_registry(fake_state: std::sync::Arc<FakeProviderState>) -> AuthProviderRegistry {
    let mut reg = AuthProviderRegistry::new();
    reg.register(std::sync::Arc::new(AnthropicAuthProvider));
    reg.register(std::sync::Arc::new(OpenAiAuthProvider));
    reg.register(std::sync::Arc::new(GeminiAuthProvider));
    reg.register(std::sync::Arc::new(CodexAuthProvider::default()));
    reg.register(std::sync::Arc::new(FakeAuthProvider::new(fake_state)));
    reg
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    // default_registry registers all five built-in providers.
    fn default_registry_has_all_builtins() {
        let state = Arc::new(FakeProviderState::new());
        let reg = default_registry(state);
        assert!(reg.get(&ProviderId::Anthropic).is_some());
        assert!(reg.get(&ProviderId::OpenAi).is_some());
        assert!(reg.get(&ProviderId::Gemini).is_some());
        assert!(reg.get(&ProviderId::Codex).is_some());
        assert!(reg.get(&ProviderId::Fake).is_some());
    }
}
