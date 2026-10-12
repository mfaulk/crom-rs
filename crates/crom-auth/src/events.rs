//! Auth-subsystem event records. These are returned to the host (CLI/TUI)
//! which is responsible for forwarding them through the central
//! `AgentEvent` stream. Keeping the data types here means the host crate
//! does not have to know how to construct them.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::AuthErrorKind;
use crate::types::{
    AccountId, CredentialSecretKind, CredentialStoreMode, LoginMethodKind, ProviderId,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderRegistryLoaded {
    pub providers: Vec<ProviderId>,
    #[serde(with = "time::serde::rfc3339")]
    pub timestamp: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginStarted {
    pub provider: ProviderId,
    pub method: LoginMethodKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginBrowserOpened {
    pub provider: ProviderId,
    /// Redacted summary; never the full URL with state/PKCE params.
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginDeviceCodeDisplayed {
    pub provider: ProviderId,
    pub user_code_redacted: String,
    pub verification_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginCompleted {
    pub provider: ProviderId,
    pub method: LoginMethodKind,
    pub account_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_account: Option<String>,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginFailed {
    pub provider: ProviderId,
    pub method: Option<LoginMethodKind>,
    pub kind: AuthErrorKind,
    pub message: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogoutCompleted {
    pub provider: ProviderId,
    pub account_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialStored {
    pub provider: ProviderId,
    pub account_hash: String,
    pub store: CredentialStoreMode,
    pub secret_kind: CredentialSecretKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRefreshed {
    pub provider: ProviderId,
    pub account_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CredentialRefreshFailed {
    pub provider: ProviderId,
    pub account_hash: String,
    pub kind: AuthErrorKind,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthProviderSelected {
    pub provider: ProviderId,
    pub account_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSelected {
    pub provider: ProviderId,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelListLoaded {
    pub provider: ProviderId,
    pub model_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthStatusChecked {
    pub provider: ProviderId,
    pub healthy: bool,
}

/// Convenience helper: record both the redacted account hash and the
/// `display_account` if one is known.
pub fn account_summary(
    account: Option<&AccountId>,
    display: Option<&str>,
) -> (String, Option<String>) {
    let hash = account
        .map(|a| a.redacted_hash())
        .unwrap_or_else(|| "anonymous".into());
    (hash, display.map(|s| s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // account_summary always emits a stable hash and lifts the optional display value.
    fn account_summary_emits_hash_and_display() {
        let aid = AccountId::new("user@example.com");
        let (hash, display) = account_summary(Some(&aid), Some("user@example.com"));
        assert!(hash.starts_with("acct_"));
        assert_eq!(display.as_deref(), Some("user@example.com"));
    }

    #[test]
    // Login event types serialize without leaking the secret payload.
    fn login_completed_serializes_without_secrets() {
        let ev = LoginCompleted {
            provider: ProviderId::OpenAi,
            method: LoginMethodKind::ApiKey,
            account_hash: "acct_xxx".into(),
            display_account: Some("user@example.com".into()),
            duration_ms: 12,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert!(s.contains("openai"));
        assert!(s.contains("acct_xxx"));
        assert!(!s.contains("secret"));
    }
}
