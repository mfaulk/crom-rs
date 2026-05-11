//! Shared token-endpoint types.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthErrorKind};

/// Token-endpoint response. Tolerates either standard OAuth 2.0 fields
/// (`access_token`, `refresh_token`, `expires_in`) or fields commonly
/// emitted by AI-platform identity providers (e.g. `id_token`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Standard OAuth 2.0 error envelope.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenError {
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub error_description: Option<String>,
}

impl TokenError {
    pub fn into_auth_error(self, fallback_kind: AuthErrorKind) -> AuthError {
        let kind = match self.error.as_str() {
            "authorization_pending" | "slow_down" => fallback_kind,
            "expired_token" => AuthErrorKind::DeviceCodeExpired,
            "access_denied" => AuthErrorKind::DeviceCodeDenied,
            "invalid_grant" => AuthErrorKind::OAuthCodeExchangeFailed,
            "invalid_request" | "invalid_client" => AuthErrorKind::OAuthCodeExchangeFailed,
            _ => fallback_kind,
        };
        let msg = self.error_description.unwrap_or_else(|| self.error.clone());
        AuthError::new(kind, msg)
    }
}

/// Form payload for an authorization-code-grant or refresh-token request.
#[derive(Debug, Clone, Default)]
pub struct TokenForm {
    pub fields: Vec<(String, String)>,
}

impl TokenForm {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(mut self, k: &str, v: impl Into<String>) -> Self {
        self.fields.push((k.into(), v.into()));
        self
    }

    pub fn extend(mut self, extras: BTreeMap<String, String>) -> Self {
        for (k, v) in extras {
            self.fields.push((k, v));
        }
        self
    }

    /// `application/x-www-form-urlencoded` body.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        for (i, (k, v)) in self.fields.iter().enumerate() {
            if i > 0 {
                out.push('&');
            }
            url_encode_into(k, &mut out);
            out.push('=');
            url_encode_into(v, &mut out);
        }
        out
    }
}

pub(crate) fn url_encode_into(s: &str, out: &mut String) {
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            other => {
                out.push('%');
                out.push_str(&format!("{:02X}", other));
            }
        }
    }
}

pub fn url_encode(s: &str) -> String {
    let mut out = String::new();
    url_encode_into(s, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // TokenForm encodes RFC 3986 reserved chars and uses `+` for spaces.
    fn token_form_encodes_reserved() {
        let f = TokenForm::new().add("a b", "c&d=e");
        assert_eq!(f.encode(), "a+b=c%26d%3De");
    }

    #[test]
    // TokenError maps known error codes to correct AuthErrorKind values.
    fn token_error_maps_to_auth_kind() {
        let e = TokenError {
            error: "expired_token".into(),
            error_description: Some("expired".into()),
        };
        let auth = e.into_auth_error(AuthErrorKind::OAuthCodeExchangeFailed);
        assert_eq!(auth.kind, AuthErrorKind::DeviceCodeExpired);
        assert!(auth.message.contains("expired"));
    }

    #[test]
    // url_encode keeps unreserved chars verbatim and percent-encodes the rest.
    fn url_encode_handles_unreserved_and_reserved() {
        assert_eq!(url_encode("foo-bar.baz~123"), "foo-bar.baz~123");
        assert_eq!(url_encode("a/b?c"), "a%2Fb%3Fc");
    }
}
