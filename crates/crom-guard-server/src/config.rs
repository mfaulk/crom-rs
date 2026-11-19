use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crom_guard::{ActionId, AppId};
use serde::Deserialize;

pub const DEFAULT_AGENT_LISTEN: &str = "127.0.0.1:8470";
/// Largest agent request body. Tool arguments are capped separately at
/// `crom_guard::MAX_ARGUMENT_BYTES`; this leaves room for the envelope.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 256 * 1024;
pub const DEFAULT_ADMIN_MAX_REQUEST_BYTES: usize = 64 * 1024;
pub const DEFAULT_REQUEST_READ_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_RESOLVE_TIMEOUT_MS: u64 = 5_000;
pub const DEFAULT_MAX_CONNECTIONS: usize = 256;
pub const DEFAULT_ADMIN_MAX_CONNECTIONS: usize = 16;
/// Denied agent requests a peer may make before it is throttled.
pub const DEFAULT_DENY_BURST: u32 = 20;
pub const DEFAULT_DENY_REFILL_PER_SECOND: f64 = 2.0;
/// Admin tokens shorter than this are refused at startup.
pub const MIN_ADMIN_TOKEN_LEN: usize = 32;

/// `guard.toml`. Relative paths are resolved against the file's directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
    pub admin: AdminConfig,
    pub policy: PolicyConfig,
    pub credentials: CredentialsConfig,
    pub audit: AuditConfig,
    #[serde(default)]
    pub grants: GrantsConfig,
    #[serde(default)]
    pub approvals: ApprovalsConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    #[serde(default = "default_agent_listen")]
    pub listen: SocketAddr,
    /// `Host` header values accepted (DNS-rebinding defense). Empty means
    /// loopback names only.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Required to bind a non-loopback address. Guard serves no TLS; put a
    /// TLS-terminating reverse proxy in front instead.
    #[serde(default)]
    pub allow_non_loopback: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            listen: default_agent_listen(),
            allowed_hosts: Vec::new(),
            allow_non_loopback: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// `unix:/path/to/admin.sock` (created with mode 0600) or a loopback
    /// `host:port`.
    pub listen: AdminListen,
    /// File holding the admin token; must be mode 0600 or 0400 and owned by
    /// Guard's user. Exactly one of `token_file` and `token_env`.
    pub token_file: Option<PathBuf>,
    /// Environment variable holding the admin token.
    pub token_env: Option<String>,
    /// The application the admin credential authenticates as; recorded as
    /// the issuer of every grant and approval created through this listener.
    pub app_id: AppId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminListen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl std::str::FromStr for AdminListen {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.strip_prefix("unix:") {
            Some("") => Err("unix: needs a socket path".into()),
            Some(path) => Ok(Self::Unix(PathBuf::from(path))),
            None => s
                .parse()
                .map(Self::Tcp)
                .map_err(|_| format!("{s:?} is neither unix:<path> nor host:port")),
        }
    }
}

impl<'de> Deserialize<'de> for AdminListen {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl std::fmt::Display for AdminListen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(a) => write!(f, "{a}"),
            Self::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    /// Cedar namespace, e.g. `Support`.
    pub namespace: String,
    pub schema: PathBuf,
    pub policies: PathBuf,
}

