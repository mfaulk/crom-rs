//! Plugin component loader. Reads each enabled plugin's cache directory
//! and produces typed component artifacts (skills, slash commands,
//! agents, hooks, MCP server configs, settings defaults, bin entries)
//! that the runtime registers through the existing M2/M4/M6/M7 systems.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use code_agent_mcp::{McpServerConfig, StdioMcpServerConfig, StreamableHttpMcpServerConfig};

use super::registry::{
    PluginComponentKind, PluginComponentSummary, PluginInstall, PluginRuntimePaths,
};
use super::vars::{substitute_str, substitute_value, PluginSubstError, PluginVarContext};
use crate::agents::{
    parse_definition_file, AgentDefinition, AgentDefinitionError, MAX_AGENT_FILE_BYTES,
};
use crate::skills::{
    parse_skill_md, SkillDefinition, SkillError, SkillSource, DEFAULT_MAX_SKILL_MD_BYTES,
};
use crate::slash::custom::{
    parse_custom_command, CustomSlashCommandDefinition, CustomSlashCommandError,
    MAX_COMMAND_FILE_BYTES,
};
use crate::tool_rule::ToolRule;

/// One declarative hook entry from `hooks/hooks.json`. Mirrors the
/// shape the spec sketches but stays loose enough that future event
/// variants don't break parsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHookCommand {
    #[serde(rename = "type", default = "default_command_type")]
    pub kind: String,
    pub command: String,
}

fn default_command_type() -> String {
    "command".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginHookMatcherEntry {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<PluginHookCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginHooksFile {
    #[serde(default)]
    pub hooks: BTreeMap<String, Vec<PluginHookMatcherEntry>>,
}

/// Per-plugin loaded artifact set. Returned by [`load_plugin_components`]
/// for the caller to merge into project-wide registries.
#[derive(Debug, Default)]
pub struct LoadedPluginComponents {
    pub skills: Vec<SkillDefinition>,
    pub commands: Vec<CustomSlashCommandDefinition>,
    pub agents: Vec<AgentDefinition>,
    pub hooks: Vec<PluginLoadedHook>,
    pub mcp_servers: Vec<(String, McpServerConfig)>,
    pub settings_defaults: Option<Value>,
    pub bin_entries: Vec<PathBuf>,
    pub failures: Vec<PluginComponentFailure>,
    pub summary: PluginComponentSummary,
}

#[derive(Debug, Clone)]
pub struct PluginLoadedHook {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    pub command_type: String,
}

#[derive(Debug, Clone)]
pub struct PluginComponentFailure {
    pub kind: PluginComponentKind,
    pub name: Option<String>,
    pub reason: String,
}

#[derive(Debug, Error)]
pub enum PluginLoadError {
    #[error("plugin {plugin}: {message}")]
    Generic { plugin: String, message: String },
    #[error("plugin {plugin}: {field}: {source}")]
    Subst {
        plugin: String,
        field: String,
        #[source]
        source: PluginSubstError,
    },
}

/// Knobs for component loading. `allow_*` mirror the corresponding
/// plugin settings flags; the caller flips them based on settings +
/// CLI flags.
#[derive(Debug, Clone)]
pub struct PluginLoadOptions {
    pub allow_plugin_settings: bool,
    pub allow_plugin_hooks: bool,
    pub allow_plugin_mcp: bool,
    pub allow_plugin_bin_path: bool,
    pub user_config: BTreeMap<String, Value>,
    pub max_skill_md_bytes: usize,
    pub max_command_file_bytes: usize,
}

impl Default for PluginLoadOptions {
    fn default() -> Self {
        Self {
            allow_plugin_settings: true,
            allow_plugin_hooks: true,
            allow_plugin_mcp: true,
            allow_plugin_bin_path: false,
            user_config: BTreeMap::new(),
            max_skill_md_bytes: DEFAULT_MAX_SKILL_MD_BYTES,
            max_command_file_bytes: MAX_COMMAND_FILE_BYTES,
        }
    }
}

/// Load the cached plugin's components, namespaced by the plugin name.
pub fn load_plugin_components(
    install: &PluginInstall,
    options: &PluginLoadOptions,
) -> LoadedPluginComponents {
    let runtime = install.runtime_paths();
    let var_ctx = PluginVarContext::new(&runtime).with_user_config(options.user_config.clone());
    let mut out = LoadedPluginComponents::default();

    load_skills(install, &runtime, options, &mut out);
    load_commands(install, &runtime, options, &mut out);
    load_agents(install, &runtime, options, &mut out);
    if options.allow_plugin_hooks {
        load_hooks(install, &runtime, &var_ctx, &mut out);
    } else {
        // Still parse to flag presence as informational.
        if let Some(rel) = &install.manifest.hooks {
            let path = install.root.join(rel);
            if path.exists() {
                out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Hook,
                    name: None,
                    reason: "plugin hooks disabled by allow_plugin_hooks=false".into(),
                });
            }
        }
    }
    if options.allow_plugin_mcp {
        load_mcp(install, &runtime, &var_ctx, &mut out);
    } else if let Some(rel) = &install.manifest.mcp_servers {
        let path = install.root.join(rel);
        if path.exists() {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::McpServer,
                name: None,
                reason: "plugin MCP disabled by allow_plugin_mcp=false".into(),
            });
        }
    }
    if options.allow_plugin_settings {
        load_settings_defaults(install, &mut out);
    }
    load_bin_entries(install, options, &mut out);

    out.summary = PluginComponentSummary {
        skills: out.skills.len(),
        commands: out.commands.len(),
        agents: out.agents.len(),
        hooks: out.hooks.len(),
        mcp_servers: out.mcp_servers.len(),
        settings_defaults: out.settings_defaults.is_some(),
        bin_entries: out.bin_entries.len(),
    };
    out
}

