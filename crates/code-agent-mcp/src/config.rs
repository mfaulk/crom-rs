use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{McpError, McpResult};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpOptions {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_file: Option<PathBuf>,
    pub max_servers: usize,
    pub max_tools_per_server: usize,
    pub max_tool_result_bytes: usize,
    #[serde(with = "duration_seconds")]
    pub startup_timeout: Duration,
    #[serde(with = "duration_seconds")]
    pub request_timeout: Duration,
    pub external_effect_policy: McpExternalEffectPolicy,
    pub load_resources: bool,
    pub load_prompts: bool,
    pub strict: bool,
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl Default for McpOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            config_file: None,
            max_servers: 10,
            max_tools_per_server: 200,
            max_tool_result_bytes: 262_144,
            startup_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            external_effect_policy: McpExternalEffectPolicy::RequireExplicitAllowRule,
            load_resources: false,
            load_prompts: false,
            strict: false,
            servers: BTreeMap::new(),
        }
    }
}

impl McpOptions {
    pub fn limits(&self) -> McpLimits {
        McpLimits {
            max_tool_result_bytes: self.max_tool_result_bytes,
            max_tools_per_server: self.max_tools_per_server,
            max_servers: self.max_servers,
            startup_timeout: self.startup_timeout,
            request_timeout: self.request_timeout,
            max_stderr_bytes: 65_536,
        }
    }

