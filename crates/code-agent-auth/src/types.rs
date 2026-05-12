//! Shared types for the auth subsystem (M10).

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Identifier for a model-access provider. The set of *named* variants is
/// closed because they map to first-party integrations; everything else
/// goes through [`ProviderId::Custom`] so external users can wire up their
/// own provider without forking this crate.
///
/// Serialized as the lowercase string returned by [`Self::as_str`] so
/// events / settings stay readable. `Custom(...)` round-trips through
/// the same string form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProviderId {
    Anthropic,
    OpenAi,
    Codex,
    Gemini,
    VertexAi,
    Bedrock,
    Foundry,
    Fake,
    Custom(String),
}

impl Serialize for ProviderId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ProviderId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

impl ProviderId {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::VertexAi => "vertex_ai",
            Self::Bedrock => "bedrock",
            Self::Foundry => "foundry",
            Self::Fake => "fake",
            Self::Custom(s) => s.as_str(),
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "anthropic" => Self::Anthropic,
            "openai" => Self::OpenAi,
            "codex" | "chatgpt" => Self::Codex,
            "gemini" => Self::Gemini,
            "vertex" | "vertex_ai" | "vertex-ai" => Self::VertexAi,
            "bedrock" => Self::Bedrock,
            "foundry" => Self::Foundry,
            "fake" => Self::Fake,
            other => Self::Custom(other.to_string()),
        }
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable identifier for an account associated with a provider.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountId(pub String);

impl AccountId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Build a synthetic account id for an API-key login. The id embeds a
    /// 32-hex-char (128-bit) prefix of `sha256(api_key)` so two distinct
    /// API keys produce distinct ids — preventing the credential-store
    /// upsert from silently replacing a previous key under the same
    /// `(provider, account_id)` tuple. 128 bits puts the birthday
    /// collision boundary at ~2^64 keys, comfortably above any realistic
    /// fleet. The raw key is never serialized; only the hash prefix
    /// appears in metadata.
    pub fn from_api_key(provider_slug: &str, api_key: &str) -> Self {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(api_key.as_bytes());
        let prefix = &hex::encode(digest)[..32];
        Self(format!("{provider_slug}-apikey-{prefix}"))
    }

    /// Stable, redacted hash form suitable for logs/telemetry and for the
    /// on-disk filename used by the file credential backend. A poor-man's
    /// salt is supplied by including the provider id in the input so the
    /// same email under different providers does not hash to the same
    /// filename. A truly random per-store salt is tracked as a follow-up.
    pub fn redacted_hash(&self) -> String {
        self.redacted_hash_with_salt("")
    }

    /// Variant of [`Self::redacted_hash`] that mixes in a caller-supplied
    /// salt prefix. Used by the file credential backend to bind the
    /// hash to a provider, defending against rainbow-table attacks
    /// against well-known email addresses.
    pub fn redacted_hash_with_salt(&self, salt: &str) -> String {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(salt.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.0.as_bytes());
        let digest = hasher.finalize();
        format!("acct_{}", &hex::encode(digest)[..12])
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Mode used to obtain a credential. `Unsupported` is *not* an error — it
/// is a first-class outcome that lets the UI explain why an option is
/// visible-but-unavailable and what the alternatives are.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoginMethod {
    ApiKey,
    OAuthBrowser,
    OAuthDeviceCode,
    ExternalCliManaged,
    ServiceAccount,
    Unsupported {
        reason: String,
        #[serde(default)]
        alternatives: Vec<LoginAlternative>,
    },
}

impl LoginMethod {
    pub fn kind(&self) -> Option<LoginMethodKind> {
        match self {
            Self::ApiKey => Some(LoginMethodKind::ApiKey),
            Self::OAuthBrowser => Some(LoginMethodKind::OAuthBrowser),
            Self::OAuthDeviceCode => Some(LoginMethodKind::OAuthDeviceCode),
            Self::ExternalCliManaged => Some(LoginMethodKind::ExternalCliManaged),
            Self::ServiceAccount => Some(LoginMethodKind::ServiceAccount),
            Self::Unsupported { .. } => None,
        }
    }

    pub fn is_supported(&self) -> bool {
        !matches!(self, Self::Unsupported { .. })
    }
}

/// Wire-friendly "kind" of login method — used in stored records and
/// public CLI/JSON surfaces where carrying the `Unsupported { reason, .. }`
/// shape would be misleading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethodKind {
    ApiKey,
    OAuthBrowser,
    OAuthDeviceCode,
    ExternalCliManaged,
    ServiceAccount,
}