/// Returns true iff `child` is a non-symlink directory whose canonical
/// path is contained inside `plugin_root`. Used by every component
/// discovery routine so a malicious plugin cannot drop a symlink into
/// `skills/`, `commands/`, or `agents/` that escapes the plugin root.
fn is_safe_child_dir(child: &Path, plugin_root: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(child) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return false;
    }
    let canonical_root = match plugin_root.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let canonical_child = match child.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    canonical_child.starts_with(&canonical_root)
}

/// Returns true iff `path` is a non-symlink regular file contained
/// inside `plugin_root`.
fn is_safe_file(path: &Path, plugin_root: &Path) -> bool {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return false;
    }
    let canonical_root = match plugin_root.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let canonical_path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => return false,
    };
    canonical_path.starts_with(&canonical_root)
}

fn load_skills(
    install: &PluginInstall,
    runtime: &PluginRuntimePaths,
    options: &PluginLoadOptions,
    out: &mut LoadedPluginComponents,
) {
    for rel in &install.manifest.skills {
        let dir = runtime.root.join(rel);
        if !dir.exists() {
            continue;
        }
        // Each skills entry can be either a directory of skill dirs or
        // a single SKILL.md directory. Detect by looking for a SKILL.md.
        if dir.join("SKILL.md").is_file() && is_safe_child_dir(&dir, &runtime.root) {
            match read_skill(&dir, install, options.max_skill_md_bytes) {
                Ok(def) => out.skills.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Skill,
                    name: dir.file_name().and_then(|s| s.to_str()).map(str::to_string),
                    reason,
                }),
            }
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Skill,
                    name: None,
                    reason: format!("read {}: {e}", dir.display()),
                });
                continue;
            }
        };
        let mut child_dirs: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if is_safe_child_dir(&p, &runtime.root) {
                child_dirs.push(p);
            }
        }
        child_dirs.sort();
        for child in child_dirs {
            if !child.join("SKILL.md").is_file() {
                continue;
            }
            match read_skill(&child, install, options.max_skill_md_bytes) {
                Ok(def) => out.skills.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Skill,
                    name: child
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                    reason,
                }),
            }
        }
    }
}

