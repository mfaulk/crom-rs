//! Codex / ChatGPT subscription auth provider.
//!
//! Out-of-the-box this provider operates in **feature-gated unsupported**
//! mode: it acknowledges that Codex subscription auth exists, but since
//! we do not have an authorized integration path with the upstream
//! provider, attempts to log in via the subscription flow surface a
//! clear `Unsupported` status. Users who do have an authorized adapter
//! configured in their settings can flip [`CodexAuthMode`] to
//! `UserSuppliedOAuth` or `ChatGptManagedSidecar` to opt in.
//!
//! The OpenAI API-key path is always available because it's the
//! documented, third-party-friendly way to use Codex models.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::provider::{AuthContext, AuthProvider};
use crate::types::{
    AccountId, AuthStatus, LoginAlternative, LoginMethod, LoginMethodKind, LoginRequest,
    LoginResult, ProviderId, RefreshResult, StoredAccount, StoredCredentialSecret,
};

/// Active Codex auth mode. Defaults to [`CodexAuthMode::Unsupported`] for
/// the subscription path; users can opt in to one of the authorized
/// integrations explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodexAuthMode {
    OpenAiApiKey,
    ChatGptManagedSidecar,
    UserSuppliedOAuth,
    #[default]
    Unsupported,
}

pub struct CodexAuthProvider {
    mode: CodexAuthMode,
}

impl CodexAuthProvider {
    pub fn new(mode: CodexAuthMode) -> Self {
        Self { mode }
    }

    pub fn mode(&self) -> CodexAuthMode {
        self.mode
    }
}

impl Default for CodexAuthProvider {
    fn default() -> Self {
        Self::new(CodexAuthMode::Unsupported)
    }
}

