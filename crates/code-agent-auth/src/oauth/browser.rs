//! Authorization-code OAuth 2.0 with PKCE and a loopback callback.
//!
//! The flow is split into two phases so tests can drive the happy path
//! with deterministic state values:
//!
//! 1. [`start_oauth_browser_login`] — generates state + PKCE, builds the
//!    auth URL, optionally binds a loopback listener, and asks the
//!    [`BrowserOpener`] to open the URL.
//! 2. [`finish_oauth_browser_login`] — waits for the callback (or accepts
//!    one injected through [`OAuthBrowserOptions::force_callback_query`]),
//!    validates state, and exchanges the code for a token.
//!
//! The convenience function [`oauth_browser_login`] wires the two phases
//! together for production callers.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;

use crate::error::{AuthError, AuthErrorKind};
use crate::oauth::pkce::{generate_state, generate_verifier, s256_challenge};
use crate::oauth::token::{url_encode, TokenError, TokenForm, TokenResponse};
use crate::provider::BrowserOpener;
use crate::types::{ProviderId, SecretString};

/// Loopback or manual-code redirection style.
#[derive(Debug, Clone)]
pub enum RedirectMode {
    Loopback {
        preferred_port: Option<u16>,
        callback_path: String,
    },
    ManualCode,
}

/// Minimal OAuth client config. Caller is responsible for sourcing
/// `client_id` from settings or an authorized provider integration —
/// we never embed first-party client IDs as defaults.
#[derive(Debug, Clone)]
pub struct OAuthClientConfig {
    pub provider: ProviderId,
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    pub redirect_mode: RedirectMode,
    pub supports_pkce: bool,
    pub extra_auth_params: BTreeMap<String, String>,
    pub extra_token_params: BTreeMap<String, String>,
}

/// In-flight login session. The verifier is held in memory only and is
/// dropped (zeroized) when the session is dropped.
pub struct OAuthLoginSession {
    pub provider: ProviderId,
    pub state: SecretString,
    pub pkce_verifier: Option<SecretString>,
    pub auth_url: String,
    pub expires_at: OffsetDateTime,
    pub callback: Option<LoopbackCallback>,
    pub redirect_uri: Option<String>,
}

/// Bound loopback listener that knows where to listen for the OAuth
/// redirect, and what callback path to expect.
pub struct LoopbackCallback {
    pub listener: TcpListener,
    pub callback_path: String,
    pub redirect_uri: String,
}

impl LoopbackCallback {
    pub fn bind(preferred_port: Option<u16>, callback_path: String) -> Result<Self, AuthError> {
        let port = preferred_port.unwrap_or(0);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = TcpListener::bind(addr)
            .or_else(|_| TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))))
            .map_err(|e| AuthError::new(AuthErrorKind::Network, format!("bind loopback: {e}")))?;
        let actual = listener
            .local_addr()
            .map_err(|e| AuthError::new(AuthErrorKind::Network, format!("local_addr: {e}")))?;
        let redirect_uri = format!("http://127.0.0.1:{}{}", actual.port(), callback_path);
        Ok(Self {
            listener,
            callback_path,
            redirect_uri,
        })
    }
}

/// HTTP transport for the token endpoint. Real callers can plug in a
/// reqwest-backed client; tests use a fake.
pub trait HttpTransport: Send + Sync {
    fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, AuthError>;
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

/// Result of a successful browser login.
#[derive(Debug, Clone)]
pub struct BrowserLoginOutcome {
    pub token: TokenResponse,
    pub redirect_uri: String,
}

#[derive(Debug, Clone)]
pub struct OAuthBrowserOptions {
    pub timeout: Duration,
    /// If set, [`finish_oauth_browser_login`] uses this query string
    /// instead of waiting on the loopback listener. Used by tests/demos.
    pub force_callback_query: Option<String>,
}

impl Default for OAuthBrowserOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            force_callback_query: None,
        }
    }
}

/// Build the authorization URL for a browser-flow session. Public so
/// integration tests can inspect it.
pub fn build_auth_url(
    cfg: &OAuthClientConfig,
    state: &str,
    code_challenge: Option<&str>,
    redirect_uri: Option<&str>,
) -> String {
    let mut url = cfg.auth_url.clone();
    let sep = if url.contains('?') { '&' } else { '?' };
    url.push(sep);
    url.push_str("response_type=code");
    url.push_str("&client_id=");
    url.push_str(&url_encode(&cfg.client_id));
    if let Some(redir) = redirect_uri {
        url.push_str("&redirect_uri=");
        url.push_str(&url_encode(redir));
    }
    if !cfg.scopes.is_empty() {
        url.push_str("&scope=");
        url.push_str(&url_encode(&cfg.scopes.join(" ")));
    }
    url.push_str("&state=");
    url.push_str(&url_encode(state));
    if let Some(c) = code_challenge {
        url.push_str("&code_challenge=");
        url.push_str(&url_encode(c));
        url.push_str("&code_challenge_method=S256");
    }
    for (k, v) in &cfg.extra_auth_params {
        url.push('&');
        url.push_str(&url_encode(k));
        url.push('=');
        url.push_str(&url_encode(v));
    }
    url
}