fn read_skill(
    skill_dir: &Path,
    install: &PluginInstall,
    max_bytes: usize,
) -> Result<SkillDefinition, String> {
    let skill_md = skill_dir.join("SKILL.md");
    let bytes =
        std::fs::read(&skill_md).map_err(|e| format!("read {}: {e}", skill_md.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!("SKILL.md exceeds {max_bytes} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8 in SKILL.md: {e}"))?;
    let mut def = parse_skill_md(&text, &skill_md, skill_dir, SkillSource::Project)
        .map_err(|e: SkillError| e.to_string())?;
    // Namespace the skill name.
    let qualified = format!("{}:{}", install.name, def.name);
    def.name = qualified;
    if def.metadata.is_null() {
        def.metadata = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Value::Object(map) = &mut def.metadata {
        map.insert("plugin".into(), Value::String(install.name.clone()));
        if let Some(v) = &install.version {
            map.insert("plugin_version".into(), Value::String(v.clone()));
        }
    }
    def.validate_structural()
        .map_err(|e: SkillError| e.to_string())?;
    Ok(def)
}

fn load_commands(
    install: &PluginInstall,
    runtime: &PluginRuntimePaths,
    options: &PluginLoadOptions,
    out: &mut LoadedPluginComponents,
) {
    for rel in &install.manifest.commands {
        let dir = runtime.root.join(rel);
        if !dir.exists() {
            continue;
        }
        if is_safe_file(&dir, &runtime.root)
            && dir.extension().and_then(|s| s.to_str()) == Some("md")
        {
            match read_command(&dir, install, options.max_command_file_bytes) {
                Ok(def) => out.commands.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Command,
                    name: dir.file_stem().and_then(|s| s.to_str()).map(str::to_string),
                    reason,
                }),
            }
            continue;
        }
        if !is_safe_child_dir(&dir, &runtime.root) {
            continue;
        }
        let mut files: Vec<PathBuf> = Vec::new();
        let mut subdirs: Vec<PathBuf> = Vec::new();
        match std::fs::read_dir(&dir) {
            Ok(rd) => {
                for entry in rd.flatten() {
                    let p = entry.path();
                    if is_safe_child_dir(&p, &runtime.root) {
                        subdirs.push(p);
                    } else if p.extension().and_then(|s| s.to_str()) == Some("md")
                        && is_safe_file(&p, &runtime.root)
                    {
                        files.push(p);
                    }
                }
            }
            Err(e) => {
                out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Command,
                    name: None,
                    reason: format!("read {}: {e}", dir.display()),
                });
                continue;
            }
        }
        files.sort();
        subdirs.sort();
        for p in files {
            match read_command(&p, install, options.max_command_file_bytes) {
                Ok(def) => out.commands.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Command,
                    name: p.file_stem().and_then(|s| s.to_str()).map(str::to_string),
                    reason,
                }),
            }
        }
        for sub in subdirs {
            walk_command_subdir(
                &sub,
                install,
                options.max_command_file_bytes,
                &runtime.root,
                out,
            );
        }
    }
}

fn walk_command_subdir(
    dir: &Path,
    install: &PluginInstall,
    max_bytes: usize,
    plugin_root: &Path,
    out: &mut LoadedPluginComponents,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    let mut files: Vec<PathBuf> = Vec::new();
    let mut subdirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if is_safe_child_dir(&p, plugin_root) {
            subdirs.push(p);
        } else if p.extension().and_then(|s| s.to_str()) == Some("md")
            && is_safe_file(&p, plugin_root)
        {
            files.push(p);
        }
    }
    files.sort();
    subdirs.sort();
    for p in files {
        match read_command(&p, install, max_bytes) {
            Ok(def) => out.commands.push(def),
            Err(reason) => out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::Command,
                name: p.file_stem().and_then(|s| s.to_str()).map(str::to_string),
                reason,
            }),
        }
    }
    for sub in subdirs {
        walk_command_subdir(&sub, install, max_bytes, plugin_root, out);
    }
}

