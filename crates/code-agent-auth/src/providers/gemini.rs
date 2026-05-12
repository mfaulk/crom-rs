//! Google AI Studio (Gemini) API-key auth provider.
//!
//! Gemini subscription / Gemini CLI OAuth is intentionally surfaced as
//! [`LoginMethod::Unsupported`]. Re-using Gemini CLI's first-party OAuth
//! client for a third-party coding agent is not authorized in this build.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::provider::{AuthContext, AuthProvider};
use crate::types::{
    AccountId, AuthStatus, LoginAlternative, LoginMethod, LoginMethodKind, LoginRequest,
    LoginResult, ProviderId, RefreshResult, StoredAccount, StoredCredentialSecret,
};

pub struct GeminiAuthProvider;

#[async_trait]
impl AuthProvider for GeminiAuthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Gemini
    }

    fn display_name(&self) -> &'static str {
        "Google AI Studio (Gemini)"
    }

    fn supported_login_methods(&self) -> Vec<LoginMethod> {
        vec![
            LoginMethod::ApiKey,
            LoginMethod::Unsupported {
                reason: "Gemini subscription / Gemini CLI OAuth is not supported for third-party coding agents in this build."
                    .into(),
                alternatives: vec![
                    LoginAlternative {
                        label: "Use a Gemini API key".into(),
                        provider: Some(ProviderId::Gemini),
                        method: Some(LoginMethodKind::ApiKey),
                        command_hint: Some("/login gemini --api-key".into()),
                    },
                    LoginAlternative {
                        label: "Configure Vertex AI if implemented".into(),
                        provider: Some(ProviderId::VertexAi),
                        method: None,
                        command_hint: None,
                    },
                ],
            },
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
                "Gemini login requires an API key (use --api-key or pipe via --api-key-stdin).",
            )
        })?;
        if key.expose().trim().is_empty() {
            return Err(AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "API key is empty.",
            ));
        }
        let account = StoredAccount {
            account_id: AccountId::from_api_key("gemini", key.expose()),
            provider: ProviderId::Gemini,
            login_method: LoginMethodKind::ApiKey,
            display_email: None,
            workspace_id: None,
            plan_type: None,
            created_at: OffsetDateTime::now_utc(),
            last_refresh_at: None,
            expires_at: None,
            scopes: vec![],
            active: true,
            metadata: serde_json::json!({}),
            needs_reauth: false,
        };
        let secret = StoredCredentialSecret {
            api_key: Some(key),
            ..Default::default()
        };
        Ok(LoginResult {
            account,
            secret,
            display_summary: "Gemini API key stored".into(),
        })
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
                provider: ProviderId::Gemini,
                account: Some(a.clone()),
                source_label: "gemini_api_key".into(),
                healthy: !a.needs_reauth,
                message: "Gemini API key configured".into(),
            },
            None => AuthStatus {
                provider: ProviderId::Gemini,
                account: None,
                source_label: "none".into(),
                healthy: false,
                message: "No Gemini credential. Use /login gemini --api-key.".into(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    // Successful API-key login produces a stored account + secret payload.
    async fn login_with_api_key_returns_stored_account() {
        let provider = GeminiAuthProvider;
        let result = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "AI-test-gemini"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.account.provider, ProviderId::Gemini);
        assert_eq!(
            result.secret.api_key.as_ref().unwrap().expose(),
            "AI-test-gemini"
        );
    }

    #[tokio::test]
    // Two distinct Gemini API keys produce two distinct synthetic account
    // ids so a second login does not silently overwrite the first.
    async fn distinct_api_keys_yield_distinct_account_ids() {
        let provider = GeminiAuthProvider;
        let a = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "AI-gemini-a"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        let b = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "AI-gemini-b"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_ne!(a.account.account_id, b.account.account_id);
        assert!(a.account.account_id.as_str().starts_with("gemini-apikey-"));
    }

    #[test]
    // Subscription OAuth is exposed as Unsupported with a Vertex hint.
    fn subscription_oauth_unsupported_with_alternatives() {
        let provider = GeminiAuthProvider;
        let methods = provider.supported_login_methods();
        let unsupported = methods
            .iter()
            .find(|m| matches!(m, LoginMethod::Unsupported { .. }))
            .expect("expected Unsupported entry");
        if let LoginMethod::Unsupported {
            alternatives,
            reason,
        } = unsupported
        {
            assert!(reason.contains("third-party"));
            assert!(alternatives
                .iter()
                .any(|a| matches!(a.provider, Some(ProviderId::VertexAi))));
        }
    }
}