/// Phase 1: build state, verifier, and auth URL; bind a loopback
/// listener if applicable; ask the opener to open the URL. The opener
/// failing is non-fatal — the auth URL is still returned via the
/// session for manual paste.
pub fn start_oauth_browser_login(
    cfg: &OAuthClientConfig,
    opener: Arc<dyn BrowserOpener>,
) -> Result<(OAuthLoginSession, Result<(), String>), AuthError> {
    let state = generate_state();
    let verifier = if cfg.supports_pkce {
        Some(generate_verifier())
    } else {
        None
    };
    let challenge = verifier.as_deref().map(s256_challenge);

    let (callback, redirect_uri) = match &cfg.redirect_mode {
        RedirectMode::Loopback {
            preferred_port,
            callback_path,
        } => {
            let cb = LoopbackCallback::bind(*preferred_port, callback_path.clone())?;
            let uri = cb.redirect_uri.clone();
            (Some(cb), Some(uri))
        }
        RedirectMode::ManualCode => (None, None),
    };

    let auth_url = build_auth_url(cfg, &state, challenge.as_deref(), redirect_uri.as_deref());
    let opened = opener.open(&auth_url);

    let expires_at = OffsetDateTime::now_utc() + time::Duration::seconds(120);
    let session = OAuthLoginSession {
        provider: cfg.provider.clone(),
        state: SecretString::new(state),
        pkce_verifier: verifier.map(SecretString::new),
        auth_url,
        expires_at,
        callback,
        redirect_uri,
    };
    Ok((session, opened))
}

/// Phase 2: wait for the callback, validate state, exchange code -> token.
pub fn finish_oauth_browser_login(
    session: OAuthLoginSession,
    cfg: &OAuthClientConfig,
    transport: &dyn HttpTransport,
    options: OAuthBrowserOptions,
) -> Result<BrowserLoginOutcome, AuthError> {
    let callback_query = if let Some(forced) = options.force_callback_query.as_deref() {
        forced.to_string()
    } else if let Some(cb) = session.callback.as_ref() {
        wait_for_callback(&cb.listener, &cb.callback_path, options.timeout)?
    } else {
        return Err(AuthError::new(
            AuthErrorKind::OAuthTimeout,
            "manual-code OAuth flow requires `options.force_callback_query` to be set in this build",
        ));
    };

    let parsed = parse_callback_query(&callback_query);
    if let Some(err) = parsed.error.as_deref() {
        return Err(AuthError::new(
            AuthErrorKind::OAuthCodeExchangeFailed,
            format!("authorization endpoint returned error: {err}"),
        ));
    }
    if parsed.state != session.state.expose() {
        return Err(AuthError::new(
            AuthErrorKind::OAuthStateMismatch,
            "OAuth callback state did not match",
        ));
    }
    let code = parsed.code.ok_or_else(|| {
        AuthError::new(
            AuthErrorKind::OAuthCodeExchangeFailed,
            "authorization endpoint returned no code",
        )
    })?;

    let mut form = TokenForm::new()
        .add("grant_type", "authorization_code")
        .add("client_id", cfg.client_id.clone())
        .add("code", code);
    if let Some(redir) = session.redirect_uri.as_deref() {
        form = form.add("redirect_uri", redir.to_string());
    }
    if let Some(v) = session.pkce_verifier.as_ref() {
        form = form.add("code_verifier", v.expose().to_string());
    }
    form = form.extend(cfg.extra_token_params.clone());

    let response = transport.post_form(&cfg.token_url, &form.encode())?;
    let token = parse_token_response(&response)?;

    Ok(BrowserLoginOutcome {
        token,
        redirect_uri: session.redirect_uri.clone().unwrap_or_default(),
    })
}

/// Convenience wrapper combining both phases.
pub fn oauth_browser_login(
    cfg: &OAuthClientConfig,
    opener: Arc<dyn BrowserOpener>,
    transport: &dyn HttpTransport,
    options: OAuthBrowserOptions,
) -> Result<BrowserLoginOutcome, AuthError> {
    let (session, _opened) = start_oauth_browser_login(cfg, opener)?;
    finish_oauth_browser_login(session, cfg, transport, options)
}

