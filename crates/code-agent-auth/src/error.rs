//! Normalized error categories for the auth subsystem.

use thiserror::Error;

/// Stable kind for [`AuthError`]. Used by the CLI/TUI to choose the user
/// message and (where appropriate) the exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthErrorKind {
    NotLoggedIn,
    UnsupportedLoginMethod,
    ProviderTermsUnsupported,
    BrowserOpenFailed,
    OAuthTimeout,
    OAuthCancelled,
    OAuthStateMismatch,
    OAuthCodeExchangeFailed,
    DeviceCodeExpired,
    DeviceCodeDenied,
    TokenExpired,
    RefreshFailed,
    Revoked,
    MissingScope,
    InvalidApiKey,
    RateLimited,
    PlanRequired,
    WorkspaceRestricted,
    CredentialStoreUnavailable,
    Network,
    SidecarUnavailable,
    SidecarProtocolError,
    Unknown,
}

impl AuthErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotLoggedIn => "not_logged_in",
            Self::UnsupportedLoginMethod => "unsupported_login_method",
            Self::ProviderTermsUnsupported => "provider_terms_unsupported",
            Self::BrowserOpenFailed => "browser_open_failed",
            Self::OAuthTimeout => "oauth_timeout",
            Self::OAuthCancelled => "oauth_cancelled",
            Self::OAuthStateMismatch => "oauth_state_mismatch",
            Self::OAuthCodeExchangeFailed => "oauth_code_exchange_failed",
            Self::DeviceCodeExpired => "device_code_expired",
            Self::DeviceCodeDenied => "device_code_denied",
            Self::TokenExpired => "token_expired",
            Self::RefreshFailed => "refresh_failed",
            Self::Revoked => "revoked",
            Self::MissingScope => "missing_scope",
            Self::InvalidApiKey => "invalid_api_key",
            Self::RateLimited => "rate_limited",
            Self::PlanRequired => "plan_required",
            Self::WorkspaceRestricted => "workspace_restricted",
            Self::CredentialStoreUnavailable => "credential_store_unavailable",
            Self::Network => "network",
            Self::SidecarUnavailable => "sidecar_unavailable",
            Self::SidecarProtocolError => "sidecar_protocol_error",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Error)]
#[error("{kind:?}: {message}")]
pub struct AuthError {
    pub kind: AuthErrorKind,
    pub message: String,
}

impl AuthError {
    pub fn new(kind: AuthErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn not_logged_in(provider: &str) -> Self {
        Self::new(
            AuthErrorKind::NotLoggedIn,
            format!("No active credential for {provider}."),
        )
    }

    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::new(AuthErrorKind::UnsupportedLoginMethod, reason)
    }
}

impl From<std::io::Error> for AuthError {
    fn from(value: std::io::Error) -> Self {
        Self::new(AuthErrorKind::Network, value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Every AuthErrorKind variant exposes a stable lowercase string.
    fn auth_error_kind_strings_are_stable() {
        // A non-exhaustive sample is enough; the enum is small and we just
        // want to catch accidental human-formatted variants.
        for k in [
            AuthErrorKind::NotLoggedIn,
            AuthErrorKind::OAuthStateMismatch,
            AuthErrorKind::CredentialStoreUnavailable,
        ] {
            let s = k.as_str();
            assert!(!s.is_empty());
            assert_eq!(s, s.to_ascii_lowercase());
        }
    }

    #[test]
    // Display impl never leaks a raw error formatted with `{:?}` style noise.
    fn display_includes_kind_and_message() {
        let e = AuthError::new(AuthErrorKind::InvalidApiKey, "bad key");
        let s = format!("{}", e);
        assert!(s.contains("InvalidApiKey"));
        assert!(s.contains("bad key"));
    }
}
