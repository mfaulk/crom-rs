//! Provider profiles: the single source of truth for how a provider id maps
//! to a wire protocol, endpoint, API-key env var, default model, and
//! capability flags. Built-ins come from `providers.toml`; settings may patch
//! or add profiles. CLI, SDK, and eval all resolve through [`resolve_profile`].
//!
//! Invariant: profiles never carry secrets. `api_key_env` is validated to be
//! an environment-variable *name*, and unknown fields (e.g. `api_key`) are
//! rejected at parse time.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Checked-in built-in profiles.
pub const BUILTIN_PROVIDERS_TOML: &str = include_str!("../providers.toml");

/// Wire protocol a profile speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderKind {
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "gemini")]
    Gemini,
}

/// Request-body field that carries the output-token limit for
/// OpenAI-compatible endpoints.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    #[default]
    MaxTokens,
    MaxCompletionTokens,
}

impl MaxTokensField {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MaxTokens => "max_tokens",
            Self::MaxCompletionTokens => "max_completion_tokens",
        }
    }
}

/// Capability flags. `None` means unknown — callers must not assume support.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    #[serde(default)]
    pub tool_calls: Option<bool>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub streaming: Option<bool>,
    #[serde(default)]
    pub reasoning_content: Option<bool>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
}

impl ProviderCapabilities {
    fn merge(&mut self, src: ProviderCapabilities) {
        macro_rules! take {
            ($($f:ident),*) => { $( if src.$f.is_some() { self.$f = src.$f; } )* };
        }
        take!(
            tool_calls,
            parallel_tool_calls,
            streaming,
            reasoning_content,
            context_window,
            max_output_tokens
        );
    }
}

/// Partial profile as written in `providers.toml` or `settings.json`
/// (`providers.<id>`). Every field is optional so settings can patch a
/// single field of a built-in profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderProfileConfig {
    #[serde(default)]
    pub kind: Option<ProviderKind>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(default)]
    pub capabilities: Option<ProviderCapabilities>,
}

impl ProviderProfileConfig {
    /// Field-wise merge: `Some` values in `src` win.
    pub fn merge(&mut self, src: ProviderProfileConfig) {
        if src.kind.is_some() {
            self.kind = src.kind;
        }
        if src.base_url.is_some() {
            self.base_url = src.base_url;
        }
        if src.api_key_env.is_some() {
            self.api_key_env = src.api_key_env;
        }
        if src.default_model.is_some() {
            self.default_model = src.default_model;
        }
        if src.max_tokens_field.is_some() {
            self.max_tokens_field = src.max_tokens_field;
        }
        if let Some(c) = src.capabilities {
            self.capabilities
                .get_or_insert_with(Default::default)
                .merge(c);
        }
    }
}

/// A fully resolved profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderProfile {
    pub id: String,
    pub kind: ProviderKind,
    /// `None` = the provider's built-in default endpoint.
    pub base_url: Option<String>,
    /// Name of the env var holding the API key (never the key itself).
    pub api_key_env: Option<String>,
    pub default_model: Option<String>,
    pub max_tokens_field: MaxTokensField,
    pub capabilities: ProviderCapabilities,
}

impl ProviderProfile {
    /// Host (and port, if any) of `base_url`, with scheme, userinfo, and
    /// path stripped. Safe to record in run metadata.
    pub fn base_url_host(&self) -> Option<String> {
        url_host(self.base_url.as_deref()?)
    }
}

/// Host (and port) of `url` with scheme, userinfo, path, and query stripped.
pub fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    (!host.is_empty()).then(|| host.to_string())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("unknown provider profile {id:?}; known profiles: {}", known.join(", "))]
    Unknown { id: String, known: Vec<String> },
    #[error("provider profile {0:?} has no `kind` (expected anthropic | openai-chat | gemini)")]
    MissingKind(String),
    #[error(
        "provider profile {0:?}: `api_key_env` must name an environment variable \
         (letters, digits, underscore), not contain a key"
    )]
    BadApiKeyEnv(String),
    #[error("built-in providers.toml is invalid: {0}")]
    Builtin(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvidersFile {
    #[serde(default)]
    profiles: BTreeMap<String, ProviderProfileConfig>,
}

/// Parse a `providers.toml` document.
pub fn parse_providers_toml(
    text: &str,
) -> Result<BTreeMap<String, ProviderProfileConfig>, ProfileError> {
    toml::from_str::<ProvidersFile>(text)
        .map(|f| f.profiles)
        .map_err(|e| ProfileError::Builtin(e.to_string()))
}

/// The built-in profiles from [`BUILTIN_PROVIDERS_TOML`].
pub fn builtin_profiles() -> BTreeMap<String, ProviderProfileConfig> {
    parse_providers_toml(BUILTIN_PROVIDERS_TOML).expect("built-in providers.toml must parse")
}

/// Sorted ids of all built-in and settings-defined profiles.
pub fn known_profile_ids(overrides: &BTreeMap<String, ProviderProfileConfig>) -> Vec<String> {
    let mut ids: Vec<String> = builtin_profiles().into_keys().collect();
    for k in overrides.keys() {
        let k = k.to_ascii_lowercase();
        if !ids.contains(&k) {
            ids.push(k);
        }
    }
    ids.sort();
    ids
}

/// Resolve `id` against built-ins patched by `overrides` (settings
/// `providers` map). Ids are case-insensitive.
pub fn resolve_profile(
    id: &str,
    overrides: &BTreeMap<String, ProviderProfileConfig>,
) -> Result<ProviderProfile, ProfileError> {
    let id = id.trim().to_ascii_lowercase();
    let builtin = builtin_profiles();
    let over = overrides
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(&id))
        .map(|(_, v)| v.clone());
    let mut cfg = match (builtin.get(&id).cloned(), over.is_some()) {
        (Some(b), _) => b,
        (None, true) => ProviderProfileConfig::default(),
        (None, false) => {
            return Err(ProfileError::Unknown {
                id,
                known: known_profile_ids(overrides),
            })
        }
    };
    if let Some(o) = over {
        cfg.merge(o);
    }
    let kind = cfg
        .kind
        .ok_or_else(|| ProfileError::MissingKind(id.clone()))?;
    if let Some(env) = cfg.api_key_env.as_deref() {
        if !is_env_var_name(env) {
            return Err(ProfileError::BadApiKeyEnv(id));
        }
    }
    Ok(ProviderProfile {
        id,
        kind,
        base_url: cfg.base_url.filter(|s| !s.trim().is_empty()),
        api_key_env: cfg.api_key_env,
        default_model: cfg.default_model,
        max_tokens_field: cfg.max_tokens_field.unwrap_or_default(),
        capabilities: cfg.capabilities.unwrap_or_default(),
    })
}

