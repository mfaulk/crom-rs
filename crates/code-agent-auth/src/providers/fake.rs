//! Fully offline fake provider for tests, demos, and the
//! `examples/auth-demo` runner. Supports API-key login, OAuth browser
//! login, and OAuth device-code login through a deterministic in-process
//! "server" that lives inside [`FakeProviderState`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::oauth::browser::{HttpResponse, HttpTransport};
use crate::provider::{AuthContext, AuthProvider};
use crate::types::{
    AccountId, AuthStatus, BillingMode, LoginMethod, LoginMethodKind, LoginRequest, LoginResult,
    ProviderId, RefreshResult, SecretString, StoredAccount, StoredCredentialSecret,
};

/// Shared, mutable state used by the fake auth provider, the fake OAuth
/// server, and the fake model provider so tests can wire them together.
#[derive(Default)]
pub struct FakeProviderState {
    pub accepted_api_keys: Mutex<Vec<String>>,
    pub issued_tokens: Mutex<BTreeMap<String, FakeTokenRecord>>,
    pub device_authorizations: Mutex<BTreeMap<String, FakeDeviceAuthorization>>,
    pub deny_next_device: Mutex<bool>,
    pub force_state_mismatch: Mutex<bool>,
}

#[derive(Clone, Debug)]
pub struct FakeTokenRecord {
    pub email: String,
    pub plan: String,
    pub access_token: String,
    pub refresh_token: String,
    pub billing_mode: BillingMode,
}

#[derive(Clone, Debug)]
pub struct FakeDeviceAuthorization {
    pub user_code: String,
    pub email: String,
    pub plan: String,
    pub authorized: bool,
}

impl FakeProviderState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn allow_api_key(&self, key: impl Into<String>) {
        self.accepted_api_keys.lock().unwrap().push(key.into());
    }

    /// Pre-populate the OAuth code so the fake server returns it from
    /// `/token`. The same code is what the OAuth flow's `force_callback_query`
    /// must include.
    pub fn issue_token(&self, code: &str, record: FakeTokenRecord) {
        self.issued_tokens
            .lock()
            .unwrap()
            .insert(code.to_string(), record);
    }

    /// Pre-populate a device-code authorization. Mark `authorized = true`
    /// to make the next `/token` poll succeed; otherwise it returns
    /// `authorization_pending` until the test mutates the state.
    pub fn issue_device_authorization(&self, device_code: &str, auth: FakeDeviceAuthorization) {
        self.device_authorizations
            .lock()
            .unwrap()
            .insert(device_code.to_string(), auth);
    }

    pub fn approve_device(&self, device_code: &str) {
        if let Some(a) = self
            .device_authorizations
            .lock()
            .unwrap()
            .get_mut(device_code)
        {
            a.authorized = true;
        }
    }
}

pub struct FakeAuthProvider {
    state: Arc<FakeProviderState>,
}

impl FakeAuthProvider {
    pub fn new(state: Arc<FakeProviderState>) -> Self {
        Self { state }
    }

    pub fn state(&self) -> &Arc<FakeProviderState> {
        &self.state
    }
}