impl LoginMethodKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ApiKey => "api_key",
            Self::OAuthBrowser => "oauth_browser",
            Self::OAuthDeviceCode => "oauth_device_code",
            Self::ExternalCliManaged => "external_cli_managed",
            Self::ServiceAccount => "service_account",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "api_key" | "api-key" | "apikey" => Self::ApiKey,
            "oauth_browser" | "oauth-browser" | "browser" => Self::OAuthBrowser,
            "oauth_device_code" | "oauth-device-code" | "device" => Self::OAuthDeviceCode,
            "external_cli_managed" | "external-cli-managed" | "external" => {
                Self::ExternalCliManaged
            }
            "service_account" | "service-account" => Self::ServiceAccount,
            _ => return None,
        })
    }
}

/// Pointer to a different login flow shown when one is unsupported. Kept
/// fully serializable so it can travel through events without losing
/// fidelity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoginAlternative {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<LoginMethodKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_hint: Option<String>,
}

/// Request body for [`AuthProvider::login`]. We carry both an opaque
/// `params` map and typed convenience fields so providers can stay
/// extensible without losing type-safety on common inputs.
#[derive(Debug, Default)]
pub struct LoginRequest {
    pub method: Option<LoginMethodKind>,
    pub api_key: Option<SecretString>,
    pub params: BTreeMap<String, String>,
}

impl LoginRequest {
    pub fn api_key(method: LoginMethodKind, key: impl Into<String>) -> Self {
        Self {
            method: Some(method),
            api_key: Some(SecretString::new(key.into())),
            params: BTreeMap::new(),
        }
    }
}

/// Result of a successful login. `account_id` is provider-stable and
/// recorded in [`StoredAccount`] so subsequent CLI invocations can
/// reference it.
#[derive(Debug, Clone)]
pub struct LoginResult {
    pub account: StoredAccount,
    pub secret: StoredCredentialSecret,
    /// Redacted summary safe for emitting to telemetry.
    pub display_summary: String,
}

/// What [`AuthProvider::status`] reports about a stored or env-resolved
/// account. Always redacted.
#[derive(Debug, Clone)]
pub struct AuthStatus {
    pub provider: ProviderId,
    pub account: Option<StoredAccount>,
    pub source_label: String,
    pub healthy: bool,
    pub message: String,
}

/// Result of a refresh attempt.
#[derive(Debug, Clone)]
pub struct RefreshResult {
    pub account: StoredAccount,
    pub secret: StoredCredentialSecret,
    pub refreshed: bool,
}

/// Persistent metadata for a successfully-authenticated account. Never
/// stores raw secrets — those live in [`StoredCredentialSecret`] and are
/// kept in the credential store backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredAccount {
    pub account_id: AccountId,
    pub provider: ProviderId,
    pub login_method: LoginMethodKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub last_refresh_at: Option<OffsetDateTime>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub needs_reauth: bool,
}

impl StoredAccount {
    /// Redact the displayed email when `redact` is true. Returns a value
    /// suitable for status/UI display; never the raw secret.
    pub fn display_label(&self, redact: bool) -> String {
        match (&self.display_email, redact) {
            (Some(email), true) => redact_email(email),
            (Some(email), false) => email.clone(),
            (None, _) => self.account_id.0.clone(),
        }
    }
}

fn redact_email(email: &str) -> String {
    if let Some((local, domain)) = email.split_once('@') {
        let visible = local.chars().take(2).collect::<String>();
        format!("{}***@{}", visible, domain)
    } else {
        "***".into()
    }
}

/// Secret payload stored under a [`StoredAccount`]. Wrapped in
/// [`SecretString`] to keep the value out of `Debug` logs.
#[derive(Debug, Clone, Default)]
pub struct StoredCredentialSecret {
    pub access_token: Option<SecretString>,
    pub refresh_token: Option<SecretString>,
    pub api_key: Option<SecretString>,
    pub id_token: Option<SecretString>,
}

/// Identifies the in-memory secret kind for telemetry events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialSecretKind {
    ApiKey,
    OAuthToken,
    ExternalManaged,
}

