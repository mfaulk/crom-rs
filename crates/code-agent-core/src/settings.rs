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

use crate::permission::PermissionMode;
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

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
}
