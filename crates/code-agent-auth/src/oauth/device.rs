//! OAuth 2.0 device-code flow (RFC 8628).
//!
//! Like the browser flow, the HTTP transport is abstracted via
//! [`crate::oauth::browser::HttpTransport`] so unit tests can run without
//! a real socket.

use std::time::Duration;

use serde::Deserialize;
use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::oauth::browser::{HttpResponse, HttpTransport};
use crate::oauth::token::{TokenError, TokenForm, TokenResponse};
use crate::types::ProviderId;

#[derive(Debug, Clone)]
pub struct DeviceCodeClientConfig {
    pub provider: ProviderId,
    pub client_id: String,
    pub device_authorization_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: i64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// What the caller should display to the user before waiting on the
/// token endpoint. Stays free of secrets — `device_code` is intentionally
/// not exposed here.
#[derive(Debug, Clone)]
pub struct DeviceCodeLogin {
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub user_code: String,
    pub expires_at: OffsetDateTime,
    pub interval: Duration,
    pub(crate) device_code: String,
}

#[derive(Debug, Clone)]
pub struct DeviceCodeOutcome {
    pub token: TokenResponse,
}

/// Errors that can occur while polling the token endpoint. Maps the
/// RFC 8628 error codes to a stable enum so callers don't have to grep
/// strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceCodeTokenError {
    AuthorizationPending,
    SlowDown,
    ExpiredToken,
    AccessDenied,
    Other,
}

impl DeviceCodeTokenError {
    pub fn from_code(code: &str) -> Self {
        match code {
            "authorization_pending" => Self::AuthorizationPending,
            "slow_down" => Self::SlowDown,
            "expired_token" => Self::ExpiredToken,
            "access_denied" => Self::AccessDenied,
            _ => Self::Other,
        }
    }
}

/// Phase 1: ask the device-authorization endpoint for a device + user code.
pub fn request_device_code(
    cfg: &DeviceCodeClientConfig,
    transport: &dyn HttpTransport,
) -> Result<DeviceCodeLogin, AuthError> {
    let mut form = TokenForm::new().add("client_id", cfg.client_id.clone());
    if !cfg.scopes.is_empty() {
        form = form.add("scope", cfg.scopes.join(" "));
    }
    let response = transport.post_form(&cfg.device_authorization_url, &form.encode())?;
    if response.status < 200 || response.status >= 300 {
        let err: TokenError = serde_json::from_str(&response.body).unwrap_or_else(|_| TokenError {
            error: format!("http_{}", response.status),
            error_description: Some(response.body.clone()),
        });
        return Err(err.into_auth_error(AuthErrorKind::OAuthCodeExchangeFailed));
    }
    let parsed: DeviceAuthorizationResponse =
        serde_json::from_str(&response.body).map_err(|e| {
            AuthError::new(
                AuthErrorKind::OAuthCodeExchangeFailed,
                format!("parse device auth response: {e}"),
            )
        })?;
    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(parsed.expires_in.max(1));
    Ok(DeviceCodeLogin {
        verification_uri: parsed.verification_uri,
        verification_uri_complete: parsed.verification_uri_complete,
        user_code: parsed.user_code,
        expires_at,
        interval: Duration::from_secs(parsed.interval),
        device_code: parsed.device_code,
    })
}

/// Sleep helper. The device-code poll loop uses this so tests can swap
/// it out via [`DeviceCodePollOptions::sleeper`].
pub trait DeviceCodeSleeper: Send + Sync {
    fn sleep(&self, duration: Duration);
}

struct StdSleeper;