/// A credential resolved for a single model-call. This is what providers
/// receive at request time so they can attach the right `Authorization`,
/// `x-api-key`, etc. without ever loading the secret store themselves.
#[derive(Clone)]
pub enum ResolvedCredential {
    ApiKey {
        provider: ProviderId,
        source: CredentialSource,
        key: SecretString,
    },
    OAuthBearer {
        provider: ProviderId,
        account_id: AccountId,
        access_token: SecretString,
        expires_at: Option<OffsetDateTime>,
    },
    ExternalManaged {
        provider: ProviderId,
        account_id: Option<AccountId>,
        metadata: serde_json::Value,
    },
    None,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey {
                provider, source, ..
            } => f
                .debug_struct("ApiKey")
                .field("provider", provider)
                .field("source", source)
                .field("key", &"<redacted>")
                .finish(),
            Self::OAuthBearer {
                provider,
                account_id,
                expires_at,
                ..
            } => f
                .debug_struct("OAuthBearer")
                .field("provider", provider)
                .field("account_id", account_id)
                .field("expires_at", expires_at)
                .field("access_token", &"<redacted>")
                .finish(),
            Self::ExternalManaged {
                provider,
                account_id,
                ..
            } => f
                .debug_struct("ExternalManaged")
                .field("provider", provider)
                .field("account_id", account_id)
                .finish(),
            Self::None => f.write_str("None"),
        }
    }
}

impl ResolvedCredential {
    pub fn provider(&self) -> Option<&ProviderId> {
        match self {
            Self::ApiKey { provider, .. }
            | Self::OAuthBearer { provider, .. }
            | Self::ExternalManaged { provider, .. } => Some(provider),
            Self::None => None,
        }
    }

    pub fn source_label(&self) -> &'static str {
        match self {
            Self::ApiKey { source, .. } => source.as_str(),
            Self::OAuthBearer { .. } => "oauth_token",
            Self::ExternalManaged { .. } => "external_managed",
            Self::None => "none",
        }
    }

    pub fn account_id(&self) -> Option<&AccountId> {
        match self {
            Self::OAuthBearer { account_id, .. } => Some(account_id),
            Self::ExternalManaged { account_id, .. } => account_id.as_ref(),
            Self::ApiKey { .. } | Self::None => None,
        }
    }
}

/// Where a resolved credential came from. Carried into events so users can
/// understand `auth precedence` outcomes without leaking the secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CredentialSource {
    Cli,
    EnvVar(String),
    Keychain,
    FileStore,
    ExternalSidecar,
    TestFake,
}

impl CredentialSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::EnvVar(_) => "env_var",
            Self::Keychain => "keychain",
            Self::FileStore => "file_store",
            Self::ExternalSidecar => "external_sidecar",
            Self::TestFake => "test_fake",
        }
    }
}

/// Mode controlling which backend the credential store will use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStoreMode {
    #[default]
    Auto,
    Keychain,
    File,
}

impl CredentialStoreMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "keychain" | "os_keychain" | "os-keychain" => Self::Keychain,
            "file" | "filestore" | "file_store" => Self::File,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Keychain => "keychain",
            Self::File => "file",
        }
    }
}

/// Tells the auth resolver in what order to consult sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthPrecedenceMode {
    #[default]
    ExplicitFirst,
    EnvFirst,
    StoredFirst,
}

impl AuthPrecedenceMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "explicit_first" | "explicit-first" => Self::ExplicitFirst,
            "env_first" | "env-first" => Self::EnvFirst,
            "stored_first" | "stored-first" => Self::StoredFirst,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExplicitFirst => "explicit_first",
            Self::EnvFirst => "env_first",
            Self::StoredFirst => "stored_first",
        }
    }
}

/// Lightweight secret wrapper. Hides the inner value from `Debug` and
/// `Display` and zeroizes its bytes on drop. Not a `SecretString` from
/// the `secrecy` crate, since pulling that in for a single field would
/// add a dep transitively touching every consumer; we recreate the
/// minimum guarantees here.
pub struct SecretString {
    inner: Vec<u8>,
}

impl SecretString {
    pub fn new(s: String) -> Self {
        Self {
            inner: s.into_bytes(),
        }
    }

    pub fn expose(&self) -> &str {
        // Safe: we always populate from a `String` (UTF-8 by construction).
        std::str::from_utf8(&self.inner).unwrap_or("")
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Clone for SecretString {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Best-effort zeroize; volatile write guards against the optimizer
        // dead-store-eliminating the loop.
        for b in self.inner.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl Serialize for SecretString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Persistence-only: the file credential store needs to round-trip
        // secrets to disk. The wrapper still hides the value from `Debug`
        // / `Display`; serialization is opt-in by virtue of the on-disk
        // schema (which is itself opt-in).
        serializer.serialize_str(self.expose())
    }
}

impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::new(s))
    }
}

/// Snapshot of provider usage information returned by a model call.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderUsageStatus {
    pub provider: Option<ProviderId>,
    pub billing_mode: BillingMode,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub estimated_cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<ProviderRateLimit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderRateLimit {
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub reset_at: Option<OffsetDateTime>,
    pub label: Option<String>,
}