#[async_trait]
impl AuthProvider for FakeAuthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Fake
    }

    fn display_name(&self) -> &'static str {
        "Fake (offline)"
    }

    fn supported_login_methods(&self) -> Vec<LoginMethod> {
        vec![
            LoginMethod::ApiKey,
            LoginMethod::OAuthBrowser,
            LoginMethod::OAuthDeviceCode,
        ]
    }

    async fn login(
        &self,
        request: LoginRequest,
        _ctx: AuthContext,
    ) -> Result<LoginResult, AuthError> {
        let key = request.api_key.ok_or_else(|| {
            AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "Fake API-key login requires --api-key.",
            )
        })?;
        let allowed = self.state.accepted_api_keys.lock().unwrap();
        if !allowed.is_empty() && !allowed.iter().any(|a| a == key.expose()) {
            return Err(AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "fake provider rejected the supplied API key",
            ));
        }
        Ok(LoginResult {
            account: StoredAccount {
                account_id: AccountId::new("fake-api-key"),
                provider: ProviderId::Fake,
                login_method: LoginMethodKind::ApiKey,
                display_email: None,
                workspace_id: None,
                plan_type: Some("api".into()),
                created_at: OffsetDateTime::now_utc(),
                last_refresh_at: None,
                expires_at: None,
                scopes: vec![],
                active: true,
                metadata: serde_json::json!({"billing_mode": "api_usage_based"}),
                needs_reauth: false,
            },
            secret: StoredCredentialSecret {
                api_key: Some(key),
                ..Default::default()
            },
            display_summary: "Fake API key stored".into(),
        })
    }

    async fn refresh(
        &self,
        account: &StoredAccount,
        _ctx: AuthContext,
    ) -> Result<RefreshResult, AuthError> {
        let mut updated = account.clone();
        updated.last_refresh_at = Some(OffsetDateTime::now_utc());
        Ok(RefreshResult {
            account: updated,
            secret: StoredCredentialSecret {
                access_token: Some(SecretString::new("fake-refreshed-access".into())),
                ..Default::default()
            },
            refreshed: true,
        })
    }

    async fn logout(&self, _account_id: &AccountId, _ctx: AuthContext) -> Result<(), AuthError> {
        Ok(())
    }

    async fn status(
        &self,
        account: Option<&StoredAccount>,
        _ctx: AuthContext,
    ) -> Result<AuthStatus, AuthError> {
        Ok(match account {
            Some(a) => AuthStatus {
                provider: ProviderId::Fake,
                account: Some(a.clone()),
                source_label: "fake".into(),
                healthy: !a.needs_reauth,
                message: "Fake provider account configured".into(),
            },
            None => AuthStatus {
                provider: ProviderId::Fake,
                account: None,
                source_label: "none".into(),
                healthy: false,
                message: "No fake account; run /login fake --api-key.".into(),
            },
        })
    }
}

/// In-memory HTTP transport that simulates the OAuth endpoints of a
/// hypothetical fake provider. Used by demos and integration tests.
pub struct FakeOAuthHttpTransport {
    pub state: Arc<FakeProviderState>,
}

impl HttpTransport for FakeOAuthHttpTransport {
    fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, AuthError> {
        let fields = parse_form(body);
        if url.ends_with("/token") {
            // Authorization-code path?
            if fields.get("grant_type").map(|s| s.as_str()) == Some("authorization_code") {
                let code = fields.get("code").cloned().unwrap_or_default();
                let issued = self.state.issued_tokens.lock().unwrap();
                if let Some(rec) = issued.get(&code) {
                    let body = serde_json::json!({
                        "access_token": rec.access_token,
                        "refresh_token": rec.refresh_token,
                        "token_type": "bearer",
                        "expires_in": 3600,
                    })
                    .to_string();
                    return Ok(HttpResponse { status: 200, body });
                }
                let body = serde_json::json!({
                    "error": "invalid_grant",
                    "error_description": "fake server: unknown code",
                })
                .to_string();
                return Ok(HttpResponse { status: 400, body });
            }
            // Device-code path?
            if fields.get("grant_type").map(|s| s.as_str())
                == Some("urn:ietf:params:oauth:grant-type:device_code")
            {
                let device_code = fields.get("device_code").cloned().unwrap_or_default();
                let mut authorizations = self.state.device_authorizations.lock().unwrap();
                let Some(auth) = authorizations.get_mut(&device_code) else {
                    let body = serde_json::json!({
                        "error": "expired_token",
                    })
                    .to_string();
                    return Ok(HttpResponse { status: 400, body });
                };
                if *self.state.deny_next_device.lock().unwrap() {
                    *self.state.deny_next_device.lock().unwrap() = false;
                    let body = serde_json::json!({"error": "access_denied"}).to_string();
                    return Ok(HttpResponse { status: 400, body });
                }
                if !auth.authorized {
                    let body = serde_json::json!({"error": "authorization_pending"}).to_string();
                    return Ok(HttpResponse { status: 400, body });
                }
                let body = serde_json::json!({
                    "access_token": format!("fake-device-at-{}", auth.user_code),
                    "refresh_token": format!("fake-device-rt-{}", auth.user_code),
                    "token_type": "bearer",
                    "expires_in": 3600,
                })
                .to_string();
                return Ok(HttpResponse { status: 200, body });
            }
        } else if url.ends_with("/device") {
            // Issue a fresh device code/user code pair.
            let device_code = format!(
                "dc-{}",
                self.state.device_authorizations.lock().unwrap().len() + 1
            );
            let user_code = format!("USER-{}", device_code.len());
            self.state.device_authorizations.lock().unwrap().insert(
                device_code.clone(),
                FakeDeviceAuthorization {
                    user_code: user_code.clone(),
                    email: "fake@example.com".into(),
                    plan: "ChatGPT Plus".into(),
                    authorized: false,
                },
            );
            let body = serde_json::json!({
                "device_code": device_code,
                "user_code": user_code,
                "verification_uri": "https://fake.example.test/device",
                "verification_uri_complete": format!(
                    "https://fake.example.test/device?user_code={}",
                    user_code
                ),
                "expires_in": 600,
                "interval": 0,
            })
            .to_string();
            return Ok(HttpResponse { status: 200, body });
        }
        Ok(HttpResponse {
            status: 404,
            body: r#"{"error":"not_found"}"#.into(),
        })
    }
}

