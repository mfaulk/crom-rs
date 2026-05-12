//! Shared token-endpoint types.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthErrorKind};

/// Token-endpoint response. Tolerates either standard OAuth 2.0 fields
/// (`access_token`, `refresh_token`, `expires_in`) or fields commonly
/// emitted by AI-platform identity providers (e.g. `id_token`).
///
/// `Debug` is implemented by hand because deriving it would dump the
/// raw `access_token` / `refresh_token` / `id_token` into any
/// `tracing::debug!` or `{:?}` format string, including indirectly
/// through containing types like `DeviceCodeOutcome` or
/// `BrowserLoginOutcome`.
#[derive(Clone, Default, Serialize, Deserialize)]
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

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact every bearer-equivalent field. The metadata fields
        // (`token_type`, `expires_in`, `scope`) are safe to surface and
        // useful for diagnostics.
        f.debug_struct("TokenResponse")
            .field("access_token", &"***")
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("id_token", &self.id_token.as_ref().map(|_| "***"))
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
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

/// Decode an `application/x-www-form-urlencoded` string.
///
/// Inverse of [`url_encode`] for well-formed input. Malformed percent escapes
/// (`%` without two following hex digits) are preserved as the literal `%`
/// plus any consumed chars, rather than silently dropped, so callers see
/// exactly what arrived on the wire. Multi-byte UTF-8 sequences are
/// reassembled across percent escapes before the bytes are decoded as a
/// string.
pub(crate) fn url_decode(s: &str) -> String {
    fn push_char(out: &mut Vec<u8>, c: char) {
        let mut buf = [0u8; 4];
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }

    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '+' => out.push(b' '),
            '%' => {
                // Take two hex digits if and only if both are present and
                // valid; otherwise emit the literal `%` and let the loop
                // re-process the following chars normally.
                let h = chars.peek().and_then(|c| c.to_digit(16));
                if let Some(h) = h {
                    // Tentatively consume the first hex digit.
                    let h_char = chars.next().expect("peeked");
                    let l = chars.peek().and_then(|c| c.to_digit(16));
                    match l {
                        Some(l) => {
                            // Commit: consume the second digit too.
                            chars.next();
                            out.push(((h << 4) | l) as u8);
                        }
                        None => {
                            // Roll back: emit `%` and the first char literally.
                            out.push(b'%');
                            push_char(&mut out, h_char);
                        }
                    }
                } else {
                    out.push(b'%');
                }
            }
            other => push_char(&mut out, other),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    #[test]
    // {:?} on a TokenResponse must never reveal the raw access /
    // refresh / id tokens, because tracing::debug! and accidental
    // log statements would otherwise dump live bearer secrets.
    fn token_response_debug_redacts_bearer_fields() {
        let tr = TokenResponse {
            access_token: "secret-access-123".into(),
            refresh_token: Some("secret-refresh-456".into()),
            id_token: Some("secret-id-789".into()),
            token_type: Some("Bearer".into()),
            expires_in: Some(3600),
            scope: Some("read write".into()),
        };
        let dbg = format!("{:?}", tr);
        assert!(
            !dbg.contains("secret-access-123"),
            "access_token leaked in Debug: {dbg}"
        );
        assert!(
            !dbg.contains("secret-refresh-456"),
            "refresh_token leaked in Debug: {dbg}"
        );
        assert!(
            !dbg.contains("secret-id-789"),
            "id_token leaked in Debug: {dbg}"
        );
        // Metadata fields are still legible — they are not secrets and
        // are useful for diagnosing token-endpoint problems.
        assert!(dbg.contains("Bearer"));
        assert!(dbg.contains("3600"));
        assert!(dbg.contains("read write"));
    }

    #[test]
    // Debug redaction transitively holds when TokenResponse is wrapped
    // in a containing type that derives Debug, such as
    // DeviceCodeOutcome / BrowserLoginOutcome.
    fn token_response_debug_redacts_through_container() {
        use crate::oauth::device::DeviceCodeOutcome;
        let outcome = DeviceCodeOutcome {
            token: TokenResponse {
                access_token: "container-secret".into(),
                ..Default::default()
            },
        };
        let dbg = format!("{:?}", outcome);
        assert!(
            !dbg.contains("container-secret"),
            "access_token leaked through container: {dbg}"
        );
    }
}