fn read_command(
    path: &Path,
    install: &PluginInstall,
    max_bytes: usize,
) -> Result<CustomSlashCommandDefinition, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() > max_bytes {
        return Err(format!("command file exceeds {max_bytes} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let mut def = parse_custom_command(&text, path, None)
        .map_err(|e: CustomSlashCommandError| e.to_string())?;
    def.namespace = Some(install.name.clone());
    Ok(def)
}

fn load_agents(
    install: &PluginInstall,
    runtime: &PluginRuntimePaths,
    _options: &PluginLoadOptions,
    out: &mut LoadedPluginComponents,
) {
    for rel in &install.manifest.agents {
        let target = runtime.root.join(rel);
        if !target.exists() {
            continue;
        }
        if is_safe_file(&target, &runtime.root)
            && target.extension().and_then(|s| s.to_str()) == Some("md")
        {
            match read_agent(&target, install) {
                Ok(def) => out.agents.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Agent,
                    name: target
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                    reason,
                }),
            }
            continue;
        }
        if !is_safe_child_dir(&target, &runtime.root) {
            continue;
        }
        let entries = match std::fs::read_dir(&target) {
            Ok(rd) => rd,
            Err(e) => {
                out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Agent,
                    name: None,
                    reason: format!("read {}: {e}", target.display()),
                });
                continue;
            }
        };
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries.flatten() {
            let p = entry.path();
            if is_safe_file(&p, &runtime.root)
                && p.extension().and_then(|s| s.to_str()) == Some("md")
            {
                files.push(p);
            }
        }
        files.sort();
        for p in files {
            match read_agent(&p, install) {
                Ok(def) => out.agents.push(def),
                Err(reason) => out.failures.push(PluginComponentFailure {
                    kind: PluginComponentKind::Agent,
                    name: p.file_stem().and_then(|s| s.to_str()).map(str::to_string),
                    reason,
                }),
            }
        }
    }
}

fn read_agent(path: &Path, install: &PluginInstall) -> Result<AgentDefinition, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if bytes.len() > MAX_AGENT_FILE_BYTES {
        return Err(format!("agent file exceeds {MAX_AGENT_FILE_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|e| format!("invalid UTF-8: {e}"))?;
    let mut def =
        parse_definition_file(&text, path).map_err(|e: AgentDefinitionError| e.to_string())?;
    let qualified = format!("{}:{}", install.name, def.name);
    def.name = qualified;
    def.validate_structural()
        .map_err(|e: AgentDefinitionError| e.to_string())?;
    Ok(def)
}

fn load_hooks(
    install: &PluginInstall,
    runtime: &PluginRuntimePaths,
    var_ctx: &PluginVarContext<'_>,
    out: &mut LoadedPluginComponents,
) {
    let Some(rel) = &install.manifest.hooks else {
        return;
    };
    let path = runtime.root.join(rel);
    if !path.exists() {
        return;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::Hook,
                name: None,
                reason: format!("read {}: {e}", path.display()),
            });
            return;
        }
    };
    let parsed: PluginHooksFile = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::Hook,
                name: None,
                reason: format!("invalid JSON in hooks.json: {e}"),
            });
            return;
        }
    };
    for (event, entries) in parsed.hooks {
        for entry in entries {
            for cmd in entry.hooks {
                let interpolated = match substitute_str(
                    &cmd.command,
                    &format!(
                        "hooks.{event}.{}.command",
                        entry.matcher.as_deref().unwrap_or("*")
                    ),
                    var_ctx,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        out.failures.push(PluginComponentFailure {
                            kind: PluginComponentKind::Hook,
                            name: Some(event.clone()),
                            reason: e.to_string(),
                        });
                        continue;
                    }
                };
                out.hooks.push(PluginLoadedHook {
                    event: event.clone(),
                    matcher: entry.matcher.clone(),
                    command: interpolated,
                    command_type: cmd.kind.clone(),
                });
            }
        }
    }
}