impl DeviceCodeSleeper for StdSleeper {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[derive(Default)]
pub struct DeviceCodePollOptions {
    pub max_polls: Option<usize>,
    pub sleeper: Option<std::sync::Arc<dyn DeviceCodeSleeper>>,
}

/// Phase 2: poll the token endpoint until the user authorizes (or the
/// device code expires/is denied).
pub fn device_code_poll(
    cfg: &DeviceCodeClientConfig,
    login: &DeviceCodeLogin,
    transport: &dyn HttpTransport,
    options: DeviceCodePollOptions,
) -> Result<DeviceCodeOutcome, AuthError> {
    let sleeper: std::sync::Arc<dyn DeviceCodeSleeper> = options
        .sleeper
        .unwrap_or_else(|| std::sync::Arc::new(StdSleeper));
    let mut interval = login.interval;
    let max_polls = options.max_polls.unwrap_or(usize::MAX);
    let mut polls = 0usize;
    loop {
        if polls >= max_polls {
            return Err(AuthError::new(
                AuthErrorKind::OAuthTimeout,
                "device-code polling exceeded max attempts",
            ));
        }
        if OffsetDateTime::now_utc() >= login.expires_at {
            return Err(AuthError::new(
                AuthErrorKind::DeviceCodeExpired,
                "device code expired before user completed authorization",
            ));
        }
        polls += 1;
        let form = TokenForm::new()
            .add("grant_type", "urn:ietf:params:oauth:grant-type:device_code")
            .add("client_id", cfg.client_id.clone())
            .add("device_code", login.device_code.clone());
        let response = transport.post_form(&cfg.token_url, &form.encode())?;
        match classify_token_response(&response) {
            DeviceCodePollStep::Token(token) => return Ok(DeviceCodeOutcome { token }),
            DeviceCodePollStep::Pending => {
                sleeper.sleep(interval);
            }
            DeviceCodePollStep::SlowDown => {
                interval += Duration::from_secs(5);
                sleeper.sleep(interval);
            }
            DeviceCodePollStep::Expired => {
                return Err(AuthError::new(
                    AuthErrorKind::DeviceCodeExpired,
                    "device code expired",
                ));
            }
            DeviceCodePollStep::Denied => {
                return Err(AuthError::new(
                    AuthErrorKind::DeviceCodeDenied,
                    "user denied device-code login",
                ));
            }
            DeviceCodePollStep::OtherError(err) => return Err(err),
        }
    }
}

enum DeviceCodePollStep {
    Token(TokenResponse),
    Pending,
    SlowDown,
    Expired,
    Denied,
    OtherError(AuthError),
}

fn classify_token_response(response: &HttpResponse) -> DeviceCodePollStep {
    if response.status >= 200 && response.status < 300 {
        match serde_json::from_str::<TokenResponse>(&response.body) {
            Ok(t) => DeviceCodePollStep::Token(t),
            Err(e) => DeviceCodePollStep::OtherError(AuthError::new(
                AuthErrorKind::OAuthCodeExchangeFailed,
                format!("parse token response: {e}"),
            )),
        }
    } else {
        let err: TokenError = serde_json::from_str(&response.body).unwrap_or_else(|_| TokenError {
            error: format!("http_{}", response.status),
            error_description: Some(response.body.clone()),
        });
        match DeviceCodeTokenError::from_code(&err.error) {
            DeviceCodeTokenError::AuthorizationPending => DeviceCodePollStep::Pending,
            DeviceCodeTokenError::SlowDown => DeviceCodePollStep::SlowDown,
            DeviceCodeTokenError::ExpiredToken => DeviceCodePollStep::Expired,
            DeviceCodeTokenError::AccessDenied => DeviceCodePollStep::Denied,
            DeviceCodeTokenError::Other => DeviceCodePollStep::OtherError(
                err.into_auth_error(AuthErrorKind::OAuthCodeExchangeFailed),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct ScriptedTransport {
        responses: Mutex<std::collections::VecDeque<HttpResponse>>,
        urls: Mutex<Vec<String>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                urls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HttpTransport for ScriptedTransport {
        fn post_form(&self, url: &str, _body: &str) -> Result<HttpResponse, AuthError> {
            self.urls.lock().unwrap().push(url.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| AuthError::new(AuthErrorKind::Network, "no scripted response"))
        }
    }

    #[derive(Default)]
    struct CountingSleeper {
        calls: Mutex<Vec<Duration>>,
    }

    impl DeviceCodeSleeper for CountingSleeper {
        fn sleep(&self, duration: Duration) {
            self.calls.lock().unwrap().push(duration);
        }
    }

    fn cfg() -> DeviceCodeClientConfig {
        DeviceCodeClientConfig {
            provider: ProviderId::Fake,
            client_id: "test-client".into(),
            device_authorization_url: "https://example.test/device".into(),
            token_url: "https://example.test/token".into(),
            scopes: vec!["openid".into()],
        }
    }

    fn device_resp() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"device_code":"DC","user_code":"UC","verification_uri":"https://example.test/v","expires_in":600,"interval":1}"#.into(),
        }
    }

    fn pending_resp() -> HttpResponse {
        HttpResponse {
            status: 400,
            body: r#"{"error":"authorization_pending"}"#.into(),
        }
    }

    fn slow_down_resp() -> HttpResponse {
        HttpResponse {
            status: 400,
            body: r#"{"error":"slow_down"}"#.into(),
        }
    }

    fn expired_resp() -> HttpResponse {
        HttpResponse {
            status: 400,
            body: r#"{"error":"expired_token"}"#.into(),
        }
    }

    fn denied_resp() -> HttpResponse {
        HttpResponse {
            status: 400,
            body: r#"{"error":"access_denied"}"#.into(),
        }
    }

    fn token_resp() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: r#"{"access_token":"AT","refresh_token":"RT","expires_in":900}"#.into(),
        }
    }