/// Exactly one of `file` and `env`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialsConfig {
    pub file: Option<PathBuf>,
    #[serde(default)]
    pub env: Vec<EnvCredentialConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvCredentialConfig {
    pub id: String,
    pub var: String,
    pub actions: Vec<ActionId>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditConfig {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantsConfig {
    #[serde(default = "default_max_ttl")]
    pub max_ttl_seconds: u64,
}

impl Default for GrantsConfig {
    fn default() -> Self {
        Self {
            max_ttl_seconds: default_max_ttl(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsConfig {
    /// Longest approval lifetime the admin may request.
    #[serde(default = "default_approval_ttl")]
    pub max_ttl_seconds: u64,
    /// How long an `approval_required` request stays approvable.
    #[serde(default = "default_pending_ttl")]
    pub pending_ttl_seconds: u64,
}

impl Default for ApprovalsConfig {
    fn default() -> Self {
        Self {
            max_ttl_seconds: default_approval_ttl(),
            pending_ttl_seconds: default_pending_ttl(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    /// Agent endpoint request body cap; larger bodies get 413.
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    #[serde(default = "default_admin_max_request_bytes")]
    pub admin_max_request_bytes: usize,
    /// Bound on reading a request's headers and body. Not applied to
    /// executing a call: an in-flight call always runs to completion.
    #[serde(default = "default_read_timeout")]
    pub request_read_timeout_ms: u64,
    /// Bound on each resource resolution (`Guard::with_resolve_timer`).
    #[serde(default = "default_resolve_timeout")]
    pub resolve_timeout_ms: u64,
    /// Concurrent connections served per listener; further connections
    /// wait in the accept backlog.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    #[serde(default = "default_admin_max_connections")]
    pub admin_max_connections: usize,
    /// Per-peer token bucket for denied agent requests (`tools/call`
    /// denials and `tools/list` session errors). An empty bucket refuses
    /// the peer's requests with `rate_limited` before Guard runs, so a
    /// flood of bad handles cannot flood the audit log.
    #[serde(default = "default_deny_burst")]
    pub deny_burst: u32,
    #[serde(default = "default_deny_refill")]
    pub deny_refill_per_second: f64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: default_max_request_bytes(),
            admin_max_request_bytes: default_admin_max_request_bytes(),
            request_read_timeout_ms: default_read_timeout(),
            resolve_timeout_ms: default_resolve_timeout(),
            max_connections: default_max_connections(),
            admin_max_connections: default_admin_max_connections(),
            deny_burst: default_deny_burst(),
            deny_refill_per_second: default_deny_refill(),
        }
    }
}

fn default_agent_listen() -> SocketAddr {
    DEFAULT_AGENT_LISTEN.parse().expect("valid default")
}
fn default_max_ttl() -> u64 {
    crom_guard::DEFAULT_MAX_TTL_SECONDS
}
fn default_max_request_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BYTES
}
fn default_admin_max_request_bytes() -> usize {
    DEFAULT_ADMIN_MAX_REQUEST_BYTES
}
fn default_read_timeout() -> u64 {
    DEFAULT_REQUEST_READ_TIMEOUT_MS
}
fn default_resolve_timeout() -> u64 {
    DEFAULT_RESOLVE_TIMEOUT_MS
}
fn default_approval_ttl() -> u64 {
    crom_guard::DEFAULT_APPROVAL_MAX_TTL_SECONDS
}
fn default_pending_ttl() -> u64 {
    crom_guard::DEFAULT_PENDING_TTL_SECONDS
}
fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}
fn default_admin_max_connections() -> usize {
    DEFAULT_ADMIN_MAX_CONNECTIONS
}
fn default_deny_burst() -> u32 {
    DEFAULT_DENY_BURST
}
fn default_deny_refill() -> f64 {
    DEFAULT_DENY_REFILL_PER_SECOND
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.to_owned(),
            message: e.to_string(),
        })?;
        let mut config = Self::parse(&text).map_err(|message| ConfigError::Parse {
            path: path.to_owned(),
            message,
        })?;
        config.resolve_paths(path.parent().unwrap_or(Path::new(".")));
        Ok(config)
    }

    /// Parses TOML. The config holds paths and variable names, never
    /// secrets, so parse errors may quote it.
    pub fn parse(text: &str) -> Result<Self, String> {
        toml::from_str(text).map_err(|e| e.to_string())
    }

    fn resolve_paths(&mut self, base: &Path) {
        let fix = |p: &mut PathBuf| {
            if p.is_relative() {
                *p = base.join(&*p);
            }
        };
        fix(&mut self.policy.schema);
        fix(&mut self.policy.policies);
        fix(&mut self.audit.path);
        if let Some(p) = &mut self.credentials.file {
            fix(p);
        }
        if let Some(p) = &mut self.admin.token_file {
            fix(p);
        }
        if let AdminListen::Unix(p) = &mut self.admin.listen {
            fix(p);
        }
    }

    /// Checks that do not touch the filesystem.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: &str| Err(ConfigError::Invalid(m.to_owned()));
        if !self.agent.listen.ip().is_loopback() && !self.agent.allow_non_loopback {
            return invalid(
                "agent.listen is not a loopback address; set agent.allow_non_loopback = true \
                 and terminate TLS in a reverse proxy",
            );
        }
        if let AdminListen::Tcp(a) = &self.admin.listen {
            // No TLS and a bearer token: TCP admin is loopback-only. Reach
            // it remotely through a unix socket and an external tunnel.
            if !a.ip().is_loopback() {
                return invalid("admin.listen must be a loopback address or a unix socket");
            }
            if *a == self.agent.listen && a.port() != 0 {
                return invalid("admin.listen must differ from agent.listen");
            }
        }
        match (&self.admin.token_file, &self.admin.token_env) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => return invalid("set exactly one of admin.token_file and admin.token_env"),
        }
        match (&self.credentials.file, self.credentials.env.is_empty()) {
            (Some(_), true) | (None, false) => {}
            _ => return invalid("set exactly one of credentials.file and credentials.env"),
        }
        let l = &self.limits;
        if l.max_request_bytes == 0 || l.admin_max_request_bytes == 0 {
            return invalid("request size limits must be positive");
        }
        if l.request_read_timeout_ms == 0 || l.resolve_timeout_ms == 0 {
            return invalid("timeouts must be positive");
        }
        if l.max_connections == 0 || l.admin_max_connections == 0 {
            return invalid("connection limits must be positive");
        }
        let refill = l.deny_refill_per_second;
        if l.deny_burst == 0 || !refill.is_finite() || refill <= 0.0 {
            return invalid("limits.deny_burst and limits.deny_refill_per_second must be positive");
        }
        let ttl = self.grants.max_ttl_seconds;
        if ttl == 0 || ttl > crom_guard::DEFAULT_MAX_TTL_SECONDS {
            return Err(ConfigError::Invalid(format!(
                "grants.max_ttl_seconds must be between 1 and {}",
                crom_guard::DEFAULT_MAX_TTL_SECONDS
            )));
        }
        let a = &self.approvals;
        if a.max_ttl_seconds == 0 || a.pending_ttl_seconds == 0 {
            return invalid("approval TTLs must be positive");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [admin]
        listen = "127.0.0.1:9001"
        token_env = "TOKEN"
        app_id = "app"
        [policy]
        namespace = "Support"
        schema = "s.cedarschema"
        policies = "p.cedar"
        [credentials]
        file = "creds.json"
        [audit]
        path = "audit.jsonl"
    "#;

    /// A minimal config parses with loopback defaults and validates.
    #[test]
    fn minimal_config_defaults_to_loopback() {
        let c = Config::parse(MINIMAL).unwrap();
        assert!(c.agent.listen.ip().is_loopback());
        assert_eq!(c.limits.max_request_bytes, DEFAULT_MAX_REQUEST_BYTES);
        c.validate().unwrap();
    }

    /// Unknown keys are rejected rather than ignored.
    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{MINIMAL}\n[extra]\nx = 1\n");
        assert!(Config::parse(&text).is_err());
    }

    /// A non-loopback agent bind is refused unless explicitly allowed.
    #[test]
    fn non_loopback_agent_bind_needs_opt_in() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.agent.listen = "0.0.0.0:8470".parse().unwrap();
        assert!(c.validate().is_err());
        c.agent.allow_non_loopback = true;
        c.validate().unwrap();
    }

    /// A non-loopback admin TCP bind is refused.
    #[test]
    fn non_loopback_admin_bind_is_refused() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.admin.listen = AdminListen::Tcp("0.0.0.0:9001".parse().unwrap());
        assert!(c.validate().is_err());
    }

    /// The admin and agent listeners may not share an address.
    #[test]
    fn admin_and_agent_must_differ() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.admin.listen = AdminListen::Tcp(c.agent.listen);
        assert!(c.validate().is_err());
    }

    /// Exactly one admin token source must be configured.
    #[test]
    fn exactly_one_token_source() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.admin.token_file = Some("t".into());
        assert!(c.validate().is_err());
        c.admin.token_file = None;
        c.admin.token_env = None;
        assert!(c.validate().is_err());
    }

    /// `grants.max_ttl_seconds` must be within 1..=DEFAULT_MAX_TTL_SECONDS.
    #[test]
    fn grant_ttl_bounds() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.grants.max_ttl_seconds = 0;
        assert!(c.validate().is_err());
        c.grants.max_ttl_seconds = crom_guard::DEFAULT_MAX_TTL_SECONDS + 1;
        assert!(c.validate().is_err());
        c.grants.max_ttl_seconds = crom_guard::DEFAULT_MAX_TTL_SECONDS;
        c.validate().unwrap();
    }

    /// `admin.allow_non_loopback` no longer exists, so a config naming it is rejected.
    #[test]
    fn admin_non_loopback_opt_in_is_gone() {
        let text = MINIMAL.replace(
            "app_id = \"app\"",
            "app_id = \"app\"\nallow_non_loopback = true",
        );
        assert!(Config::parse(&text).is_err());
    }

    /// Zero connection caps and deny budgets are refused.
    #[test]
    fn limits_must_be_positive() {
        let mut c = Config::parse(MINIMAL).unwrap();
        c.limits.max_connections = 0;
        assert!(c.validate().is_err());
        let mut c = Config::parse(MINIMAL).unwrap();
        c.limits.deny_refill_per_second = f64::NAN;
        assert!(c.validate().is_err());
    }

    /// `unix:` admin addresses parse as socket paths and resolve relative to the config.
    #[test]
    fn unix_admin_listen_resolves_relative_path() {
        let text = MINIMAL.replace("127.0.0.1:9001", "unix:run/admin.sock");
        let mut c = Config::parse(&text).unwrap();
        c.resolve_paths(Path::new("/etc/guard"));
        assert_eq!(
            c.admin.listen,
            AdminListen::Unix("/etc/guard/run/admin.sock".into())
        );
        assert_eq!(c.audit.path, PathBuf::from("/etc/guard/audit.jsonl"));
    }
}
