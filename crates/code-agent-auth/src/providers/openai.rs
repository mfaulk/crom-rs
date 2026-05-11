//! OpenAI Platform API-key auth provider.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::provider::{AuthContext, AuthProvider};
use crate::types::{
    AccountId, AuthStatus, LoginMethod, LoginMethodKind, LoginRequest, LoginResult, ProviderId,
    RefreshResult, StoredAccount, StoredCredentialSecret,
};

pub struct OpenAiAuthProvider;

#[async_trait]
impl AuthProvider for OpenAiAuthProvider {
    fn id(&self) -> ProviderId {
        ProviderId::OpenAi
    }

    fn display_name(&self) -> &'static str {
        "OpenAI Platform"
    }

    fn supported_login_methods(&self) -> Vec<LoginMethod> {
        vec![LoginMethod::ApiKey]
    }

    async fn login(
        &self,
        request: LoginRequest,
        _ctx: AuthContext,
    ) -> Result<LoginResult, AuthError> {
        let key = request.api_key.ok_or_else(|| {
            AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "OpenAI login requires an API key (use --api-key or pipe via --api-key-stdin).",
            )
        })?;
        if key.expose().trim().is_empty() {
            return Err(AuthError::new(
                AuthErrorKind::InvalidApiKey,
                "API key is empty.",
            ));
        }
        let account = StoredAccount {
            account_id: AccountId::new("openai-api-key"),
            provider: ProviderId::OpenAi,
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
            display_summary: "OpenAI API key stored".into(),
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
                provider: ProviderId::OpenAi,
                account: Some(a.clone()),
                source_label: "openai_api_key".into(),
                healthy: !a.needs_reauth,
                message: "OpenAI API key configured".into(),
            },
            None => AuthStatus {
                provider: ProviderId::OpenAi,
                account: None,
                source_label: "none".into(),
                healthy: false,
                message: "No OpenAI credential. Use /login openai --api-key.".into(),
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
        let provider = OpenAiAuthProvider;
        let result = provider
            .login(
                LoginRequest::api_key(LoginMethodKind::ApiKey, "sk-test-openai"),
                AuthContext::default(),
            )
            .await
            .unwrap();
        assert_eq!(result.account.provider, ProviderId::OpenAi);
        assert_eq!(
            result.secret.api_key.as_ref().unwrap().expose(),
            "sk-test-openai"
        );
    }

    #[test]
    // OpenAI exposes only ApiKey as a supported method (no first-party OAuth here).
    fn openai_only_advertises_api_key() {
        let provider = OpenAiAuthProvider;
        let methods = provider.supported_login_methods();
        assert_eq!(methods.len(), 1);
        assert!(matches!(methods[0], LoginMethod::ApiKey));
    }
}
