//! Project + user settings loading and merging.
//!
//! Project settings:    `<cwd>/.agent/settings.json`
//! User settings:       `${CODE_AGENT_CONFIG_HOME:-$HOME/.config/code-agent}/settings.json`
//!
//! Project settings override user settings on scalar fields. List-like fields
//! are concatenated (user first, then project), preserving order.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use code_agent_mcp::McpSettingsConfig;

use crate::permission::PermissionMode;
use crate::tool_catalog::ToolSearchSettingsConfig;
use crate::tool_rule::ToolRule;

/// Which settings sources should be loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SettingSource {
    #[default]
    None,
    Project,
    User,
    UserAndProject,
}

impl SettingSource {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "project" => Some(Self::Project),
            "user" => Some(Self::User),
            "user-and-project" | "user_and_project" | "userandproject" | "both" => {
                Some(Self::UserAndProject)
            }
            _ => None,
        }
    }

    pub fn includes_user(self) -> bool {
        matches!(self, Self::User | Self::UserAndProject)
    }

    pub fn includes_project(self) -> bool {
        matches!(self, Self::Project | Self::UserAndProject)
    }
}

/// On-disk schema. All fields optional; merged into `Settings`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<ToolRule>>,
    #[serde(default)]
    pub disallowed_tools: Option<Vec<ToolRule>>,
    #[serde(default)]
    pub tool_limits: Option<ToolLimitsConfig>,
    #[serde(default)]
    pub hooks: Option<HooksConfig>,
    #[serde(default)]
    pub project_instructions_mode: Option<ProjectInstructionsMode>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub checkpointing: Option<CheckpointingConfig>,
    #[serde(default)]
    pub validation: Option<ValidationConfig>,
    #[serde(default)]
    pub mcp: Option<McpSettingsConfig>,
    #[serde(default)]
    pub tool_search: Option<ToolSearchSettingsConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointingConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub failure_mode: Option<String>,
    #[serde(default)]
    pub create_on_user_message: Option<bool>,
    #[serde(default)]
    pub max_checkpoint_bytes_per_file: Option<u64>,
    #[serde(default)]
    pub max_total_checkpoint_bytes_per_session: Option<u64>,
    #[serde(default)]
    pub retention: Option<CheckpointRetentionConfig>,
    #[serde(default)]
    pub bash_policy: Option<String>,
    #[serde(default)]
    pub max_diff_bytes: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointRetentionConfig {
    #[serde(default)]
    pub max_checkpoints_per_session: Option<usize>,
    #[serde(default)]
    pub max_age_days: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationConfig {
    #[serde(default)]
    pub commands: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub rewind_on_failure: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectInstructionsMode {
    #[default]
    AgentsOnly,
    AgentsAndClaudeMd,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLimitsConfig {
    #[serde(default)]
    pub max_read_bytes: Option<usize>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[serde(default)]
    pub bash_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub max_glob_results: Option<usize>,
    #[serde(default)]
    pub max_grep_results: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    #[serde(default)]
    pub block_env_file_writes: Option<bool>,
    #[serde(default)]
    pub block_dangerous_bash: Option<bool>,
    #[serde(default)]
    pub audit_log: Option<bool>,
    #[serde(default)]
    pub secret_redaction: Option<bool>,
    #[serde(default)]
    pub max_patch_size_bytes: Option<usize>,
}

/// Effective merged settings.
#[derive(Debug, Clone, Default)]
pub struct Settings {
    pub model: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub allowed_tools: Vec<ToolRule>,
    pub disallowed_tools: Vec<ToolRule>,
    pub tool_limits: ToolLimitsConfig,
    pub hooks: HooksConfig,
    pub project_instructions_mode: ProjectInstructionsMode,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u32>,
    pub checkpointing: CheckpointingConfig,
    pub validation: ValidationConfig,
    pub mcp: Option<McpSettingsConfig>,
    pub tool_search: ToolSearchSettingsConfig,
    pub source: SettingSource,
    pub project_path: Option<PathBuf>,
    pub user_path: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("settings file at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("settings file at {path}: invalid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("invalid permission_mode: {0}")]
    BadPermissionMode(String),
    #[error("invalid checkpointing.failure_mode: {0}")]
    BadCheckpointFailureMode(String),
    #[error("invalid checkpointing.bash_policy: {0}")]
    BadBashCheckpointPolicy(String),
    #[error("{0}")]
    BadToolSearch(String),
}

/// Load project settings from `<cwd>/.agent/settings.json` if present.
pub fn project_settings_path(cwd: &Path) -> PathBuf {
    cwd.join(".agent").join("settings.json")
}

/// Resolve the user-settings path. Honors `CODE_AGENT_CONFIG_HOME` first,
/// then `XDG_CONFIG_HOME`, then `$HOME/.config/code-agent`.
pub fn user_settings_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CODE_AGENT_CONFIG_HOME") {
        return Some(PathBuf::from(p).join("settings.json"));
    }
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(p).join("code-agent").join("settings.json"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(
            PathBuf::from(home)
                .join(".config")
                .join("code-agent")
                .join("settings.json"),
        );
    }
    None
}

/// Load and merge settings according to `source`.
pub fn load(cwd: &Path, source: SettingSource) -> Result<Settings, SettingsError> {
    if matches!(source, SettingSource::None) {
        return Ok(Settings {
            source: SettingSource::None,
            ..Settings::default()
        });
    }

    let mut user_settings: Option<SettingsFile> = None;
    let mut user_path_used: Option<PathBuf> = None;
    if source.includes_user() {
        if let Some(p) = user_settings_path() {
            if p.exists() {
                user_path_used = Some(p.clone());
                user_settings = Some(read_file(&p)?);
            }
        }
    }

    let mut project_settings: Option<SettingsFile> = None;
    let mut project_path_used: Option<PathBuf> = None;
    if source.includes_project() {
        let p = project_settings_path(cwd);
        if p.exists() {
            project_path_used = Some(p.clone());
            project_settings = Some(read_file(&p)?);
        }
    }

    let mut effective = Settings {
        source,
        project_path: project_path_used,
        user_path: user_path_used,
        ..Settings::default()
    };

    // Apply user, then project (project overrides user on scalars).
    if let Some(u) = user_settings {
        merge_into(&mut effective, u, true)?;
    }
    if let Some(p) = project_settings {
        merge_into(&mut effective, p, false)?;
    }

    Ok(effective)
}

fn read_file(path: &Path) -> Result<SettingsFile, SettingsError> {
    let bytes = std::fs::read(path).map_err(|e| SettingsError::Read {
        path: path.to_path_buf(),
        source: e,
    })?;
    let s: SettingsFile = serde_json::from_slice(&bytes).map_err(|e| SettingsError::Parse {
        path: path.to_path_buf(),
        source: e,
    })?;
    Ok(s)
}

/// Merge a `SettingsFile` into `dst`. List fields concatenate; scalars
/// overwrite (caller controls precedence by call order).
fn merge_into(dst: &mut Settings, src: SettingsFile, _is_user: bool) -> Result<(), SettingsError> {
    if let Some(m) = src.model {
        dst.model = Some(m);
    }
    if let Some(pm) = src.permission_mode {
        let parsed =
            PermissionMode::from_str_lenient(&pm).ok_or(SettingsError::BadPermissionMode(pm))?;
        dst.permission_mode = Some(parsed);
    }
    if let Some(rules) = src.allowed_tools {
        dst.allowed_tools.extend(rules);
    }
    if let Some(rules) = src.disallowed_tools {
        dst.disallowed_tools.extend(rules);
    }
    if let Some(lim) = src.tool_limits {
        if let Some(v) = lim.max_read_bytes {
            dst.tool_limits.max_read_bytes = Some(v);
        }
        if let Some(v) = lim.max_output_bytes {
            dst.tool_limits.max_output_bytes = Some(v);
        }
        if let Some(v) = lim.bash_timeout_seconds {
            dst.tool_limits.bash_timeout_seconds = Some(v);
        }
        if let Some(v) = lim.max_glob_results {
            dst.tool_limits.max_glob_results = Some(v);
        }
        if let Some(v) = lim.max_grep_results {
            dst.tool_limits.max_grep_results = Some(v);
        }
    }
    if let Some(h) = src.hooks {
        if let Some(v) = h.block_env_file_writes {
            dst.hooks.block_env_file_writes = Some(v);
        }
        if let Some(v) = h.block_dangerous_bash {
            dst.hooks.block_dangerous_bash = Some(v);
        }
        if let Some(v) = h.audit_log {
            dst.hooks.audit_log = Some(v);
        }
        if let Some(v) = h.secret_redaction {
            dst.hooks.secret_redaction = Some(v);
        }
        if let Some(v) = h.max_patch_size_bytes {
            dst.hooks.max_patch_size_bytes = Some(v);
        }
    }
    if let Some(m) = src.project_instructions_mode {
        dst.project_instructions_mode = m;
    }
    if let Some(t) = src.max_turns {
        dst.max_turns = Some(t);
    }
    if let Some(t) = src.max_tokens {
        dst.max_tokens = Some(t);
    }
    if let Some(c) = src.checkpointing {
        if let Some(v) = c.enabled {
            dst.checkpointing.enabled = Some(v);
        }
        if let Some(v) = c.failure_mode {
            // Validate the string value here so an invalid setting fails
            // loudly at load time rather than silently defaulting.
            if !matches!(
                v.to_ascii_lowercase().as_str(),
                "strict" | "best_effort" | "best-effort" | "besteffort"
            ) {
                return Err(SettingsError::BadCheckpointFailureMode(v));
            }
            dst.checkpointing.failure_mode = Some(v);
        }
        if let Some(v) = c.create_on_user_message {
            dst.checkpointing.create_on_user_message = Some(v);
        }
        if let Some(v) = c.max_checkpoint_bytes_per_file {
            dst.checkpointing.max_checkpoint_bytes_per_file = Some(v);
        }
        if let Some(v) = c.max_total_checkpoint_bytes_per_session {
            dst.checkpointing.max_total_checkpoint_bytes_per_session = Some(v);
        }
        if let Some(r) = c.retention {
            let dest_r = dst
                .checkpointing
                .retention
                .get_or_insert(CheckpointRetentionConfig::default());
            if let Some(v) = r.max_checkpoints_per_session {
                dest_r.max_checkpoints_per_session = Some(v);
            }
            if let Some(v) = r.max_age_days {
                dest_r.max_age_days = Some(v);
            }
        }
        if let Some(v) = c.bash_policy {
            if !matches!(
                v.to_ascii_lowercase().as_str(),
                "allow_with_warning"
                    | "allow-with-warning"
                    | "require_explicit_approval"
                    | "require-explicit-approval"
                    | "deny_likely_mutating_commands"
                    | "deny-likely-mutating-commands"
                    | "deny_likely_mutating"
            ) {
                return Err(SettingsError::BadBashCheckpointPolicy(v));
            }
            dst.checkpointing.bash_policy = Some(v);
        }
        if let Some(v) = c.max_diff_bytes {
            dst.checkpointing.max_diff_bytes = Some(v);
        }
    }
    if let Some(v) = src.validation {
        if let Some(c) = v.commands {
            dst.validation.commands = Some(c);
        }
        if let Some(t) = v.timeout_seconds {
            dst.validation.timeout_seconds = Some(t);
        }
        if let Some(r) = v.rewind_on_failure {
            dst.validation.rewind_on_failure = Some(r);
        }
    }
    if let Some(mcp) = src.mcp {
        match &mut dst.mcp {
            Some(existing) => merge_mcp_settings(existing, mcp),
            None => dst.mcp = Some(mcp),
        }
    }
    if let Some(tool_search) = src.tool_search {
        merge_tool_search_settings(&mut dst.tool_search, tool_search);
        let mut options = crate::tool_catalog::ToolSearchOptions::default();
        options
            .apply_settings(&dst.tool_search)
            .map_err(SettingsError::BadToolSearch)?;
    }
    Ok(())
}

fn merge_tool_search_settings(dst: &mut ToolSearchSettingsConfig, src: ToolSearchSettingsConfig) {
    if src.enabled.is_some() {
        dst.enabled = src.enabled;
    }
    if src.mode.is_some() {
        dst.mode = src.mode;
    }
    if src.strategy.is_some() {
        dst.strategy = src.strategy;
    }
    if src.native_provider_mode.is_some() {
        dst.native_provider_mode = src.native_provider_mode;
    }
    if src.activation_threshold_percent.is_some() {
        dst.activation_threshold_percent = src.activation_threshold_percent;
    }
    if src.min_tool_count.is_some() {
        dst.min_tool_count = src.min_tool_count;
    }
    if src.max_results.is_some() {
        dst.max_results = src.max_results;
    }
    if src.max_loaded_tools.is_some() {
        dst.max_loaded_tools = src.max_loaded_tools;
    }
    if src.max_loaded_tool_tokens.is_some() {
        dst.max_loaded_tool_tokens = src.max_loaded_tool_tokens;
    }
    if src.always_loaded.is_some() {
        dst.always_loaded = src.always_loaded;
    }
    if src.deferred.is_some() {
        dst.deferred = src.deferred;
    }
    if src.disabled.is_some() {
        dst.disabled = src.disabled;
    }
    if src.include_schema_arguments.is_some() {
        dst.include_schema_arguments = src.include_schema_arguments;
    }
    if src.include_tool_category_hints.is_some() {
        dst.include_tool_category_hints = src.include_tool_category_hints;
    }
    if src.persist_discovered_tools.is_some() {
        dst.persist_discovered_tools = src.persist_discovered_tools;
    }
}

fn merge_mcp_settings(dst: &mut McpSettingsConfig, src: McpSettingsConfig) {
    if src.enabled.is_some() {
        dst.enabled = src.enabled;
    }
    if src.config_file.is_some() {
        dst.config_file = src.config_file;
    }
    if src.max_servers.is_some() {
        dst.max_servers = src.max_servers;
    }
    if src.max_tools_per_server.is_some() {
        dst.max_tools_per_server = src.max_tools_per_server;
    }
    if src.max_tool_result_bytes.is_some() {
        dst.max_tool_result_bytes = src.max_tool_result_bytes;
    }
    if src.startup_timeout_seconds.is_some() {
        dst.startup_timeout_seconds = src.startup_timeout_seconds;
    }
    if src.request_timeout_seconds.is_some() {
        dst.request_timeout_seconds = src.request_timeout_seconds;
    }
    if src.external_effect_policy.is_some() {
        dst.external_effect_policy = src.external_effect_policy;
    }
    if src.load_resources.is_some() {
        dst.load_resources = src.load_resources;
    }
    if src.load_prompts.is_some() {
        dst.load_prompts = src.load_prompts;
    }
    if src.strict.is_some() {
        dst.strict = src.strict;
    }
    dst.servers.extend(src.servers);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Serializes tests that mutate process-wide env vars (CODE_AGENT_CONFIG_HOME,
    // XDG_CONFIG_HOME, HOME) so they cannot race under parallel test execution.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    // SettingSource::None short-circuits and returns an empty Settings struct.
    fn none_source_skips_io() {
        let dir = TempDir::new().unwrap();
        let s = load(dir.path(), SettingSource::None).unwrap();
        assert_eq!(s.source, SettingSource::None);
        assert!(s.allowed_tools.is_empty());
    }

    #[test]
    // Loads project settings JSON and parses tool rule shapes.
    fn loads_project_settings() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let cfg = serde_json::json!({
            "model": "claude-sonnet-4-6",
            "permission_mode": "default",
            "allowed_tools": [
                "Read",
                "Glob",
                {"tool": "Bash", "command_regex": "^cargo (test|check)"}
            ],
            "disallowed_tools": [
                {"tool": "Edit", "path_glob": "**/.env*"}
            ],
            "tool_limits": {"max_read_bytes": 4096},
            "hooks": {"block_env_file_writes": true}
        });
        fs::write(agent_dir.join("settings.json"), cfg.to_string()).unwrap();
        let s = load(dir.path(), SettingSource::Project).unwrap();
        assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-6"));
        assert!(matches!(s.permission_mode, Some(PermissionMode::Default)));
        assert_eq!(s.allowed_tools.len(), 3);
        assert_eq!(s.disallowed_tools.len(), 1);
        assert_eq!(s.tool_limits.max_read_bytes, Some(4096));
        assert_eq!(s.hooks.block_env_file_writes, Some(true));
    }

    #[test]
    // Project settings override user settings on scalar fields, but list
    // fields concatenate.
    fn project_overrides_user_scalar_concat_lists() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let user_home = TempDir::new().unwrap();
        let user_cfg_dir = user_home.path().join("code-agent");
        fs::create_dir_all(&user_cfg_dir).unwrap();
        let user_cfg = serde_json::json!({
            "model": "user-model",
            "allowed_tools": ["Read"],
        });
        fs::write(user_cfg_dir.join("settings.json"), user_cfg.to_string()).unwrap();

        let project = TempDir::new().unwrap();
        let agent_dir = project.path().join(".agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let proj_cfg = serde_json::json!({
            "model": "project-model",
            "allowed_tools": ["Glob"],
        });
        fs::write(agent_dir.join("settings.json"), proj_cfg.to_string()).unwrap();

        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_cah = std::env::var("CODE_AGENT_CONFIG_HOME").ok();
        // Use CODE_AGENT_CONFIG_HOME directly to avoid relying on parent
        // structure conventions.
        std::env::set_var("CODE_AGENT_CONFIG_HOME", user_cfg_dir.to_str().unwrap());
        std::env::remove_var("XDG_CONFIG_HOME");

        let s = load(project.path(), SettingSource::UserAndProject).unwrap();

        // Restore env.
        if let Some(v) = prev_xdg {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        match prev_cah {
            Some(v) => std::env::set_var("CODE_AGENT_CONFIG_HOME", v),
            None => std::env::remove_var("CODE_AGENT_CONFIG_HOME"),
        }

        assert_eq!(s.model.as_deref(), Some("project-model"));
        assert_eq!(s.allowed_tools.len(), 2);
    }

    #[test]
    // CODE_AGENT_CONFIG_HOME resolves to the expected user-settings path.
    fn code_agent_config_home_resolves_user_settings_path() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let cfg_home = TempDir::new().unwrap();
        let prev_cah = std::env::var("CODE_AGENT_CONFIG_HOME").ok();
        let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let prev_home = std::env::var("HOME").ok();

        std::env::set_var("CODE_AGENT_CONFIG_HOME", cfg_home.path().to_str().unwrap());
        std::env::remove_var("XDG_CONFIG_HOME");
        std::env::remove_var("HOME");

        let path = user_settings_path();

        // Restore env.
        match prev_cah {
            Some(v) => std::env::set_var("CODE_AGENT_CONFIG_HOME", v),
            None => std::env::remove_var("CODE_AGENT_CONFIG_HOME"),
        }
        if let Some(v) = prev_xdg {
            std::env::set_var("XDG_CONFIG_HOME", v);
        }
        if let Some(v) = prev_home {
            std::env::set_var("HOME", v);
        }

        assert_eq!(
            path,
            Some(cfg_home.path().join("settings.json")),
            "CODE_AGENT_CONFIG_HOME should produce <dir>/settings.json"
        );
    }

    #[test]
    // Invalid permission_mode produces a clear error.
    fn invalid_permission_mode_errors() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let cfg = serde_json::json!({"permission_mode": "bogus"});
        fs::write(agent_dir.join("settings.json"), cfg.to_string()).unwrap();
        let err = load(dir.path(), SettingSource::Project).unwrap_err();
        assert!(matches!(err, SettingsError::BadPermissionMode(_)));
    }

    #[test]
    // Loads and validates the tool_search settings block.
    fn loads_tool_search_settings() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let cfg = serde_json::json!({
            "tool_search": {
                "enabled": true,
                "mode": "search",
                "strategy": "bm25",
                "native_provider_mode": "off",
                "max_results": 7,
                "always_loaded": ["Read", "SearchTools"],
                "deferred": ["mcp__*"],
                "disabled": ["mcp__fake__delete_*"]
            }
        });
        fs::write(agent_dir.join("settings.json"), cfg.to_string()).unwrap();
        let s = load(dir.path(), SettingSource::Project).unwrap();
        assert_eq!(s.tool_search.mode.as_deref(), Some("search"));
        assert_eq!(s.tool_search.strategy.as_deref(), Some("bm25"));
        assert_eq!(s.tool_search.max_results, Some(7));
        assert_eq!(
            s.tool_search.disabled.as_deref(),
            Some(&["mcp__fake__delete_*".to_string()][..])
        );
    }

    #[test]
    // Invalid tool_search values produce a clear settings error.
    fn invalid_tool_search_setting_errors() {
        let dir = TempDir::new().unwrap();
        let agent_dir = dir.path().join(".agent");
        fs::create_dir_all(&agent_dir).unwrap();
        let cfg = serde_json::json!({"tool_search": {"strategy": "semantic"}});
        fs::write(agent_dir.join("settings.json"), cfg.to_string()).unwrap();
        let err = load(dir.path(), SettingSource::Project).unwrap_err();
        assert!(matches!(err, SettingsError::BadToolSearch(_)));
    }
}