fn load_mcp(
    install: &PluginInstall,
    runtime: &PluginRuntimePaths,
    var_ctx: &PluginVarContext<'_>,
    out: &mut LoadedPluginComponents,
) {
    let Some(rel) = &install.manifest.mcp_servers else {
        return;
    };
    let path = runtime.root.join(rel);
    if !path.exists() {
        return;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::McpServer,
                name: None,
                reason: format!("read {}: {e}", path.display()),
            });
            return;
        }
    };
    let raw: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::McpServer,
                name: None,
                reason: format!("invalid JSON in .mcp.json: {e}"),
            });
            return;
        }
    };
    let mut interpolated = match substitute_value(&raw, "mcpServers", var_ctx) {
        Ok(v) => v,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::McpServer,
                name: None,
                reason: e.to_string(),
            });
            return;
        }
    };
    let servers = interpolated
        .as_object_mut()
        .and_then(|m| m.remove("mcpServers"));
    let Some(Value::Object(map)) = servers else {
        out.failures.push(PluginComponentFailure {
            kind: PluginComponentKind::McpServer,
            name: None,
            reason: "missing or non-object `mcpServers` in plugin .mcp.json".into(),
        });
        return;
    };
    for (raw_name, server_value) in map {
        let qualified_name = format!("{}.{}", install.name, raw_name);
        match serde_json::from_value::<McpServerConfig>(server_value) {
            Ok(cfg) => out.mcp_servers.push((qualified_name, cfg)),
            Err(e) => out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::McpServer,
                name: Some(qualified_name),
                reason: e.to_string(),
            }),
        }
    }
}

fn load_settings_defaults(install: &PluginInstall, out: &mut LoadedPluginComponents) {
    let Some(rel) = &install.manifest.settings else {
        return;
    };
    let path = install.root.join(rel);
    if !path.exists() {
        return;
    }
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::SettingDefault,
                name: None,
                reason: format!("read {}: {e}", path.display()),
            });
            return;
        }
    };
    let parsed: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::SettingDefault,
                name: None,
                reason: format!("invalid JSON in settings.json: {e}"),
            });
            return;
        }
    };
    out.settings_defaults = Some(parsed);
}

fn load_bin_entries(
    install: &PluginInstall,
    options: &PluginLoadOptions,
    out: &mut LoadedPluginComponents,
) {
    let Some(rel) = &install.manifest.bin else {
        return;
    };
    let path = install.root.join(rel);
    if !path.exists() {
        return;
    }
    if is_safe_file(&path, &install.root) {
        out.bin_entries.push(path);
        return;
    }
    if !is_safe_child_dir(&path, &install.root) {
        return;
    }
    let entries = match std::fs::read_dir(&path) {
        Ok(rd) => rd,
        Err(e) => {
            out.failures.push(PluginComponentFailure {
                kind: PluginComponentKind::Bin,
                name: None,
                reason: format!("read {}: {e}", path.display()),
            });
            return;
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if is_safe_file(&p, &install.root) {
            paths.push(p);
        }
    }
    paths.sort();
    out.bin_entries = paths;
    if !options.allow_plugin_bin_path && !out.bin_entries.is_empty() {
        // Surface as informational failure so callers can show a warning;
        // bin entries themselves are still listed so `plugin show` works.
        out.failures.push(PluginComponentFailure {
            kind: PluginComponentKind::Bin,
            name: None,
            reason: "bin PATH injection disabled by allow_plugin_bin_path=false".into(),
        });
    }
}

/// Allowance for shell timeouts when plugin hooks are translated into
/// runtime hook invocations. Currently informational — the orchestrator
/// can surface this to the bash hook if it implements command-style
/// hook execution.
pub const PLUGIN_HOOK_DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

#[allow(dead_code)]
fn _typecheck_tool_rule(_r: ToolRule) {}
#[allow(dead_code)]
fn _typecheck_stdio(_c: StdioMcpServerConfig) {}
#[allow(dead_code)]
fn _typecheck_http(_c: StreamableHttpMcpServerConfig) {}