fn parse_form(body: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for part in body.split('&').filter(|s| !s.is_empty()) {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(decode(k), decode(v));
        }
    }
    out
}

fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '+' {
            out.push(' ');
        } else if c == '%' {
            let hex: String = (0..2).filter_map(|_| chars.next()).collect();
            if let Ok(b) = u8::from_str_radix(&hex, 16) {
                out.push(b as char);
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::browser::{
        finish_oauth_browser_login, start_oauth_browser_login, OAuthBrowserOptions,
        OAuthClientConfig, RedirectMode,
    };
    use crate::oauth::device::{
        device_code_poll, request_device_code, DeviceCodeClientConfig, DeviceCodePollOptions,
    };
    use crate::provider::RecordingBrowserOpener;

    #[tokio::test]
    // Fake API-key login: accepted key returns Ok; rejected key returns InvalidApiKey.
    async fn fake_api_key_login_accepts_and_rejects() {
        let state = Arc::new(FakeProviderState::new());
        state.allow_api_key("ok");
        let provider = FakeAuthProvider::new(state.clone());

        let ok = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "ok"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(ok.account.provider, ProviderId::Fake);

        let err = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "wrong"),
                AuthContext::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::InvalidApiKey);
    }

    #[test]
    // End-to-end fake OAuth browser flow returns the issued token.
    fn fake_browser_oauth_end_to_end() {
        let state = Arc::new(FakeProviderState::new());
        state.issue_token(
            "GOOD-CODE",
            FakeTokenRecord {
                email: "user@example.com".into(),
                plan: "Plus".into(),
                access_token: "AT".into(),
                refresh_token: "RT".into(),
                billing_mode: BillingMode::SubscriptionIncluded,
            },
        );
        let cfg = OAuthClientConfig {
            provider: ProviderId::Fake,
            client_id: "fake-client".into(),
            auth_url: "https://fake.example.test/authorize".into(),
            token_url: "https://fake.example.test/token".into(),
            scopes: vec!["openid".into()],
            redirect_mode: RedirectMode::Loopback {
                preferred_port: None,
                callback_path: "/cb".into(),
            },
            supports_pkce: true,
            extra_auth_params: Default::default(),
            extra_token_params: Default::default(),
        };
        let opener = Arc::new(RecordingBrowserOpener::default());
        let transport = FakeOAuthHttpTransport {
            state: state.clone(),
        };
        let (session, opened) = start_oauth_browser_login(&cfg, opener.clone()).unwrap();
        assert!(opened.is_ok());
        let s = session.state.expose().to_string();
        let opts = OAuthBrowserOptions {
            force_callback_query: Some(format!("code=GOOD-CODE&state={}", s)),
            ..Default::default()
        };
        let outcome = finish_oauth_browser_login(session, &cfg, &transport, opts).unwrap();
        assert_eq!(outcome.token.access_token, "AT");
        assert_eq!(outcome.token.refresh_token.as_deref(), Some("RT"));
    }

    #[test]
    // End-to-end fake device-code flow: request -> approve -> poll yields a token.
    fn fake_device_code_end_to_end() {
        let state = Arc::new(FakeProviderState::new());
        let transport = FakeOAuthHttpTransport {
            state: state.clone(),
        };
        let cfg = DeviceCodeClientConfig {
            provider: ProviderId::Fake,
            client_id: "fake-client".into(),
            device_authorization_url: "https://fake.example.test/device".into(),
            token_url: "https://fake.example.test/token".into(),
            scopes: vec![],
        };
        let login = request_device_code(&cfg, &transport).unwrap();
        assert!(!login.user_code.is_empty());
        // Approve the only device code in state.
        let codes: Vec<String> = state
            .device_authorizations
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(codes.len(), 1);
        state.approve_device(&codes[0]);
        let outcome = device_code_poll(
            &cfg,
            &login,
            &transport,
            DeviceCodePollOptions {
                max_polls: Some(2),
                sleeper: None,
            },
        )
        .unwrap();
        assert!(outcome.token.access_token.starts_with("fake-device-at-"));
    }
}