    pub fn secret_values(&self) -> Vec<String> {
        let mut out = Vec::new();
        for server in self.servers.values() {
            match server {
                McpServerConfig::Stdio(cfg) => {
                    for (key, value) in &cfg.env {
                        if looks_like_secret_key(key) || should_redact_value(value) {
                            out.push(value.clone());
                        }
                    }
                }
                McpServerConfig::StreamableHttp(cfg) => {
                    for (key, value) in &cfg.headers {
                        if looks_like_secret_key(key) || should_redact_value(value) {
                            out.push(value.clone());
                        }
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }

    pub fn redacted_summary(&self) -> Value {
        let mut servers = serde_json::Map::new();
        for (name, cfg) in &self.servers {
            servers.insert(name.clone(), cfg.redacted_value());
        }
        serde_json::json!({
            "enabled": self.enabled,
            "config_file": self.config_file,
            "max_servers": self.max_servers,
            "max_tools_per_server": self.max_tools_per_server,
            "max_tool_result_bytes": self.max_tool_result_bytes,
            "startup_timeout_seconds": self.startup_timeout.as_secs(),
            "request_timeout_seconds": self.request_timeout.as_secs(),
            "external_effect_policy": self.external_effect_policy,
            "load_resources": self.load_resources,
            "load_prompts": self.load_prompts,
            "strict": self.strict,
            "servers": servers,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpExternalEffectPolicy {
    AllowWithApproval,
    RequireExplicitAllowRule,
    DenyMutatingNameByDefault,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpLimits {
    pub max_tool_result_bytes: usize,
    pub max_tools_per_server: usize,
    pub max_servers: usize,
    #[serde(with = "duration_seconds")]
    pub startup_timeout: Duration,
    #[serde(with = "duration_seconds")]
    pub request_timeout: Duration,
    pub max_stderr_bytes: usize,
}

impl Default for McpLimits {
    fn default() -> Self {
        McpOptions::default().limits()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    Stdio(StdioMcpServerConfig),
    #[serde(rename = "http")]
    StreamableHttp(StreamableHttpMcpServerConfig),
}

impl McpServerConfig {
    pub fn startup_timeout(&self) -> Option<Duration> {
        match self {
            Self::Stdio(c) => c.startup_timeout,
            Self::StreamableHttp(c) => c.startup_timeout,
        }
    }

    pub fn request_timeout(&self) -> Option<Duration> {
        match self {
            Self::Stdio(c) => c.request_timeout,
            Self::StreamableHttp(c) => c.request_timeout,
        }
    }

    pub fn redacted_value(&self) -> Value {
        match self {
            Self::Stdio(c) => {
                let mut env = serde_json::Map::new();
                for (k, v) in &c.env {
                    env.insert(
                        k.clone(),
                        if looks_like_secret_key(k) || should_redact_value(v) {
                            Value::String("[REDACTED]".into())
                        } else {
                            Value::String(v.clone())
                        },
                    );
                }
                serde_json::json!({
                    "type": "stdio",
                    "command": c.command,
                    "args": c.args,
                    "env": env,
                    "cwd": c.cwd,
                    "startup_timeout_seconds": c.startup_timeout.map(|d| d.as_secs()),
                    "request_timeout_seconds": c.request_timeout.map(|d| d.as_secs()),
                })
            }
            Self::StreamableHttp(c) => {
                let mut headers = serde_json::Map::new();
                for (k, v) in &c.headers {
                    headers.insert(
                        k.clone(),
                        if looks_like_secret_key(k) || should_redact_value(v) {
                            Value::String("[REDACTED]".into())
                        } else {
                            Value::String(v.clone())
                        },
                    );
                }
                serde_json::json!({
                    "type": "http",
                    "url": c.url,
                    "headers": headers,
                    "startup_timeout_seconds": c.startup_timeout.map(|d| d.as_secs()),
                    "request_timeout_seconds": c.request_timeout.map(|d| d.as_secs()),
                })
            }
        }
    }
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawServerConfig::deserialize(deserializer)?;
        raw.into_config().map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StdioMcpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default, with = "option_duration_seconds")]
    pub startup_timeout: Option<Duration>,
    #[serde(default, with = "option_duration_seconds")]
    pub request_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StreamableHttpMcpServerConfig {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default, with = "option_duration_seconds")]
    pub startup_timeout: Option<Duration>,
    #[serde(default, with = "option_duration_seconds")]
    pub request_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpConfigFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub config_file: Option<PathBuf>,
    #[serde(default)]
    pub max_servers: Option<usize>,
    #[serde(default)]
    pub max_tools_per_server: Option<usize>,
    #[serde(default)]
    pub max_tool_result_bytes: Option<usize>,
    #[serde(default)]
    pub startup_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub request_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub external_effect_policy: Option<McpExternalEffectPolicy>,
    #[serde(default)]
    pub load_resources: Option<bool>,
    #[serde(default)]
    pub load_prompts: Option<bool>,
    #[serde(default)]
    pub strict: Option<bool>,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

pub fn load_mcp_options(
    cwd: &Path,
    settings: Option<&McpSettingsConfig>,
    explicit_config_file: Option<&Path>,
    enabled_override: Option<bool>,
    strict_override: Option<bool>,
    timeout_override: Option<Duration>,
) -> McpResult<McpOptions> {
    let mut options = McpOptions::default();
    if let Some(s) = settings {
        apply_settings(&mut options, s);
    }

    let config_file = explicit_config_file
        .map(Path::to_path_buf)
        .or_else(|| options.config_file.clone())
        .or_else(|| {
            let default_path = cwd.join(".agent").join("mcp.json");
            default_path
                .exists()
                .then_some(PathBuf::from(".agent/mcp.json"))
        });

    if let Some(path) = &config_file {
        let absolute = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        };
        if absolute.exists() {
            let file = read_mcp_config_file(&absolute)?;
            options.servers.extend(file.mcp_servers);
            options.config_file = Some(path.clone());
            if explicit_config_file.is_some() || settings.and_then(|s| s.enabled).is_none() {
                options.enabled = true;
            }
        } else if explicit_config_file.is_some() {
            return Err(McpError::config(absolute, "MCP config file does not exist"));
        }
    }

    if let Some(s) = settings {
        options.servers.extend(s.servers.clone());
    }
    if let Some(enabled) = enabled_override {
        options.enabled = enabled;
    }
    if let Some(strict) = strict_override {
        options.strict = strict;
    }
    if let Some(timeout) = timeout_override {
        options.startup_timeout = timeout;
        options.request_timeout = timeout;
    }
    interpolate_options(&mut options)?;
    validate_options(&options)?;
    Ok(options)
}

pub fn merge_mcp_options(base: &mut McpOptions, overlay: McpOptions) {
    base.enabled = overlay.enabled;
    base.config_file = overlay.config_file.or_else(|| base.config_file.clone());
    base.max_servers = overlay.max_servers;
    base.max_tools_per_server = overlay.max_tools_per_server;
    base.max_tool_result_bytes = overlay.max_tool_result_bytes;
    base.startup_timeout = overlay.startup_timeout;
    base.request_timeout = overlay.request_timeout;
    base.external_effect_policy = overlay.external_effect_policy;
    base.load_resources = overlay.load_resources;
    base.load_prompts = overlay.load_prompts;
    base.strict = overlay.strict;
    base.servers.extend(overlay.servers);
}

fn read_mcp_config_file(path: &Path) -> McpResult<McpConfigFile> {
    let bytes = std::fs::read(path).map_err(|e| McpError::Config {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    serde_json::from_slice(&bytes).map_err(|e| McpError::Config {
        path: path.to_path_buf(),
        message: format!("invalid JSON: {e}"),
    })
}

fn apply_settings(options: &mut McpOptions, s: &McpSettingsConfig) {
    if let Some(v) = s.enabled {
        options.enabled = v;
    }
    if let Some(v) = &s.config_file {
        options.config_file = Some(v.clone());
    }
    if let Some(v) = s.max_servers {
        options.max_servers = v;
    }
    if let Some(v) = s.max_tools_per_server {
        options.max_tools_per_server = v;
    }
    if let Some(v) = s.max_tool_result_bytes {
        options.max_tool_result_bytes = v;
    }
    if let Some(v) = s.startup_timeout_seconds {
        options.startup_timeout = Duration::from_secs(v);
    }
    if let Some(v) = s.request_timeout_seconds {
        options.request_timeout = Duration::from_secs(v);
    }
    if let Some(v) = s.external_effect_policy {
        options.external_effect_policy = v;
    }
    if let Some(v) = s.load_resources {
        options.load_resources = v;
    }
    if let Some(v) = s.load_prompts {
        options.load_prompts = v;
    }
    if let Some(v) = s.strict {
        options.strict = v;
    }
    options.servers.extend(s.servers.clone());
}

fn interpolate_options(options: &mut McpOptions) -> McpResult<()> {
    for (name, server) in &mut options.servers {
        match server {
            McpServerConfig::Stdio(cfg) => {
                cfg.command = interpolate_str(&cfg.command, &format!("{name}.command"))?;
                for (idx, arg) in cfg.args.iter_mut().enumerate() {
                    *arg = interpolate_str(arg, &format!("{name}.args[{idx}]"))?;
                }
                for (key, value) in &mut cfg.env {
                    *value = interpolate_str(value, &format!("{name}.env.{key}"))?;
                }
            }
            McpServerConfig::StreamableHttp(cfg) => {
                cfg.url = interpolate_str(&cfg.url, &format!("{name}.url"))?;
                for (key, value) in &mut cfg.headers {
                    *value = interpolate_str(value, &format!("{name}.headers.{key}"))?;
                }
            }
        }
    }
    Ok(())
}

fn validate_options(options: &McpOptions) -> McpResult<()> {
    if options.max_servers == 0 {
        return Err(McpError::InvalidConfig(
            "mcp.max_servers must be greater than zero".into(),
        ));
    }
    if options.max_tools_per_server == 0 {
        return Err(McpError::InvalidConfig(
            "mcp.max_tools_per_server must be greater than zero".into(),
        ));
    }
    if options.max_tool_result_bytes == 0 {
        return Err(McpError::InvalidConfig(
            "mcp.max_tool_result_bytes must be greater than zero".into(),
        ));
    }
    if options.servers.len() > options.max_servers {
        return Err(McpError::InvalidConfig(format!(
            "configured MCP servers ({}) exceeds max_servers ({})",
            options.servers.len(),
            options.max_servers
        )));
    }
    for (name, cfg) in &options.servers {
        if !crate::registry::is_identifier_safe(name) {
            return Err(McpError::InvalidConfig(format!(
                "unsafe MCP server name {name:?}; expected [A-Za-z_][A-Za-z0-9_]*"
            )));
        }
        match cfg {
            McpServerConfig::Stdio(c) if c.command.trim().is_empty() => {
                return Err(McpError::InvalidConfig(format!(
                    "MCP server {name} stdio command is required"
                )));
            }
            McpServerConfig::StreamableHttp(c) if c.url.trim().is_empty() => {
                return Err(McpError::InvalidConfig(format!(
                    "MCP server {name} http url is required"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn interpolate_str(input: &str, field: &str) -> McpResult<String> {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return Ok(out);
        };
        let var = &after[..end];
        if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(McpError::InvalidConfig(format!(
                "invalid environment interpolation {var:?} in {field}"
            )));
        }
        let value = std::env::var(var).map_err(|_| McpError::MissingEnv {
            var: var.to_string(),
            field: field.to_string(),
        })?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[derive(Debug, Clone, Deserialize)]
struct RawServerConfig {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    startup_timeout_seconds: Option<u64>,
    #[serde(default)]
    request_timeout_seconds: Option<u64>,
}

impl RawServerConfig {
    fn into_config(self) -> Result<McpServerConfig, String> {
        let startup_timeout = self.startup_timeout_seconds.map(Duration::from_secs);
        let request_timeout = self.request_timeout_seconds.map(Duration::from_secs);
        match self.kind.as_str() {
            "stdio" => Ok(McpServerConfig::Stdio(StdioMcpServerConfig {
                command: self
                    .command
                    .ok_or_else(|| "stdio MCP server missing `command`".to_string())?,
                args: self.args,
                env: self.env,
                cwd: self.cwd,
                startup_timeout,
                request_timeout,
            })),
            "http" | "streamable_http" | "streamable-http" => Ok(McpServerConfig::StreamableHttp(
                StreamableHttpMcpServerConfig {
                    url: self
                        .url
                        .ok_or_else(|| "http MCP server missing `url`".to_string())?,
                    headers: self.headers,
                    startup_timeout,
                    request_timeout,
                },
            )),
            other => Err(format!(
                "unsupported MCP server type {other:?}; expected stdio or http"
            )),
        }
    }
}

pub fn redact_json_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if looks_like_secret_key(k) {
                    out.insert(k.clone(), Value::String("[REDACTED]".into()));
                } else {
                    out.insert(k.clone(), redact_json_value(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact_json_value).collect()),
        other => other.clone(),
    }
}

pub fn redact_value_with_secrets(v: &Value, secrets: &[String]) -> Value {
    let by_key = redact_json_value(v);
    match by_key {
        Value::String(s) => Value::String(redact_text_with_secrets(&s, secrets)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_value_with_secrets(item, secrets))
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k, redact_value_with_secrets(&val, secrets));
            }
            Value::Object(out)
        }
        other => other,
    }
}

pub fn redact_text_with_secrets(input: &str, secrets: &[String]) -> String {
    // Defense in depth: even when `secrets` is populated, every entry must
    // independently look secret-shaped before we substring-replace it in
    // tool output. Otherwise an attacker who controls an env value or
    // header (e.g. via `MCP_FOO=permission denied`) could poison the list
    // to redact arbitrary text from diagnostics.
    let mut out = input.to_string();
    for secret in secrets {
        if should_redact_value(secret) {
            out = out.replace(secret, "[REDACTED]");
        }
    }
    out
}

pub fn looks_like_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("authorization")
        || key.contains("token")
        || key.contains("secret")
        || key.contains("api_key")
        || key.contains("apikey")
        || key.contains("password")
        || key.contains("credential")
}

/// Conservative "this string is shaped like a real secret" heuristic.
///
/// Length alone is not enough — benign values like `User-Agent: my-app/1.2.3`
/// or `LANG=en_US.UTF-8` happily exceed any size bar. We require either an
/// explicit auth-scheme prefix (`Bearer ` / `Basic ` / `Token `, RFC 7235)
/// or a key-shaped value: 24+ contiguous non-whitespace bytes spanning at
/// least three character classes (upper, lower, digit, non-alphanumeric).
fn should_redact_value(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("bearer ") || lower.starts_with("basic ") || lower.starts_with("token ") {
        return true;
    }
    if trimmed.len() < 24 {
        return false;
    }
    if trimmed.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let mut classes = 0u8;
    if trimmed.chars().any(|c| c.is_ascii_lowercase()) {
        classes += 1;
    }
    if trimmed.chars().any(|c| c.is_ascii_uppercase()) {
        classes += 1;
    }
    if trimmed.chars().any(|c| c.is_ascii_digit()) {
        classes += 1;
    }
    if trimmed
        .chars()
        .any(|c| !c.is_ascii_alphanumeric() && !c.is_whitespace())
    {
        classes += 1;
    }
    classes >= 3
}

mod duration_seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(duration.as_secs())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Duration::from_secs(u64::deserialize(deserializer)?))
    }
}

mod option_duration_seconds {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(duration: &Option<Duration>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match duration {
            Some(d) => serializer.serialize_some(&d.as_secs()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<u64>::deserialize(deserializer)?.map(Duration::from_secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // `.agent/mcp.json` loads stdio and HTTP server definitions.
    fn loads_mcp_json() {
        let dir = TempDir::new().unwrap();
        let agent = dir.path().join(".agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "fake": {"type": "stdio", "command": "fake"},
                    "docs": {"type": "http", "url": "http://127.0.0.1/mcp"}
                }
            })
            .to_string(),
        )
        .unwrap();
        let options = load_mcp_options(dir.path(), None, None, None, None, None).unwrap();
        assert!(options.enabled);
        assert_eq!(options.servers.len(), 2);
    }

    #[test]
    // Embedded settings servers override same-name file servers deterministically.
    fn settings_servers_override_file_servers() {
        let dir = TempDir::new().unwrap();
        let agent = dir.path().join(".agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(
            agent.join("mcp.json"),
            serde_json::json!({
                "mcpServers": {"fake": {"type": "stdio", "command": "from-file"}}
            })
            .to_string(),
        )
        .unwrap();
        let mut settings = McpSettingsConfig {
            enabled: Some(true),
            ..McpSettingsConfig::default()
        };
        settings.servers.insert(
            "fake".into(),
            McpServerConfig::Stdio(StdioMcpServerConfig {
                command: "from-settings".into(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                startup_timeout: None,
                request_timeout: None,
            }),
        );
        let options =
            load_mcp_options(dir.path(), Some(&settings), None, None, None, None).unwrap();
        match options.servers.get("fake").unwrap() {
            McpServerConfig::Stdio(c) => assert_eq!(c.command, "from-settings"),
            _ => panic!("expected stdio"),
        }
    }

    #[test]
    // Environment interpolation replaces exactly `${VAR}` placeholders.
    fn environment_interpolation_works() {
        std::env::set_var("CODE_AGENT_MCP_TEST_TOKEN", "abc123secretxyz");
        let dir = TempDir::new().unwrap();
        let settings = McpSettingsConfig {
            enabled: Some(true),
            servers: BTreeMap::from([(
                "fake".into(),
                McpServerConfig::Stdio(StdioMcpServerConfig {
                    command: "fake".into(),
                    args: vec!["${CODE_AGENT_MCP_TEST_TOKEN}".into()],
                    env: BTreeMap::from([("TOKEN".into(), "${CODE_AGENT_MCP_TEST_TOKEN}".into())]),
                    cwd: None,
                    startup_timeout: None,
                    request_timeout: None,
                }),
            )]),
            ..McpSettingsConfig::default()
        };
        let options =
            load_mcp_options(dir.path(), Some(&settings), None, None, None, None).unwrap();
        match options.servers.get("fake").unwrap() {
            McpServerConfig::Stdio(c) => {
                assert_eq!(c.args[0], "abc123secretxyz");
                assert_eq!(c.env["TOKEN"], "abc123secretxyz");
            }
            _ => panic!("expected stdio"),
        }
        std::env::remove_var("CODE_AGENT_MCP_TEST_TOKEN");
    }

    #[test]
    // Missing environment variables fail with a clear structured error.
    fn missing_environment_variable_errors() {
        let dir = TempDir::new().unwrap();
        let settings = McpSettingsConfig {
            enabled: Some(true),
            servers: BTreeMap::from([(
                "fake".into(),
                McpServerConfig::Stdio(StdioMcpServerConfig {
                    command: "fake".into(),
                    args: vec!["${CODE_AGENT_MCP_TEST_MISSING}".into()],
                    env: BTreeMap::new(),
                    cwd: None,
                    startup_timeout: None,
                    request_timeout: None,
                }),
            )]),
            ..McpSettingsConfig::default()
        };
        let err =
            load_mcp_options(dir.path(), Some(&settings), None, None, None, None).unwrap_err();
        assert!(matches!(err, McpError::MissingEnv { .. }));
    }

    #[test]
    // Redacted summaries hide authorization-style headers.
    fn redacts_header_secrets() {
        let options = McpOptions {
            enabled: true,
            servers: BTreeMap::from([(
                "docs".into(),
                McpServerConfig::StreamableHttp(StreamableHttpMcpServerConfig {
                    url: "http://127.0.0.1/mcp".into(),
                    headers: BTreeMap::from([("Authorization".into(), "Bearer abc".into())]),
                    startup_timeout: None,
                    request_timeout: None,
                }),
            )]),
            ..McpOptions::default()
        };
        let summary = options.redacted_summary();
        assert_eq!(
            summary["servers"]["docs"]["headers"]["Authorization"],
            "[REDACTED]"
        );
    }

    /// `should_redact_value` accepts strings that look like real secrets:
    /// auth-scheme prefixes and high-entropy key-shaped values.
    #[test]
    fn should_redact_value_flags_real_secrets() {
        assert!(super::should_redact_value("Bearer abc"));
        assert!(super::should_redact_value("bearer abc"));
        assert!(super::should_redact_value("Basic dXNlcjpwYXNz"));
        assert!(super::should_redact_value("Token abc"));
        // Long alphanumeric mixed-case secrets.
        assert!(super::should_redact_value(
            "sk-proj-AbCdEf1234567890XyZqrstuvWxyz"
        ));
        // JWT-shaped tokens are alphanumeric with dots/dashes (4 classes).
        assert!(super::should_redact_value(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkw.abcdef-ghijk"
        ));
        // Pure-upper or pure-lower 24+ char keys aren't caught by the
        // entropy heuristic on their own — they rely on the env-var
        // name (`looks_like_secret_key`) to be redacted. That is
        // intentional: AWS_SECRET_ACCESS_KEY-shaped values share too
        // much structure with arbitrary uppercase prose for the
        // value heuristic alone to be safe.
    }

    /// Benign long strings that the previous length-only heuristic would
    /// have flagged must no longer be added to the redaction list.
    #[test]
    fn should_redact_value_rejects_benign_long_strings() {
        // The issue's two motivating examples.
        assert!(!super::should_redact_value("code-agent/1.2.3"));
        assert!(!super::should_redact_value("en_US.UTF-8"));
        // Common header values.
        assert!(!super::should_redact_value(
            "Mozilla/5.0 (X11; Linux x86_64)"
        ));
        assert!(!super::should_redact_value("application/json"));
        // Attacker-shaped poisoning probes (contain "token"/"secret"
        // substring but no actual key shape).
        assert!(!super::should_redact_value("permission denied"));
        assert!(!super::should_redact_value("the access token has expired"));
        assert!(!super::should_redact_value("secret recipe"));
        // Pure lowercase or no-digit values fail the class check.
        assert!(!super::should_redact_value("abcdefghijklmnopqrstuvwxyz"));
        assert!(!super::should_redact_value("AAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    /// `redact_text_with_secrets` ignores list entries that fail the
    /// secret-shape check, so an attacker who poisons the list with
    /// arbitrary text can no longer hide that text from output.
    #[test]
    fn redact_text_with_secrets_resists_poisoning() {
        let secrets = vec!["permission denied".to_string()];
        let out = redact_text_with_secrets("error: permission denied", &secrets);
        assert_eq!(out, "error: permission denied");

        let real_secret = "sk-proj-AbCdEf1234567890XyZqrstuvWxyz".to_string();
        let secrets = vec![real_secret.clone()];
        let out = redact_text_with_secrets(&format!("key={real_secret}"), &secrets);
        assert!(out.contains("[REDACTED]"));
        assert!(!out.contains(&real_secret));
    }

    /// `secret_values` honors the env-var *name* for stdio servers (not
    /// only the value), so `API_KEY=short` still ends up redactable.
    #[test]
    fn secret_values_uses_stdio_env_key_name() {
        let options = McpOptions {
            enabled: true,
            servers: BTreeMap::from([(
                "fake".into(),
                McpServerConfig::Stdio(StdioMcpServerConfig {
                    command: "fake".into(),
                    args: Vec::new(),
                    env: BTreeMap::from([
                        ("API_KEY".into(), "short".into()),
                        ("LANG".into(), "en_US.UTF-8".into()),
                    ]),
                    cwd: None,
                    startup_timeout: None,
                    request_timeout: None,
                }),
            )]),
            ..McpOptions::default()
        };
        let secrets = options.secret_values();
        assert!(secrets.contains(&"short".to_string()));
        assert!(!secrets.contains(&"en_US.UTF-8".to_string()));
    }

    /// A long but benign header value is NOT added to the redaction list,
    /// so its substrings cannot leak [REDACTED] markers into tool output.
    #[test]
    fn secret_values_excludes_long_benign_headers() {
        let options = McpOptions {
            enabled: true,
            servers: BTreeMap::from([(
                "docs".into(),
                McpServerConfig::StreamableHttp(StreamableHttpMcpServerConfig {
                    url: "http://127.0.0.1/mcp".into(),
                    headers: BTreeMap::from([
                        ("User-Agent".into(), "code-agent/1.2.3".into()),
                        ("Accept".into(), "application/json".into()),
                    ]),
                    startup_timeout: None,
                    request_timeout: None,
                }),
            )]),
            ..McpOptions::default()
        };
        let secrets = options.secret_values();
        assert!(secrets.is_empty(), "unexpected secrets: {secrets:?}");
    }
}