#[async_trait]
impl AuthProvider for CodexAuthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Codex
    }

    fn display_name(&self) -> &'static str {
        "Codex / ChatGPT"
    }

    fn supported_login_methods(&self) -> Vec<LoginMethod> {
        let mut methods = vec![LoginMethod::ApiKey];
        match self.mode {
            CodexAuthMode::ChatGptManagedSidecar => {
                methods.push(LoginMethod::ExternalCliManaged);
            }
            CodexAuthMode::UserSuppliedOAuth => {
                methods.push(LoginMethod::OAuthBrowser);
                methods.push(LoginMethod::OAuthDeviceCode);
            }
            CodexAuthMode::OpenAiApiKey => {}
            CodexAuthMode::Unsupported => {
                methods.push(LoginMethod::Unsupported {
                    reason:
                        "Codex / ChatGPT subscription login is feature-gated in this build. Configure auth.providers.codex.allowed_methods to include `chatgpt_managed` (with an authorized sidecar) or `user_supplied_oauth` to enable it."
                            .into(),
                    alternatives: vec![
                        LoginAlternative {
                            label: "Use an OpenAI API key".into(),
                            provider: Some(ProviderId::Codex),
                            method: Some(LoginMethodKind::ApiKey),
                            command_hint: Some("/login codex --api-key".into()),
                        },
                    ],
                });
            }
        }
        methods
    }

    async fn login(
        &self,
        request: LoginRequest,
        _ctx: AuthContext,
    ) -> Result<LoginResult, AuthError> {
        // API-key path always works.
        if matches!(request.method, Some(LoginMethodKind::ApiKey)) || request.api_key.is_some() {
            let key = request.api_key.ok_or_else(|| {
                AuthError::new(
                    AuthErrorKind::InvalidApiKey,
                    "Codex API-key login requires an API key.",
                )
            })?;
            if key.expose().trim().is_empty() {
                return Err(AuthError::new(
                    AuthErrorKind::InvalidApiKey,
                    "API key is empty.",
                ));
            }
            let account = StoredAccount {
                account_id: AccountId::from_api_key("codex", key.expose()),
                provider: ProviderId::Codex,
                login_method: LoginMethodKind::ApiKey,
                display_email: None,
                workspace_id: None,
                plan_type: Some("api".into()),
                created_at: OffsetDateTime::now_utc(),
                last_refresh_at: None,
                expires_at: None,
                scopes: vec![],
                active: true,
                metadata: serde_json::json!({"codex_mode": "api_key"}),
                needs_reauth: false,
            };
            let secret = StoredCredentialSecret {
                api_key: Some(key),
                ..Default::default()
            };
            return Ok(LoginResult {
                account,
                secret,
                display_summary: "Codex API key stored".into(),
            });
        }

        match self.mode {
            CodexAuthMode::ChatGptManagedSidecar => Err(AuthError::new(
                AuthErrorKind::SidecarUnavailable,
                "ChatGPT managed sidecar is not configured. Provide a sidecar process implementing the documented protocol and re-run.",
            )),
            CodexAuthMode::UserSuppliedOAuth => Err(AuthError::new(
                AuthErrorKind::UnsupportedLoginMethod,
                "User-supplied OAuth requires `auth.providers.codex.user_oauth` configuration. The CLI does not embed first-party Codex client IDs.",
            )),
            CodexAuthMode::OpenAiApiKey | CodexAuthMode::Unsupported => Err(AuthError::new(
                AuthErrorKind::UnsupportedLoginMethod,
                "Codex subscription login is not enabled in this build. Use OpenAI API key login, or configure an authorized Codex auth adapter.",
            )),
        }
    }

    async fn refresh(
        &self,
        account: &StoredAccount,
        _ctx: AuthContext,
    ) -> Result<RefreshResult, AuthError> {
        Ok(RefreshResult {
            account: account.clone(),
            secret: StoredCredentialSecret::default(),
            refreshed: false,
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
                provider: ProviderId::Codex,
                account: Some(a.clone()),
                source_label: format!("codex_{}", a.login_method.as_str()),
                healthy: !a.needs_reauth,
                message: format!("Codex {} configured", a.login_method.as_str()),
            },
            None => AuthStatus {
                provider: ProviderId::Codex,
                account: None,
                source_label: "none".into(),
                healthy: false,
                message:
                    "No Codex credential. Use /login codex --api-key, or configure an authorized adapter."
                        .into(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    // Codex API-key login path is always available, regardless of mode.
    async fn api_key_login_succeeds_in_default_mode() {
        let provider = CodexAuthProvider::default();
        let req = LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-codex-1");
        let result = provider.login(req, AuthContext::default()).await.unwrap();
        assert_eq!(result.account.provider, ProviderId::Codex);
    }

    #[tokio::test]
    // Two distinct Codex API keys produce two distinct synthetic account
    // ids so a second login does not silently overwrite the first.
    async fn distinct_api_keys_yield_distinct_account_ids() {
        let provider = CodexAuthProvider::default();
        let a = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-codex-a"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        let b = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-codex-b"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_ne!(a.account.account_id, b.account.account_id);
        assert!(a.account.account_id.as_str().starts_with("codex-apikey-"));
    }

    #[tokio::test]
    // Subscription path in unsupported mode returns UnsupportedLoginMethod.
    async fn subscription_login_unsupported_by_default() {
        let provider = CodexAuthProvider::default();
        let req = LoginRequest {
            method: Some(LoginMethodKind::OAuthBrowser),
            api_key: None,
            params: Default::default(),
        };
        let err = provider
            .login(req, AuthContext::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::UnsupportedLoginMethod);
        assert!(err.message.contains("authorized Codex auth adapter"));
    }

    #[test]
    // Unsupported mode advertises an Unsupported method with an alternative.
    fn unsupported_mode_emits_unsupported_method() {
        let provider = CodexAuthProvider::default();
        let methods = provider.supported_login_methods();
        assert!(methods
            .iter()
            .any(|m| matches!(m, LoginMethod::Unsupported { .. })));
    }

    #[test]
    // ChatGptManagedSidecar mode advertises ExternalCliManaged.
    fn sidecar_mode_advertises_external() {
        let provider = CodexAuthProvider::new(CodexAuthMode::ChatGptManagedSidecar);
        let methods = provider.supported_login_methods();
        assert!(methods
            .iter()
            .any(|m| matches!(m, LoginMethod::ExternalCliManaged)));
    }

    #[tokio::test]
    // ChatGptManagedSidecar without a sidecar configured returns SidecarUnavailable.
    async fn sidecar_login_without_sidecar_returns_unavailable() {
        let provider = CodexAuthProvider::new(CodexAuthMode::ChatGptManagedSidecar);
        let req = LoginRequest {
            method: Some(LoginMethodKind::ExternalCliManaged),
            api_key: None,
            params: Default::default(),
        };
        let err = provider
            .login(req, AuthContext::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::SidecarUnavailable);
    }
}
