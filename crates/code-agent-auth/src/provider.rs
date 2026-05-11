//! Trait + registry for auth providers.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::AuthError;
use crate::types::{
    AccountId, AuthStatus, LoginMethod, LoginRequest, LoginResult, ProviderId, RefreshResult,
    StoredAccount,
};

/// Plumbing exposed to providers during login/refresh/etc. Lets providers
/// do offline-friendly things (read env vars, ask for input) without
/// requiring a global state singleton. The `browser_opener` is captured
/// here so tests can substitute a fake opener.
#[derive(Clone, Default)]
pub struct AuthContext {
    pub interactive: bool,
    pub no_browser: bool,
    pub env: BTreeMap<String, String>,
    pub browser_opener: Option<Arc<dyn BrowserOpener>>,
}

impl std::fmt::Debug for AuthContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthContext")
            .field("interactive", &self.interactive)
            .field("no_browser", &self.no_browser)
            .field("env_keys", &self.env.keys().collect::<Vec<_>>())
            .field(
                "browser_opener",
                &self.browser_opener.is_some().then_some("set"),
            )
            .finish()
    }
}

impl AuthContext {
    pub fn from_process(interactive: bool) -> Self {
        let env = std::env::vars().collect();
        Self {
            interactive,
            no_browser: false,
            env,
            browser_opener: None,
        }
    }

    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_browser_opener(mut self, opener: Arc<dyn BrowserOpener>) -> Self {
        self.browser_opener = Some(opener);
        self
    }
}

/// Allows the OAuth flow to attempt to launch a browser. Real
/// implementations call `xdg-open`/`open`/`cmd /c start`. Fake implementations
/// in tests just record the URL so the harness can drive the callback.
pub trait BrowserOpener: Send + Sync {
    fn open(&self, url: &str) -> Result<(), String>;
}

/// Browser opener that records URLs to a [`Vec`] without launching a real
/// browser. Used by tests and by the `examples/auth-demo` runner.
#[derive(Default)]
pub struct RecordingBrowserOpener {
    pub opened: std::sync::Mutex<Vec<String>>,
}

impl BrowserOpener for RecordingBrowserOpener {
    fn open(&self, url: &str) -> Result<(), String> {
        self.opened.lock().unwrap().push(url.to_string());
        Ok(())
    }
}

/// Browser opener that always reports failure. Used to test the "fall
/// back to manual URL" path.
#[derive(Default)]
pub struct FailingBrowserOpener;

impl BrowserOpener for FailingBrowserOpener {
    fn open(&self, _url: &str) -> Result<(), String> {
        Err("no display available".into())
    }
}

/// Provider-side hook implementing one or more login flows.
#[async_trait]
pub trait AuthProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn display_name(&self) -> &'static str;

    fn supported_login_methods(&self) -> Vec<LoginMethod>;

    async fn login(
        &self,
        request: LoginRequest,
        ctx: AuthContext,
    ) -> Result<LoginResult, AuthError>;

    async fn refresh(
        &self,
        account: &StoredAccount,
        ctx: AuthContext,
    ) -> Result<RefreshResult, AuthError>;

    async fn logout(&self, account_id: &AccountId, ctx: AuthContext) -> Result<(), AuthError>;

    async fn status(
        &self,
        account: Option<&StoredAccount>,
        ctx: AuthContext,
    ) -> Result<AuthStatus, AuthError>;
}

/// Collection of registered [`AuthProvider`] implementations keyed by
/// [`ProviderId`].
#[derive(Default, Clone)]
pub struct AuthProviderRegistry {
    providers: BTreeMap<String, Arc<dyn AuthProvider>>,
}

impl AuthProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn AuthProvider>) {
        self.providers
            .insert(provider.id().as_str().to_string(), provider);
    }

    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn AuthProvider>> {
        self.providers.get(id.as_str()).cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn AuthProvider>)> {
        self.providers.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn ids(&self) -> Vec<ProviderId> {
        self.providers
            .keys()
            .map(|s| ProviderId::parse(s))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}
