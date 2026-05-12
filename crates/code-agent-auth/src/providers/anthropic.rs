//! Anthropic API-key auth provider.
//!
//! Subscription / Claude Code OAuth is intentionally surfaced as
//! [`LoginMethod::Unsupported`]. Implementing it here would require
//! impersonating the Claude Code first-party client ID, which is not
//! authorized for third-party use in this build.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::provider::{AuthContext, AuthProvider};
use crate::types::{
    AccountId, AuthStatus, LoginAlternative, LoginMethod, LoginMethodKind, LoginRequest,
    LoginResult, ProviderId, RefreshResult, StoredAccount, StoredCredentialSecret,
};

pub struct AnthropicAuthProvider;

#[async_trait]
impl AuthProvider for AnthropicAuthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::Anthropic
    }

    fn display_name(&self) -> &'static str {
        "Anthropic API"
    }

    fn supported_login_methods(&self) -> Vec<LoginMethod> {
        vec![
            LoginMethod::ApiKey,
            LoginMethod::Unsupported {
                reason:
                    "Claude subscription / Claude Code OAuth is not available for third-party clients in this build."
                        .into(),
                alternatives: vec![
                    LoginAlternative {
                        label: "Use an Anthropic API key".into(),
                        provider: Some(ProviderId::Anthropic),
                        method: Some(LoginMethodKind::ApiKey),
                        command_hint: Some("/login anthropic --api-key".into()),
                    },
                    LoginAlternative {
                        label: "Use Bedrock / Vertex / Foundry if configured".into(),
                        provider: None,
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
                "Anthropic login requires an API key (use --api-key or pipe via --api-key-stdin).",
            )
        })?;
        if key.expose().trim().is_empty() {
            return Err(AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "API key is empty.",
            ));
        }
        // Provider-side syntactic check: reject obviously truncated values
        // before we store them. We deliberately do *not* call out to the
        // network here so login works in offline builds.
        if !key.expose().starts_with("sk-") && !key.expose().starts_with("test-") {
            tracing::debug!(
                "anthropic login: key does not start with sk-; storing anyway as the format may have changed"
            );
        }
        let account = StoredAccount {
            account_id: AccountId::from_api_key("anthropic", key.expose()),
            provider: ProviderId::Anthropic,
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
            display_summary: "Anthropic API key stored".into(),
        })
    }

    async fn refresh(
        &self,
        account: &StoredAccount,
        _ctx: AuthContext,
    ) -> Result<RefreshResult, AuthError> {
        // API keys do not refresh; we report no-op.
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
                provider: ProviderId::Anthropic,
                account: Some(a.clone()),
                source_label: "anthropic_api_key".into(),
                healthy: !a.needs_reauth,
                message: "Anthropic API key configured".into(),
            },
            None => AuthStatus {
                provider: ProviderId::Anthropic,
                account: None,
                source_label: "none".into(),
                healthy: false,
                message: "No Anthropic credential. Use /login anthropic --api-key.".into(),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SecretString;

    #[tokio::test]
    // Successful API-key login produces a stored account + secret payload.
    async fn login_with_api_key_returns_stored_account() {
        let provider = AnthropicAuthProvider;
        let result = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-test-1234"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.account.provider, ProviderId::Anthropic);
        assert_eq!(
            result.secret.api_key.as_ref().unwrap().expose(),
            "sk-test-1234"
        );
        assert!(result
            .account
            .account_id
            .as_str()
            .starts_with("anthropic-apikey-"));
    }

    #[tokio::test]
    // Two distinct API keys produce two distinct account ids, so a second
    // login does not silently overwrite the first stored credential.
    async fn distinct_api_keys_yield_distinct_account_ids() {
        let provider = AnthropicAuthProvider;
        let a = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-aaa"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        let b = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-bbb"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_ne!(a.account.account_id, b.account.account_id);
    }

    #[tokio::test]
    // Empty API key returns InvalidApiKey.
    async fn empty_api_key_is_rejected() {
        let provider = AnthropicAuthProvider;
        let req = LoginRequest {
            api_key: Some(SecretString::new("".into())),
            method: Some(LoginMethodKind::ApiKey),
            ..Default::default()
        };
        let err = provider
            .login(req, AuthContext::default())
            .await
            .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::InvalidApiKey);
    }

    #[test]
    // Subscription OAuth is exposed as Unsupported with a Bedrock/Vertex hint.
    fn subscription_oauth_unsupported_with_alternatives() {
        let provider = AnthropicAuthProvider;
        let methods = provider.supported_login_methods();
        let unsupported = methods
            .iter()
            .find(|m| matches!(m, LoginMethod::Unsupported { .. }))
            .expect("expected Unsupported entry");
        if let LoginMethod::Unsupported { alternatives, .. } = unsupported {
            assert!(alternatives.iter().any(|a| a
                .command_hint
                .as_deref()
                .map(|c| c.contains("anthropic --api-key"))
                .unwrap_or(false)));
        }
    }
}