fn is_env_var_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(json: serde_json::Value) -> BTreeMap<String, ProviderProfileConfig> {
        serde_json::from_value(json).unwrap()
    }

    /// The checked-in providers.toml parses and contains every required built-in.
    #[test]
    fn builtins_parse_and_cover_required_ids() {
        let b = builtin_profiles();
        for id in ["anthropic", "openai", "deepseek", "gemini", "local"] {
            assert!(b.contains_key(id), "missing built-in {id}");
            resolve_profile(id, &BTreeMap::new()).unwrap();
        }
    }

    /// The deepseek built-in is an openai-chat profile with its own env var and endpoint.
    #[test]
    fn deepseek_builtin_profile() {
        let p = resolve_profile("DeepSeek", &BTreeMap::new()).unwrap();
        assert_eq!(p.id, "deepseek");
        assert_eq!(p.kind, ProviderKind::OpenAiChat);
        assert_eq!(p.base_url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(p.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(p.default_model.as_deref(), Some("deepseek-chat"));
        assert_eq!(p.max_tokens_field, MaxTokensField::MaxTokens);
    }

    /// Capabilities that a profile does not declare resolve to explicit `None`.
    #[test]
    fn unknown_capabilities_are_none() {
        let p = resolve_profile("local", &BTreeMap::new()).unwrap();
        assert_eq!(p.capabilities, ProviderCapabilities::default());
        assert_eq!(p.capabilities.tool_calls, None);
        assert_eq!(p.capabilities.context_window, None);
        let d = resolve_profile("deepseek", &BTreeMap::new()).unwrap();
        assert_eq!(d.capabilities.parallel_tool_calls, None);
    }

    /// Settings patch individual fields of a built-in without clobbering the rest.
    #[test]
    fn settings_patch_builtin_field_wise() {
        let o = overrides(serde_json::json!({
            "deepseek": { "base_url": "http://127.0.0.1:9", "capabilities": { "parallel_tool_calls": false } }
        }));
        let p = resolve_profile("deepseek", &o).unwrap();
        assert_eq!(p.base_url.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(p.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(p.capabilities.parallel_tool_calls, Some(false));
        assert_eq!(p.capabilities.tool_calls, Some(true));
    }

    /// Settings can add a brand-new profile, which must declare a kind.
    #[test]
    fn settings_add_custom_profile() {
        let o = overrides(serde_json::json!({
            "vllm": { "kind": "openai-chat", "base_url": "http://gpu:8000", "api_key_env": "VLLM_KEY", "default_model": "qwen" }
        }));
        let p = resolve_profile("vllm", &o).unwrap();
        assert_eq!(p.kind, ProviderKind::OpenAiChat);
        assert!(known_profile_ids(&o).contains(&"vllm".to_string()));
        let o = overrides(serde_json::json!({ "nokind": { "base_url": "http://x" } }));
        assert_eq!(
            resolve_profile("nokind", &o),
            Err(ProfileError::MissingKind("nokind".into()))
        );
    }

    /// Unknown ids produce an error listing the known profiles.
    #[test]
    fn unknown_profile_errors() {
        let err = resolve_profile("nope", &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("deepseek"));
    }

    /// A profile rejects an inline `api_key` field so secrets cannot live in settings.
    #[test]
    fn inline_api_key_is_rejected() {
        let res: Result<BTreeMap<String, ProviderProfileConfig>, _> =
            serde_json::from_value(serde_json::json!({
                "deepseek": { "api_key": "sk-super-secret-value" }
            }));
        let err = res.unwrap_err().to_string();
        assert!(err.contains("api_key"));
        assert!(!err.contains("sk-super-secret-value"));
    }

    /// `api_key_env` holding something that looks like a key is rejected without echoing it.
    #[test]
    fn api_key_env_must_be_a_name() {
        let o = overrides(serde_json::json!({ "deepseek": { "api_key_env": "sk-abc-123" } }));
        let err = resolve_profile("deepseek", &o).unwrap_err();
        assert_eq!(err, ProfileError::BadApiKeyEnv("deepseek".into()));
        assert!(!err.to_string().contains("sk-abc-123"));
    }

    /// base_url_host strips scheme, userinfo, and path but keeps the port.
    #[test]
    fn base_url_host_strips_everything_but_host() {
        let mut p = resolve_profile("deepseek", &BTreeMap::new()).unwrap();
        assert_eq!(p.base_url_host().as_deref(), Some("api.deepseek.com"));
        p.base_url = Some("http://user:secret@127.0.0.1:8080/v1?x=1".into());
        assert_eq!(p.base_url_host().as_deref(), Some("127.0.0.1:8080"));
        p.base_url = None;
        assert_eq!(p.base_url_host(), None);
    }
}