/// How a model is billed at the provider. `subscription_*` modes hide the
/// local dollar estimate because it would be misleading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingMode {
    #[default]
    Unknown,
    ApiUsageBased,
    SubscriptionIncluded,
    SubscriptionCredits,
}

impl BillingMode {
    pub fn is_subscription_backed(self) -> bool {
        matches!(self, Self::SubscriptionIncluded | Self::SubscriptionCredits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // ProviderId round-trips through parse + as_str for known and custom values.
    fn provider_id_roundtrips_known_and_custom() {
        for s in [
            "anthropic",
            "openai",
            "codex",
            "gemini",
            "vertex_ai",
            "fake",
        ] {
            assert_eq!(ProviderId::parse(s).as_str(), s);
        }
        let custom = ProviderId::parse("Acme-LLM");
        assert_eq!(custom.as_str(), "acme-llm");
    }

    #[test]
    // SecretString never leaks the inner value from Debug/Display.
    fn secret_string_is_redacted() {
        let s = SecretString::new("sk-test-1234".into());
        assert_eq!(format!("{:?}", s), "SecretString(<redacted>)");
        assert_eq!(format!("{}", s), "<redacted>");
        assert_eq!(s.expose(), "sk-test-1234");
    }

    #[test]
    // Email redaction shows only the first 2 local-part chars.
    fn redact_email_shows_only_prefix() {
        assert_eq!(redact_email("alice@example.com"), "al***@example.com");
        assert_eq!(redact_email("nobodyhere"), "***");
    }

    #[test]
    // AccountId redacted_hash is stable, deterministic, and contains no PII.
    fn account_id_hash_is_stable() {
        let a = AccountId::new("user@example.com");
        let h1 = a.redacted_hash();
        let h2 = a.redacted_hash();
        assert_eq!(h1, h2);
        assert!(h1.starts_with("acct_"));
        assert!(!h1.contains("user"));
    }

    #[test]
    // AccountId::from_api_key returns distinct ids for distinct API keys.
    fn from_api_key_returns_distinct_ids_for_distinct_keys() {
        let a = AccountId::from_api_key("anthropic", "sk-aaa");
        let b = AccountId::from_api_key("anthropic", "sk-bbb");
        assert_ne!(a, b);
        // The synthetic id encodes a hash prefix, not the key itself.
        assert!(!a.as_str().contains("sk-aaa"));
        assert!(!b.as_str().contains("sk-bbb"));
        assert!(a.as_str().starts_with("anthropic-apikey-"));
        // Deterministic for the same key.
        assert_eq!(a, AccountId::from_api_key("anthropic", "sk-aaa"));
    }

    #[test]
    // The hash-prefix carried in the synthetic account id is at least
    // 128 bits (32 hex chars) so birthday-collision risk is negligible
    // even across very large fleets. An 8-hex-char (32-bit) prefix
    // collides at ~65k keys, which is unsafe.
    fn from_api_key_hash_prefix_is_at_least_128_bits() {
        let id = AccountId::from_api_key("anthropic", "sk-some-test-key");
        let suffix = id
            .as_str()
            .strip_prefix("anthropic-apikey-")
            .expect("expected anthropic-apikey- prefix");
        assert!(
            suffix.len() >= 32,
            "expected >= 32 hex chars (128 bits), got {}: {suffix}",
            suffix.len()
        );
        assert!(
            suffix.chars().all(|c| c.is_ascii_hexdigit()),
            "expected pure hex suffix, got {suffix}"
        );
    }

    #[test]
    // redacted_hash_with_salt produces different hashes for the same
    // account id under different salts.
    fn redacted_hash_with_salt_is_salt_sensitive() {
        let a = AccountId::new("user@example.com");
        let h_anthropic = a.redacted_hash_with_salt("anthropic");
        let h_openai = a.redacted_hash_with_salt("openai");
        assert_ne!(h_anthropic, h_openai);
        assert!(h_anthropic.starts_with("acct_"));
    }

    #[test]
    // BillingMode::is_subscription_backed only returns true for subscription variants.
    fn billing_mode_subscription_check() {
        assert!(BillingMode::SubscriptionIncluded.is_subscription_backed());
        assert!(BillingMode::SubscriptionCredits.is_subscription_backed());
        assert!(!BillingMode::ApiUsageBased.is_subscription_backed());
        assert!(!BillingMode::Unknown.is_subscription_backed());
    }
}