#[derive(Debug, Default)]
struct CallbackParams {
    code: Option<String>,
    state: String,
    error: Option<String>,
}

fn parse_callback_query(query: &str) -> CallbackParams {
    let mut params = CallbackParams::default();
    for part in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = match part.split_once('=') {
            Some(p) => p,
            None => (part, ""),
        };
        let v = url_decode(v);
        match k {
            "code" => params.code = Some(v),
            "state" => params.state = v,
            "error" => params.error = Some(v),
            _ => {}
        }
    }
    params
}

fn url_decode(s: &str) -> String {
    crate::oauth::token::url_decode(s)
}

fn wait_for_callback(
    listener: &TcpListener,
    callback_path: &str,
    timeout: Duration,
) -> Result<String, AuthError> {
    listener
        .set_nonblocking(true)
        .map_err(|e| AuthError::new(AuthErrorKind::Network, format!("set_nonblocking: {e}")))?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() > deadline {
            return Err(AuthError::new(
                AuthErrorKind::OAuthTimeout,
                "timed out waiting for OAuth callback",
            ));
        }
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .map_err(|e| {
                        AuthError::new(AuthErrorKind::Network, format!("set_read_timeout: {e}"))
                    })?;
                let mut buf = [0u8; 4096];
                let mut total = String::new();
                loop {
                    let n = stream.read(&mut buf).map_err(|e| {
                        AuthError::new(AuthErrorKind::Network, format!("read callback: {e}"))
                    })?;
                    if n == 0 {
                        break;
                    }
                    total.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if total.contains("\r\n\r\n") {
                        break;
                    }
                }

                let request_line = total.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let target = parts.next().unwrap_or("");
                let (path, query) = match target.split_once('?') {
                    Some((p, q)) => (p, q),
                    None => (target, ""),
                };

                // Reject everything that is not a GET to the configured
                // callback path. Drop the stream and keep listening — a
                // favicon probe or stray local request must not consume
                // the only callback we accept.
                if method != "GET" {
                    let body = b"Method Not Allowed";
                    let _ = write!(
                        stream,
                        "HTTP/1.1 405 Method Not Allowed\r\nAllow: GET\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                    continue;
                }
                if path != callback_path {
                    let body = b"Not Found";
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                    let _ = stream.flush();
                    continue;
                }

                let body = b"<html><body><p>You may close this tab and return to the terminal.</p></body></html>";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body);
                let _ = stream.flush();

                return Ok(query.to_string());
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(AuthError::new(
                    AuthErrorKind::Network,
                    format!("accept callback: {e}"),
                ));
            }
        }
    }
}

