//! Generic OAuth 2.0 helpers: PKCE, CSRF state, loopback callback,
//! authorization-code exchange, and device-code polling.
//!
//! No first-party client IDs are hard-coded here. Callers must supply a
//! [`OAuthClientConfig`] (or [`DeviceCodeClientConfig`]) whose `client_id`
//! and endpoints come from the user's settings or an explicitly
//! authorized provider integration.

pub mod browser;
pub mod device;
pub mod pkce;
pub mod token;

pub use browser::{
    oauth_browser_login, BrowserLoginOutcome, LoopbackCallback, OAuthBrowserOptions,
    OAuthClientConfig, OAuthLoginSession, RedirectMode,
};
pub use device::{
    device_code_poll, request_device_code, DeviceCodeClientConfig, DeviceCodeLogin,
    DeviceCodeOutcome, DeviceCodeTokenError,
};
pub use pkce::{generate_state, generate_verifier, s256_challenge};
pub use token::{TokenError, TokenForm, TokenResponse};
