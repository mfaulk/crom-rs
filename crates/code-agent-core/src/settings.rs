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
use crate::ui::{
    merge_recovery_into, merge_ui_into, RecoveryConfig, RecoverySettingsConfig, UiConfig,
    UiSettingsConfig, UiSettingsError,
};

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
    #[serde(default)]
    pub agents: Option<AgentsSettingsConfig>,
    /// M7: Skills configuration.
    #[serde(default)]
    pub skills: Option<SkillsSettingsConfig>,
    /// M7: Slash-commands configuration.
    #[serde(default)]
    pub slash_commands: Option<SlashCommandsSettingsConfig>,
    /// M7: Manual compaction configuration.
    #[serde(default)]
    pub compaction: Option<CompactionSettingsConfig>,
    /// M8: Plugin system configuration.
    #[serde(default)]
    pub plugins: Option<PluginsSettingsConfig>,
    /// M8: Per-plugin configuration values, keyed by plugin name.
    /// These are exposed to plugins via `${user_config.<key>}`.
    #[serde(default)]
    pub plugin_config: Option<std::collections::BTreeMap<String, serde_json::Value>>,
    /// M9: terminal UI configuration.
    #[serde(default)]
    pub ui: Option<UiSettingsConfig>,
    /// M9: session recovery configuration.
    #[serde(default)]
    pub recovery: Option<RecoverySettingsConfig>,
}

/// Settings for the M7 Skills subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillsSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
    #[serde(default)]
    pub user_path: Option<PathBuf>,
    #[serde(default)]
    pub allow_filesystem_skills: Option<bool>,
    #[serde(default)]
    pub compat_claude_skills: Option<bool>,
    #[serde(default)]
    pub max_skill_md_bytes: Option<usize>,
    #[serde(default)]
    pub max_resource_bytes_per_skill: Option<usize>,
    #[serde(default)]
    pub max_loaded_skills_per_run: Option<usize>,
    #[serde(default)]
    pub load_resources_on_demand: Option<bool>,
    #[serde(default)]
    pub enforce_skill_allowed_tools: Option<bool>,
}

/// Settings for the M7 slash-command subsystem.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlashCommandsSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub project_path: Option<PathBuf>,
    #[serde(default)]
    pub user_path: Option<PathBuf>,
    #[serde(default)]
    pub compat_claude_commands: Option<bool>,
    #[serde(default)]
    pub enable_builtin_commands: Option<bool>,
    #[serde(default)]
    pub allow_bash_expansion: Option<bool>,
    #[serde(default)]
    pub allow_file_references: Option<bool>,
    #[serde(default)]
    pub max_expanded_prompt_bytes: Option<usize>,
    #[serde(default)]
    pub max_expansion_output_bytes: Option<usize>,
}

/// One enabled-plugin entry stored in settings (M8).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EnabledPluginEntry {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Scope where this enabled-plugin record lives. Optional in JSON;
    /// defaulted to the file's owning scope on load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// Plugin-system settings block (M8). Disabled plugins are NOT stored here
/// — operators flip `enabled` on the [`EnabledPluginEntry`] or move it to
/// `disabled_plugins`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginsSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub enabled_plugins: Option<Vec<EnabledPluginEntry>>,
    #[serde(default)]
    pub disabled_plugins: Option<Vec<String>>,
    #[serde(default)]
    pub allow_local_plugin_dirs: Option<bool>,
    #[serde(default)]
    pub allow_plugin_bin_path: Option<bool>,
    #[serde(default)]
    pub allow_plugin_hooks: Option<bool>,
    #[serde(default)]
    pub allow_plugin_mcp: Option<bool>,
    #[serde(default)]
    pub allow_plugin_settings: Option<bool>,
    #[serde(default)]
    pub compat_claude_plugin_manifest: Option<bool>,
    #[serde(default)]
    pub max_plugins: Option<usize>,
    #[serde(default)]
    pub strict_component_paths: Option<bool>,
    /// Marketplace files registered with the runtime.
    #[serde(default)]
    pub marketplaces: Option<Vec<PathBuf>>,
}

/// Settings for the M7 manual compaction.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactionSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub preserve_recent_turns: Option<usize>,
    #[serde(default)]
    pub max_summary_tokens: Option<usize>,
}