fn parse_token_response(response: &HttpResponse) -> Result<TokenResponse, AuthError> {
    if response.status >= 200 && response.status < 300 {
        let token: TokenResponse = serde_json::from_str(&response.body).map_err(|e| {
            AuthError::new(
                AuthErrorKind::OAuthCodeExchangeFailed,
                format!("parse token response: {}", e),
            )
        })?;
        Ok(token)
    } else {
        let err: TokenError = serde_json::from_str(&response.body).unwrap_or_else(|_| TokenError {
            error: format!("http_{}", response.status),
            error_description: Some(response.body.clone()),
        });
        Err(err.into_auth_error(AuthErrorKind::OAuthCodeExchangeFailed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FailingBrowserOpener, RecordingBrowserOpener};

    struct FakeTransport {
        response: HttpResponse,
        captured: std::sync::Mutex<Option<(String, String)>>,
    }

    impl FakeTransport {
        fn ok_token() -> Self {
            Self {
                response: HttpResponse {
                    status: 200,
                    body: r#"{"access_token":"AT","refresh_token":"RT","expires_in":600}"#.into(),
                },
                captured: Default::default(),
            }
        }

        fn err() -> Self {
            Self {
                response: HttpResponse {
                    status: 400,
                    body: r#"{"error":"invalid_grant","error_description":"bad code"}"#.into(),
                },
                captured: Default::default(),
            }
        }
    }

    impl HttpTransport for FakeTransport {
        fn post_form(&self, url: &str, body: &str) -> Result<HttpResponse, AuthError> {
            *self.captured.lock().unwrap() = Some((url.to_string(), body.to_string()));
            Ok(self.response.clone())
        }
    }

    fn cfg() -> OAuthClientConfig {
        OAuthClientConfig {
            provider: ProviderId::Fake,
            client_id: "test-client".into(),
            auth_url: "https://example.test/auth".into(),
            token_url: "https://example.test/token".into(),
            scopes: vec!["openid".into(), "email".into()],
            redirect_mode: RedirectMode::Loopback {
                preferred_port: None,
                callback_path: "/callback".into(),
            },
            supports_pkce: true,
            extra_auth_params: Default::default(),
            extra_token_params: Default::default(),
        }
    }

    #[test]
    // Authorization URL contains client_id, scopes, state, S256 PKCE challenge,
    // and the loopback redirect_uri.
    fn auth_url_contains_required_params() {
        let url = build_auth_url(
            &cfg(),
            "state-xyz",
            Some("CHAL"),
            Some("http://127.0.0.1:1234/callback"),
        );
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=test-client"));
        assert!(url.contains("scope=openid+email"));
        assert!(url.contains("state=state-xyz"));
        assert!(url.contains("code_challenge=CHAL"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1234%2Fcallback"));
    }

    #[test]
    // Happy path: state matches, transport returns 200 with a token —
    // outcome carries the access token and the form body includes the
    // PKCE verifier.
    fn happy_path_exchange_returns_token_with_pkce() {
        let opener = Arc::new(RecordingBrowserOpener::default());
        let transport = FakeTransport::ok_token();
        let (session, opened) = start_oauth_browser_login(&cfg(), opener.clone()).unwrap();
        assert!(opened.is_ok());
        assert!(session.pkce_verifier.is_some());
        let state = session.state.expose().to_string();
        let verifier = session.pkce_verifier.as_ref().unwrap().expose().to_string();
        let opts = OAuthBrowserOptions {
            force_callback_query: Some(format!("code=GOOD&state={}", state)),
            ..Default::default()
        };
        let outcome = finish_oauth_browser_login(session, &cfg(), &transport, opts).unwrap();
        assert_eq!(outcome.token.access_token, "AT");
        assert_eq!(outcome.token.refresh_token.as_deref(), Some("RT"));

        // Form body must carry grant_type, code, client_id, redirect_uri,
        // and the PKCE verifier.
        let captured = transport.captured.lock().unwrap().clone().unwrap();
        let body = captured.1;
        assert!(body.contains("grant_type=authorization_code"));
        assert!(body.contains("code=GOOD"));
        assert!(body.contains("client_id=test-client"));
        assert!(body.contains(&format!("code_verifier={}", verifier)));
        // Browser was opened with the auth URL.
        let opened = opener.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].contains("client_id=test-client"));
    }

    #[test]
    // State mismatch is rejected before the token endpoint is hit.
    fn state_mismatch_is_rejected() {
        let opener = Arc::new(RecordingBrowserOpener::default());
        let transport = FakeTransport::ok_token();
        let (session, _) = start_oauth_browser_login(&cfg(), opener).unwrap();
        let opts = OAuthBrowserOptions {
            force_callback_query: Some("code=ABC&state=NOT_MATCHING".into()),
            ..Default::default()
        };
        let err = finish_oauth_browser_login(session, &cfg(), &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::OAuthStateMismatch);
        assert!(transport.captured.lock().unwrap().is_none());
    }

    #[test]
    // Token-exchange failures are surfaced as structured errors with the
    // provider's error_description.
    fn token_endpoint_error_returns_structured_error() {
        let opener = Arc::new(RecordingBrowserOpener::default());
        let transport = FakeTransport::err();
        let (session, _) = start_oauth_browser_login(&cfg(), opener).unwrap();
        let state = session.state.expose().to_string();
        let opts = OAuthBrowserOptions {
            force_callback_query: Some(format!("code=BAD&state={state}")),
            ..Default::default()
        };
        let err = finish_oauth_browser_login(session, &cfg(), &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::OAuthCodeExchangeFailed);
        assert!(err.message.contains("bad code"));
    }

    #[test]
    // A failing browser opener does not abort the flow — the caller can
    // still copy the auth URL manually.
    fn failing_browser_opener_does_not_abort() {
        let opener = Arc::new(FailingBrowserOpener);
        let (session, opened) = start_oauth_browser_login(&cfg(), opener).unwrap();
        assert!(opened.is_err());
        assert!(!session.auth_url.is_empty());
    }

    #[test]
    // Authorization endpoint error= is surfaced as OAuthCodeExchangeFailed.
    fn authorization_endpoint_error_is_propagated() {
        let opener = Arc::new(RecordingBrowserOpener::default());
        let transport = FakeTransport::ok_token();
        let (session, _) = start_oauth_browser_login(&cfg(), opener).unwrap();
        let state = session.state.expose().to_string();
        let opts = OAuthBrowserOptions {
            force_callback_query: Some(format!("error=access_denied&state={state}")),
            ..Default::default()
        };
        let err = finish_oauth_browser_login(session, &cfg(), &transport, opts).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::OAuthCodeExchangeFailed);
        assert!(err.message.contains("access_denied"));
    }

    #[test]
    // url_decode reverses url_encode behavior including the `+` -> space rule.
    fn url_decode_inverse_of_encode() {
        for s in ["abc", "a b c", "k v", "x&y"] {
            let enc = crate::oauth::token::url_encode(s);
            let dec = url_decode(&enc);
            assert_eq!(dec, s);
        }
    }

    #[test]
    // url_decode reassembles multi-byte UTF-8 percent-encoded sequences
    // into a single Unicode scalar, not one char per encoded byte.
    fn url_decode_handles_multi_byte_utf8() {
        let decoded = url_decode("%C3%A9");
        assert_eq!(decoded, "é");
        assert_eq!(decoded.chars().count(), 1);
    }

    #[test]
    // url_decode translates %20 to a single space (pinning existing behavior).
    fn url_decode_translates_percent20_to_space() {
        assert_eq!(url_decode("hello%20world"), "hello world");
    }

    #[test]
    // url_decode preserves a trailing `%` with no following chars as a literal.
    fn url_decode_preserves_lone_percent() {
        assert_eq!(url_decode("%"), "%");
    }

    #[test]
    // url_decode preserves `%` followed by a single trailing char as literals.
    fn url_decode_preserves_percent_with_one_trailing_char() {
        assert_eq!(url_decode("%A"), "%A");
    }

    #[test]
    // url_decode preserves `%` followed by two non-hex chars as literals.
    fn url_decode_preserves_percent_with_two_non_hex_chars() {
        assert_eq!(url_decode("%ZZ"), "%ZZ");
    }

    #[test]
    // url_decode preserves a malformed escape embedded in surrounding text.
    fn url_decode_preserves_malformed_escape_in_context() {
        assert_eq!(url_decode("a%Zb"), "a%Zb");
    }

    #[test]
    // url_decode decodes a well-formed single-byte hex escape (pinning behavior).
    fn url_decode_decodes_single_byte_escape() {
        assert_eq!(url_decode("%41"), "A");
    }

    fn bind_local_listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    fn send_raw_request(port: u16, request: &str) -> std::net::TcpStream {
        use std::net::TcpStream;
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        // The server thread accepts on a non-blocking listener with a 50ms
        // poll, so a connect can land in a transient window. Under heavy
        // parallel test load (e.g. nextest on a busy macOS runner) the
        // loopback connect can momentarily get `ConnectionRefused` before the
        // accept loop drains the backlog. Retry briefly rather than unwrap on
        // the first attempt — the server deadline is 5s, well beyond this.
        let mut stream = {
            let mut last_err = None;
            let mut connected = None;
            for _ in 0..100 {
                match TcpStream::connect(addr) {
                    Ok(s) => {
                        connected = Some(s);
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
            }
            connected.unwrap_or_else(|| panic!("connect to callback server failed: {last_err:?}"))
        };
        stream.write_all(request.as_bytes()).unwrap();
        stream.flush().unwrap();
        stream
    }

    #[test]
    // wait_for_callback skips a request whose path does not match the
    // configured callback path (e.g. a favicon probe) and continues
    // listening until the real callback arrives.
    fn wait_for_callback_skips_non_matching_paths() {
        let (listener, port) = bind_local_listener();
        let handle = std::thread::spawn(move || {
            wait_for_callback(&listener, "/callback", Duration::from_secs(5))
        });

        // First probe to /favicon.ico must be ignored.
        let probe = send_raw_request(
            port,
            "GET /favicon.ico HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        // Drain and drop the probe response so the server can move on.
        drop(probe);

        // Real callback arrives on the configured path.
        let real = send_raw_request(
            port,
            "GET /callback?state=S&code=C HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        drop(real);

        let query = handle.join().unwrap().unwrap();
        assert_eq!(query, "state=S&code=C");
    }

    #[test]
    // wait_for_callback rejects non-GET methods and keeps listening for
    // the real callback.
    fn wait_for_callback_rejects_non_get_methods() {
        let (listener, port) = bind_local_listener();
        let handle = std::thread::spawn(move || {
            wait_for_callback(&listener, "/callback", Duration::from_secs(5))
        });

        // POST to the right path is rejected; loop must continue.
        let bad = send_raw_request(
            port,
            "POST /callback HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        drop(bad);

        let good = send_raw_request(
            port,
            "GET /callback?state=X&code=Y HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
        );
        drop(good);

        let query = handle.join().unwrap().unwrap();
        assert_eq!(query, "state=X&code=Y");
    }
}