    #[test]
    // Device authorization request returns user code, verification URI, and interval.
    fn device_authorization_returns_login() {
        let transport = ScriptedTransport::new(vec![device_resp()]);
        let login = request_device_code(&cfg(), &transport).unwrap();
        assert_eq!(login.user_code, "UC");
        assert_eq!(login.verification_uri, "https://example.test/v");
        assert_eq!(login.interval, Duration::from_secs(1));
    }

    #[test]
    // Polling resolves a token after authorization_pending and slow_down responses;
    // slow_down increases the next interval.
    fn polling_handles_pending_and_slow_down_then_token() {
        let transport =
            ScriptedTransport::new(vec![pending_resp(), slow_down_resp(), token_resp()]);
        let sleeper = Arc::new(CountingSleeper::default());
        let login = DeviceCodeLogin {
            verification_uri: "https://example.test/v".into(),
            verification_uri_complete: None,
            user_code: "UC".into(),
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(60),
            interval: Duration::from_secs(1),
            device_code: "DC".into(),
        };
        let opts = DeviceCodePollOptions {
            max_polls: None,
            sleeper: Some(sleeper.clone()),
        };
        let outcome = device_code_poll(&cfg(), &login, &transport, opts).unwrap();
        assert_eq!(outcome.token.access_token, "AT");

        let calls = sleeper.calls.lock().unwrap().clone();
        // Two waits before success; second one extended by slow_down.
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], Duration::from_secs(1));
        assert!(calls[1] >= Duration::from_secs(6));
    }

    #[test]
    // expired_token is mapped to AuthErrorKind::DeviceCodeExpired.
    fn expired_token_maps_to_expired() {
        let transport = ScriptedTransport::new(vec![expired_resp()]);
        let login = DeviceCodeLogin {
            verification_uri: "x".into(),
            verification_uri_complete: None,
            user_code: "u".into(),
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(60),
            interval: Duration::from_secs(0),
            device_code: "DC".into(),
        };
        let opts = DeviceCodePollOptions {
            max_polls: Some(2),
            sleeper: Some(Arc::new(CountingSleeper::default())),
        };
        let err = device_code_poll(&cfg(), &login, &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::DeviceCodeExpired);
    }

    #[test]
    // access_denied is mapped to AuthErrorKind::DeviceCodeDenied.
    fn access_denied_maps_to_denied() {
        let transport = ScriptedTransport::new(vec![denied_resp()]);
        let login = DeviceCodeLogin {
            verification_uri: "x".into(),
            verification_uri_complete: None,
            user_code: "u".into(),
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(60),
            interval: Duration::from_secs(0),
            device_code: "DC".into(),
        };
        let opts = DeviceCodePollOptions {
            max_polls: Some(2),
            sleeper: Some(Arc::new(CountingSleeper::default())),
        };
        let err = device_code_poll(&cfg(), &login, &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::DeviceCodeDenied);
    }

    #[test]
    // Polling stops with OAuthTimeout when max_polls is exhausted with only
    // pending responses.
    fn max_polls_aborts_loop() {
        let transport = ScriptedTransport::new(vec![pending_resp(), pending_resp()]);
        let login = DeviceCodeLogin {
            verification_uri: "x".into(),
            verification_uri_complete: None,
            user_code: "u".into(),
            expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(60),
            interval: Duration::from_secs(0),
            device_code: "DC".into(),
        };
        let opts = DeviceCodePollOptions {
            max_polls: Some(2),
            sleeper: Some(Arc::new(CountingSleeper::default())),
        };
        let err = device_code_poll(&cfg(), &login, &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::OAuthTimeout);
    }

    #[test]
    // Polling aborts with DeviceCodeExpired once expires_at has passed,
    // independently of the server response.
    fn local_expiry_check_returns_expired() {
        let transport = ScriptedTransport::new(vec![pending_resp()]);
        let login = DeviceCodeLogin {
            verification_uri: "x".into(),
            verification_uri_complete: None,
            user_code: "u".into(),
            expires_at: OffsetDateTime::now_utc() - time::Duration::seconds(1),
            interval: Duration::from_secs(0),
            device_code: "DC".into(),
        };
        let opts = DeviceCodePollOptions {
            max_polls: Some(5),
            sleeper: Some(Arc::new(CountingSleeper::default())),
        };
        let err = device_code_poll(&cfg(), &login, &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::DeviceCodeExpired);
    }
}