/// Settings for the M6 subagent orchestrator. All fields optional; merged
/// at load time from user + project settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentsSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub allow_filesystem_agents: Option<bool>,
    #[serde(default)]
    pub compat_claude_agents: Option<bool>,
    #[serde(default)]
    pub max_concurrent_subagents: Option<usize>,
    #[serde(default)]
    pub subagent_timeout_seconds: Option<u64>,
    #[serde(default)]
    pub persist_subagent_transcripts: Option<bool>,
    #[serde(default)]
    pub event_visibility: Option<String>,
    #[serde(default)]
    pub checkpoint_policy: Option<String>,
    #[serde(default)]
    pub inherit_project_instructions: Option<bool>,
    #[serde(default)]
    pub inherit_settings: Option<bool>,
    #[serde(default)]
    pub inherit_hooks: Option<bool>,
    #[serde(default)]
    pub inherit_permissions: Option<bool>,
    #[serde(default)]
    pub inherit_mcp_servers: Option<bool>,
    #[serde(default)]
    pub inherit_tool_search_state: Option<bool>,
    #[serde(default)]
    pub inherit_approval_memory: Option<bool>,
    #[serde(default)]
    pub builtins: Option<Vec<String>>,
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
    pub agents: Option<AgentsSettingsConfig>,
    pub skills: Option<SkillsSettingsConfig>,
    pub slash_commands: Option<SlashCommandsSettingsConfig>,
    pub compaction: Option<CompactionSettingsConfig>,
    pub plugins: Option<PluginsSettingsConfig>,
    pub plugin_config: std::collections::BTreeMap<String, serde_json::Value>,
    /// M9: effective UI configuration.
    pub ui: UiConfig,
    /// M9: effective recovery configuration.
    pub recovery: RecoveryConfig,
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
    #[error("{0}")]
    BadUi(#[from] UiSettingsError),
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
    if let Some(agents) = src.agents {
        merge_agents_settings(&mut dst.agents, agents);
    }
    if let Some(skills) = src.skills {
        merge_skills_settings(&mut dst.skills, skills);
    }
    if let Some(slash) = src.slash_commands {
        merge_slash_settings(&mut dst.slash_commands, slash);
    }
    if let Some(compact) = src.compaction {
        merge_compaction_settings(&mut dst.compaction, compact);
    }
    if let Some(plugins) = src.plugins {
        merge_plugins_settings(&mut dst.plugins, plugins);
    }
    if let Some(pc) = src.plugin_config {
        for (k, v) in pc {
            dst.plugin_config.insert(k, v);
        }
    }
    if let Some(ui) = src.ui {
        merge_ui_into(&mut dst.ui, ui)?;
    }
    if let Some(rec) = src.recovery {
        merge_recovery_into(&mut dst.recovery, rec);
    }
    Ok(())
}

fn merge_plugins_settings(dst: &mut Option<PluginsSettingsConfig>, src: PluginsSettingsConfig) {
    let target = dst.get_or_insert_with(PluginsSettingsConfig::default);
    if src.enabled.is_some() {
        target.enabled = src.enabled;
    }
    if let Some(list) = src.enabled_plugins {
        let merged = target.enabled_plugins.get_or_insert_with(Vec::new);
        for entry in list {
            // Replace any existing entry with the same name; otherwise append.
            if let Some(existing) = merged.iter_mut().find(|e| e.name == entry.name) {
                *existing = entry;
            } else {
                merged.push(entry);
            }
        }
    }
    if let Some(list) = src.disabled_plugins {
        let merged = target.disabled_plugins.get_or_insert_with(Vec::new);
        for name in list {
            if !merged.contains(&name) {
                merged.push(name);
            }
        }
    }
    if src.allow_local_plugin_dirs.is_some() {
        target.allow_local_plugin_dirs = src.allow_local_plugin_dirs;
    }
    if src.allow_plugin_bin_path.is_some() {
        target.allow_plugin_bin_path = src.allow_plugin_bin_path;
    }
    if src.allow_plugin_hooks.is_some() {
        target.allow_plugin_hooks = src.allow_plugin_hooks;
    }
    if src.allow_plugin_mcp.is_some() {
        target.allow_plugin_mcp = src.allow_plugin_mcp;
    }
    if src.allow_plugin_settings.is_some() {
        target.allow_plugin_settings = src.allow_plugin_settings;
    }
    if src.compat_claude_plugin_manifest.is_some() {
        target.compat_claude_plugin_manifest = src.compat_claude_plugin_manifest;
    }
    if src.max_plugins.is_some() {
        target.max_plugins = src.max_plugins;
    }
    if src.strict_component_paths.is_some() {
        target.strict_component_paths = src.strict_component_paths;
    }
    if let Some(list) = src.marketplaces {
        let merged = target.marketplaces.get_or_insert_with(Vec::new);
        for p in list {
            if !merged.contains(&p) {
                merged.push(p);
            }
        }
    }
}

fn merge_skills_settings(dst: &mut Option<SkillsSettingsConfig>, src: SkillsSettingsConfig) {
    let target = dst.get_or_insert_with(SkillsSettingsConfig::default);
    if src.enabled.is_some() {
        target.enabled = src.enabled;
    }
    if src.project_path.is_some() {
        target.project_path = src.project_path;
    }
    if src.user_path.is_some() {
        target.user_path = src.user_path;
    }
    if src.allow_filesystem_skills.is_some() {
        target.allow_filesystem_skills = src.allow_filesystem_skills;
    }
    if src.compat_claude_skills.is_some() {
        target.compat_claude_skills = src.compat_claude_skills;
    }
    if src.max_skill_md_bytes.is_some() {
        target.max_skill_md_bytes = src.max_skill_md_bytes;
    }
    if src.max_resource_bytes_per_skill.is_some() {
        target.max_resource_bytes_per_skill = src.max_resource_bytes_per_skill;
    }
    if src.max_loaded_skills_per_run.is_some() {
        target.max_loaded_skills_per_run = src.max_loaded_skills_per_run;
    }
    if src.load_resources_on_demand.is_some() {
        target.load_resources_on_demand = src.load_resources_on_demand;
    }
    if src.enforce_skill_allowed_tools.is_some() {
        target.enforce_skill_allowed_tools = src.enforce_skill_allowed_tools;
    }
}

fn merge_slash_settings(
    dst: &mut Option<SlashCommandsSettingsConfig>,
    src: SlashCommandsSettingsConfig,
) {
    let target = dst.get_or_insert_with(SlashCommandsSettingsConfig::default);
    if src.enabled.is_some() {
        target.enabled = src.enabled;
    }
    if src.project_path.is_some() {
        target.project_path = src.project_path;
    }
    if src.user_path.is_some() {
        target.user_path = src.user_path;
    }
    if src.compat_claude_commands.is_some() {
        target.compat_claude_commands = src.compat_claude_commands;
    }
    if src.enable_builtin_commands.is_some() {
        target.enable_builtin_commands = src.enable_builtin_commands;
    }
    if src.allow_bash_expansion.is_some() {
        target.allow_bash_expansion = src.allow_bash_expansion;
    }
    if src.allow_file_references.is_some() {
        target.allow_file_references = src.allow_file_references;
    }
    if src.max_expanded_prompt_bytes.is_some() {
        target.max_expanded_prompt_bytes = src.max_expanded_prompt_bytes;
    }
    if src.max_expansion_output_bytes.is_some() {
        target.max_expansion_output_bytes = src.max_expansion_output_bytes;
    }
}

fn merge_compaction_settings(
    dst: &mut Option<CompactionSettingsConfig>,
    src: CompactionSettingsConfig,
) {
    let target = dst.get_or_insert_with(CompactionSettingsConfig::default);
    if src.enabled.is_some() {
        target.enabled = src.enabled;
    }
    if src.preserve_recent_turns.is_some() {
        target.preserve_recent_turns = src.preserve_recent_turns;
    }
    if src.max_summary_tokens.is_some() {
        target.max_summary_tokens = src.max_summary_tokens;
    }
}

fn merge_agents_settings(dst: &mut Option<AgentsSettingsConfig>, src: AgentsSettingsConfig) {
    let target = dst.get_or_insert_with(AgentsSettingsConfig::default);
    if src.enabled.is_some() {
        target.enabled = src.enabled;
    }
    if src.path.is_some() {
        target.path = src.path;
    }
    if src.allow_filesystem_agents.is_some() {
        target.allow_filesystem_agents = src.allow_filesystem_agents;
    }
    if src.compat_claude_agents.is_some() {
        target.compat_claude_agents = src.compat_claude_agents;
    }
    if src.max_concurrent_subagents.is_some() {
        target.max_concurrent_subagents = src.max_concurrent_subagents;
    }
    if src.subagent_timeout_seconds.is_some() {
        target.subagent_timeout_seconds = src.subagent_timeout_seconds;
    }
    if src.persist_subagent_transcripts.is_some() {
        target.persist_subagent_transcripts = src.persist_subagent_transcripts;
    }
    if src.event_visibility.is_some() {
        target.event_visibility = src.event_visibility;
    }
    if src.checkpoint_policy.is_some() {
        target.checkpoint_policy = src.checkpoint_policy;
    }
    if src.inherit_project_instructions.is_some() {
        target.inherit_project_instructions = src.inherit_project_instructions;
    }
    if src.inherit_settings.is_some() {
        target.inherit_settings = src.inherit_settings;
    }
    if src.inherit_hooks.is_some() {
        target.inherit_hooks = src.inherit_hooks;
    }
    if src.inherit_permissions.is_some() {
        target.inherit_permissions = src.inherit_permissions;
    }
    if src.inherit_mcp_servers.is_some() {
        target.inherit_mcp_servers = src.inherit_mcp_servers;
    }
    if src.inherit_tool_search_state.is_some() {
        target.inherit_tool_search_state = src.inherit_tool_search_state;
    }
    if src.inherit_approval_memory.is_some() {
        target.inherit_approval_memory = src.inherit_approval_memory;
    }
    if src.builtins.is_some() {
        target.builtins = src.builtins;
    }
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
