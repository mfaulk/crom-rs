//! Runtime wiring: build a [`PluginRegistry`] from project + user
//! settings, optionally load dev plugins via `--plugin-dir`, then merge
//! the result into a project-wide skills / commands / agents / hooks /
//! MCP / settings-defaults bundle.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::install::{
    discover_manifest_path, parse_and_validate_source, PluginInstallError, PluginInstallLayout,
};
use super::loader::{load_plugin_components, LoadedPluginComponents, PluginLoadOptions};
use super::manifest::PluginManifest;
use super::registry::{
    plugin_data_dir, PluginInstall, PluginRegistry, PluginScope, PluginSource, PluginStatus,
};
use crate::settings::{EnabledPluginEntry, PluginsSettingsConfig, Settings};

/// Effective plugin runtime configuration after merging settings + CLI
/// flags. Toggles default to permissive values *except* bin PATH
/// injection, which is opt-in.
#[derive(Debug, Clone)]
pub struct EffectivePluginConfig {
    pub enabled: bool,
    pub allow_local_plugin_dirs: bool,
    pub allow_plugin_bin_path: bool,
    pub allow_plugin_hooks: bool,
    pub allow_plugin_mcp: bool,
    pub allow_plugin_settings: bool,
    pub compat_claude_plugin_manifest: bool,
    pub max_plugins: usize,
    pub strict_component_paths: bool,
}

impl Default for EffectivePluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_local_plugin_dirs: true,
            allow_plugin_bin_path: false,
            allow_plugin_hooks: true,
            allow_plugin_mcp: true,
            allow_plugin_settings: true,
            compat_claude_plugin_manifest: false,
            max_plugins: 20,
            strict_component_paths: true,
        }
    }
}

impl EffectivePluginConfig {
    /// Apply a [`PluginsSettingsConfig`] block on top of defaults.
    pub fn apply_settings(&mut self, src: &PluginsSettingsConfig) {
        if let Some(v) = src.enabled {
            self.enabled = v;
        }
        if let Some(v) = src.allow_local_plugin_dirs {
            self.allow_local_plugin_dirs = v;
        }
        if let Some(v) = src.allow_plugin_bin_path {
            self.allow_plugin_bin_path = v;
        }
        if let Some(v) = src.allow_plugin_hooks {
            self.allow_plugin_hooks = v;
        }
        if let Some(v) = src.allow_plugin_mcp {
            self.allow_plugin_mcp = v;
        }
        if let Some(v) = src.allow_plugin_settings {
            self.allow_plugin_settings = v;
        }
        if let Some(v) = src.compat_claude_plugin_manifest {
            self.compat_claude_plugin_manifest = v;
        }
        if let Some(v) = src.max_plugins {
            self.max_plugins = v;
        }
        if let Some(v) = src.strict_component_paths {
            self.strict_component_paths = v;
        }
    }
}

/// Where the runtime was instructed to look for installed plugins. The
/// caller can tell us about the user-scope config home (where the user
/// settings live) so we can resolve user-scope cache paths without
/// duplicating env-var lookup logic.
#[derive(Debug, Clone, Default)]
pub struct PluginLoadContext {
    pub cwd: PathBuf,
    pub user_config_home: Option<PathBuf>,
    /// Extra dev plugin directories from `--plugin-dir`.
    pub dev_plugin_dirs: Vec<PathBuf>,
    pub force_disabled: bool,
}

/// Result of building the plugin registry: the populated [`PluginRegistry`]
/// plus an aggregated [`LoadedPluginComponents`] ready for the caller to
/// merge into project-wide registries. The caller is responsible for
/// emitting the corresponding events.
#[derive(Debug, Default)]
pub struct PluginRuntimeBuild {
    pub registry: PluginRegistry,
    pub aggregated: LoadedPluginComponents,
    /// Plugins that failed to load (manifest parse or validation
    /// errors). Each entry pairs the offending source path with a
    /// human-readable reason and the would-be plugin name when known.
    pub validation_failures: Vec<PluginValidationFailure>,
}

#[derive(Debug, Clone)]
pub struct PluginValidationFailure {
    pub plugin: Option<String>,
    pub source: PathBuf,
    pub reason: String,
}

/// Top-level runtime build entry point.
///
/// Walks every enabled-plugin entry in `settings.plugins.enabled_plugins`
/// across all scopes, opens the cached install, loads its components,
/// and aggregates the result. Dev plugins from `ctx.dev_plugin_dirs` run
/// on top of the cached set.
pub fn build_plugin_runtime(settings: &Settings, ctx: &PluginLoadContext) -> PluginRuntimeBuild {
    let mut config = EffectivePluginConfig::default();
    if let Some(p) = &settings.plugins {
        config.apply_settings(p);
    }
    let mut build = PluginRuntimeBuild::default();
    if !config.enabled || ctx.force_disabled {
        return build;
    }
    let plugins_cfg = settings.plugins.clone().unwrap_or_default();
    let disabled_set: std::collections::BTreeSet<String> = plugins_cfg
        .disabled_plugins
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut load_opts = PluginLoadOptions {
        allow_plugin_settings: config.allow_plugin_settings,
        allow_plugin_hooks: config.allow_plugin_hooks,
        allow_plugin_mcp: config.allow_plugin_mcp,
        allow_plugin_bin_path: config.allow_plugin_bin_path,
        user_config: BTreeMap::new(),
        ..PluginLoadOptions::default()
    };

    let mut count: usize = 0;
    if let Some(entries) = plugins_cfg.enabled_plugins.as_ref() {
        for entry in entries {
            if disabled_set.contains(&entry.name) {
                continue;
            }
            if count >= config.max_plugins {
                build.validation_failures.push(PluginValidationFailure {
                    plugin: Some(entry.name.clone()),
                    source: PathBuf::new(),
                    reason: format!(
                        "max_plugins limit ({}) reached; skipping {}",
                        config.max_plugins, entry.name
                    ),
                });
                continue;
            }
            let scope = entry
                .scope
                .as_deref()
                .and_then(PluginScope::from_str_lenient)
                .unwrap_or(PluginScope::Project);
            let layout = layout_for_scope(scope, ctx);
            match open_cached_install(&layout, entry, scope) {
                Ok(install) => {
                    load_opts.user_config = user_config_for(settings, &install.name);
                    let mut registered = install;
                    let components = load_plugin_components(&registered, &load_opts);
                    registered.components = components.summary.clone();
                    let hard_failures: Vec<_> = components
                        .failures
                        .iter()
                        .filter(|f| !is_informational_failure(f))
                        .collect();
                    if hard_failures.is_empty() {
                        registered.status = PluginStatus::Loaded;
                    } else {
                        registered.status = PluginStatus::Error {
                            reason: hard_failures
                                .iter()
                                .map(|f| f.reason.clone())
                                .collect::<Vec<_>>()
                                .join("; "),
                        };
                    }
                    merge_components(&mut build.aggregated, components);
                    build.registry.insert(registered);
                    count += 1;
                }
                Err(e) => {
                    build.validation_failures.push(PluginValidationFailure {
                        plugin: Some(entry.name.clone()),
                        source: layout.cache_root.clone(),
                        reason: e.to_string(),
                    });
                }
            }
        }
    }

    // Dev plugins: --plugin-dir loads straight from the source path,
    // bypassing the cache. Useful for plugin authors.
    for dev_dir in &ctx.dev_plugin_dirs {
        if !config.allow_local_plugin_dirs {
            build.validation_failures.push(PluginValidationFailure {
                plugin: None,
                source: dev_dir.clone(),
                reason: "allow_local_plugin_dirs is false".into(),
            });
            continue;
        }
        if count >= config.max_plugins {
            build.validation_failures.push(PluginValidationFailure {
                plugin: None,
                source: dev_dir.clone(),
                reason: format!("max_plugins limit ({}) reached", config.max_plugins),
            });
            continue;
        }
        let parse_result = parse_and_validate_source(
            dev_dir,
            config.compat_claude_plugin_manifest,
            config.strict_component_paths,
        );
        match parse_result {
            Ok(manifest) => {
                if disabled_set.contains(&manifest.name) {
                    continue;
                }
                let canonical_root = dev_dir
                    .canonicalize()
                    .unwrap_or_else(|_| dev_dir.to_path_buf());
                let data_dir = canonical_root.join(".plugin-data");
                let _ = std::fs::create_dir_all(&data_dir);
                let install = PluginInstall {
                    name: manifest.name.clone(),
                    version: manifest.version.clone(),
                    manifest: manifest.clone(),
                    root: canonical_root.clone(),
                    data_dir: data_dir.clone(),
                    scope: PluginScope::Local,
                    enabled: true,
                    installed_at_unix: time::OffsetDateTime::now_utc().unix_timestamp(),
                    source: PluginSource::DevPath(canonical_root.clone()),
                    status: PluginStatus::Loaded,
                    components: Default::default(),
                };
                load_opts.user_config = user_config_for(settings, &install.name);
                let mut registered = install;
                let components = load_plugin_components(&registered, &load_opts);
                registered.components = components.summary.clone();
                let hard_failures: Vec<_> = components
                    .failures
                    .iter()
                    .filter(|f| !is_informational_failure(f))
                    .collect();
                if !hard_failures.is_empty() {
                    registered.status = PluginStatus::Error {
                        reason: hard_failures
                            .iter()
                            .map(|f| f.reason.clone())
                            .collect::<Vec<_>>()
                            .join("; "),
                    };
                }
                merge_components(&mut build.aggregated, components);
                build.registry.insert(registered);
                count += 1;
            }
            Err(e) => {
                build.validation_failures.push(PluginValidationFailure {
                    plugin: None,
                    source: dev_dir.clone(),
                    reason: e.to_string(),
                });
            }
        }
    }

    build
}

/// Pick the cache + data layout used for a given scope.
pub fn layout_for_scope(scope: PluginScope, ctx: &PluginLoadContext) -> PluginInstallLayout {
    match scope {
        PluginScope::Project | PluginScope::Managed => PluginInstallLayout::project(&ctx.cwd),
        PluginScope::Local => PluginInstallLayout::local(&ctx.cwd),
        PluginScope::User => match &ctx.user_config_home {
            Some(home) => PluginInstallLayout::user(home),
            None => PluginInstallLayout::project(&ctx.cwd),
        },
    }
}

fn open_cached_install(
    layout: &PluginInstallLayout,
    entry: &EnabledPluginEntry,
    scope: PluginScope,
) -> Result<PluginInstall, PluginInstallError> {
    // Defense-in-depth: settings.json is operator-supplied data and could
    // be hand-edited or imported from a foreign source. Re-validate the
    // entry's name and version here so a crafted `enabled_plugins` entry
    // can't make the runtime open a cache directory outside the layout
    // root.
    if !super::manifest::is_plugin_name_safe(&entry.name) {
        return Err(PluginInstallError::Manifest(
            super::manifest::PluginManifestError::UnsafeName {
                path: layout.cache_root.clone(),
                name: entry.name.clone(),
            },
        ));
    }
    if let Some(v) = entry.version.as_deref() {
        if !super::manifest::is_plugin_version_safe(v) {
            return Err(PluginInstallError::Manifest(
                super::manifest::PluginManifestError::InvalidField {
                    path: layout.cache_root.clone(),
                    field: "version",
                    message: format!("unsafe version {v:?} in enabled_plugins"),
                },
            ));
        }
    }
    let cache_root = super::registry::plugin_cache_dir(
        &layout.cache_root,
        &entry.name,
        entry.version.as_deref(),
    );
    // Lexical containment: even with the validators above this is a
    // belt-and-braces check before any filesystem read.
    if !cache_root.starts_with(&layout.cache_root) {
        return Err(PluginInstallError::Manifest(
            super::manifest::PluginManifestError::UnsafeName {
                path: cache_root,
                name: entry.name.clone(),
            },
        ));
    }
    if !cache_root.exists() {
        return Err(PluginInstallError::NotInstalled {
            name: entry.name.clone(),
            scope,
        });
    }
    let manifest_path = discover_manifest_path(&cache_root, true).ok_or_else(|| {
        PluginInstallError::Manifest(super::manifest::PluginManifestError::Read {
            path: cache_root.join(super::manifest::PLUGIN_MANIFEST_PATH),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "manifest not found"),
        })
    })?;
    let manifest = PluginManifest::load_from_path(&manifest_path)?;
    let data_dir = plugin_data_dir(&layout.data_root, &entry.name);
    let _ = std::fs::create_dir_all(&data_dir);
    Ok(PluginInstall {
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        manifest,
        root: cache_root.clone(),
        data_dir,
        scope,
        enabled: true,
        installed_at_unix: time::OffsetDateTime::now_utc().unix_timestamp(),
        source: PluginSource::InstalledCache {
            cache_path: cache_root,
            original_source: None,
        },
        status: PluginStatus::Loaded,
        components: Default::default(),
    })
}

fn user_config_for(settings: &Settings, plugin: &str) -> BTreeMap<String, Value> {
    settings
        .plugin_config
        .get(plugin)
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default()
}

fn merge_components(into: &mut LoadedPluginComponents, mut from: LoadedPluginComponents) {
    into.skills.append(&mut from.skills);
    into.commands.append(&mut from.commands);
    into.agents.append(&mut from.agents);
    into.hooks.append(&mut from.hooks);
    into.mcp_servers.append(&mut from.mcp_servers);
    if into.settings_defaults.is_none() {
        into.settings_defaults = from.settings_defaults;
    } else if let (Some(Value::Object(dst)), Some(Value::Object(src))) =
        (&mut into.settings_defaults, from.settings_defaults)
    {
        merge_json_objects(dst, src);
    }
    into.bin_entries.append(&mut from.bin_entries);
    into.failures.append(&mut from.failures);
    into.summary.skills += from.summary.skills;
    into.summary.commands += from.summary.commands;
    into.summary.agents += from.summary.agents;
    into.summary.hooks += from.summary.hooks;
    into.summary.mcp_servers += from.summary.mcp_servers;
    into.summary.bin_entries += from.summary.bin_entries;
    into.summary.settings_defaults =
        into.summary.settings_defaults || from.summary.settings_defaults;
}

fn merge_json_objects(
    dst: &mut serde_json::Map<String, Value>,
    src: serde_json::Map<String, Value>,
) {
    for (k, v) in src {
        match (dst.get_mut(&k), v) {
            (Some(Value::Object(existing)), Value::Object(new)) => {
                merge_json_objects(existing, new);
            }
            (entry @ Some(_), v) => {
                // Plugin defaults must NOT override an existing value at
                // a deeper level — leave the existing value alone.
                let _ = entry;
                let _ = v;
            }
            (None, v) => {
                dst.insert(k, v);
            }
        }
    }
}

/// Treat plugin component failures that are purely informational
/// (e.g. "bin disabled") as non-fatal so they don't escalate the
/// plugin's status into Error.
fn is_informational_failure(f: &super::loader::PluginComponentFailure) -> bool {
    matches!(
        f.kind,
        super::registry::PluginComponentKind::Bin
            | super::registry::PluginComponentKind::Hook
            | super::registry::PluginComponentKind::McpServer
    ) && (f.reason.contains("disabled by allow_plugin_")
        || f.reason.contains("disabled by allow_local_plugin_"))
}

/// Apply plugin settings defaults to an existing settings JSON object
/// without overriding any value already present. This is the merge
/// strategy used to wire `settings_defaults` into project-loaded
/// settings.
pub fn merge_plugin_defaults_into_settings(
    project_settings_value: &mut Value,
    plugin_defaults: &Value,
) {
    if let (Value::Object(dst), Value::Object(src)) = (project_settings_value, plugin_defaults) {
        for (k, v) in src {
            // Treat both "absent key" and "explicit null" as unset, so
            // a settings JSON serialized with Option::None fields (which
            // serialize to `null`) doesn't block the plugin default.
            let entry_is_unset = matches!(dst.get(k), None | Some(Value::Null));
            if entry_is_unset {
                if !matches!(v, Value::Null) {
                    dst.insert(k.clone(), v.clone());
                }
            } else if let (Some(Value::Object(existing)), Value::Object(deep)) = (dst.get_mut(k), v)
            {
                merge_plugin_defaults_into_settings_object(existing, deep);
            }
        }
    }
}

fn merge_plugin_defaults_into_settings_object(
    dst: &mut serde_json::Map<String, Value>,
    src: &serde_json::Map<String, Value>,
) {
    for (k, v) in src {
        let entry_is_unset = matches!(dst.get(k), None | Some(Value::Null));
        if entry_is_unset {
            if !matches!(v, Value::Null) {
                dst.insert(k.clone(), v.clone());
            }
        } else if let (Some(Value::Object(existing)), Value::Object(deep)) = (dst.get_mut(k), v) {
            merge_plugin_defaults_into_settings_object(existing, deep);
        }
    }
}

/// Helper: turn a [`Path`] into a printable string with the user's HOME
/// redacted. Used for log output.
pub fn redacted_display(path: &Path) -> String {
    let s = path.display().to_string();
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return s.replacen(&home, "~", 1);
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_dev_plugin(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join(".agent-plugin")).unwrap();
        std::fs::write(
            dir.join(".agent-plugin/plugin.json"),
            serde_json::json!({ "name": name, "version": "0.1.0" }).to_string(),
        )
        .unwrap();
    }

    #[test]
    // A dev plugin path is parsed and surfaced through the registry.
    fn dev_plugin_loads() {
        let work = TempDir::new().unwrap();
        let plugin_dir = work.path().join("p");
        write_dev_plugin(&plugin_dir, "demo");
        let settings = Settings {
            plugins: Some(PluginsSettingsConfig {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let ctx = PluginLoadContext {
            cwd: work.path().to_path_buf(),
            user_config_home: None,
            dev_plugin_dirs: vec![plugin_dir],
            force_disabled: false,
        };
        let build = build_plugin_runtime(&settings, &ctx);
        assert_eq!(build.registry.len(), 1);
        assert!(build.registry.get("demo").is_some());
    }

    #[test]
    // Plugin defaults do not override existing settings values.
    fn defaults_do_not_override_existing() {
        let mut existing = serde_json::json!({"max_turns": 4});
        let defaults = serde_json::json!({"max_turns": 99, "max_tokens": 8000});
        merge_plugin_defaults_into_settings(&mut existing, &defaults);
        assert_eq!(existing["max_turns"], 4);
        assert_eq!(existing["max_tokens"], 8000);
    }

    #[test]
    // An explicit `null` in existing settings is treated as "unset" so
    // plugin defaults are still applied. Without this, a Settings struct
    // serialized without `skip_serializing_if` would silently drop every
    // plugin default — Codex pass-3 found this regression.
    fn defaults_apply_through_null_round_trip() {
        let mut existing = serde_json::json!({"max_turns": null});
        let defaults = serde_json::json!({"max_turns": 12});
        merge_plugin_defaults_into_settings(&mut existing, &defaults);
        assert_eq!(existing["max_turns"], 12);
    }

    #[test]
    // The disabled-plugin list keeps an enabled entry from being loaded.
    fn disabled_list_skips_dev_plugin() {
        let work = TempDir::new().unwrap();
        let plugin_dir = work.path().join("p");
        write_dev_plugin(&plugin_dir, "demo");
        let settings = Settings {
            plugins: Some(PluginsSettingsConfig {
                enabled: Some(true),
                disabled_plugins: Some(vec!["demo".into()]),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let ctx = PluginLoadContext {
            cwd: work.path().to_path_buf(),
            user_config_home: None,
            dev_plugin_dirs: vec![plugin_dir],
            force_disabled: false,
        };
        let build = build_plugin_runtime(&settings, &ctx);
        assert_eq!(build.registry.len(), 0);
    }

    #[test]
    // max_plugins caps the registry.
    fn max_plugins_caps_registry() {
        let work = TempDir::new().unwrap();
        let mut dev_dirs = Vec::new();
        for i in 0..3 {
            let p = work.path().join(format!("p{i}"));
            write_dev_plugin(&p, &format!("p{i}"));
            dev_dirs.push(p);
        }
        let settings = Settings {
            plugins: Some(PluginsSettingsConfig {
                enabled: Some(true),
                max_plugins: Some(2),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let ctx = PluginLoadContext {
            cwd: work.path().to_path_buf(),
            user_config_home: None,
            dev_plugin_dirs: dev_dirs,
            force_disabled: false,
        };
        let build = build_plugin_runtime(&settings, &ctx);
        assert_eq!(build.registry.len(), 2);
        assert_eq!(build.validation_failures.len(), 1);
    }

    #[test]
    // Defense-in-depth: a settings.json with a malicious enabled_plugins
    // entry whose `name` or `version` contains a path-traversal sequence
    // is rejected at runtime — the plugin is reported as a validation
    // failure rather than loaded from outside the cache root.
    fn enabled_plugins_traversal_is_rejected_at_runtime() {
        let work = TempDir::new().unwrap();
        let settings = Settings {
            plugins: Some(PluginsSettingsConfig {
                enabled: Some(true),
                enabled_plugins: Some(vec![EnabledPluginEntry {
                    name: "../../etc".into(),
                    version: Some("1.0.0".into()),
                    scope: Some("project".into()),
                }]),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let ctx = PluginLoadContext {
            cwd: work.path().to_path_buf(),
            user_config_home: None,
            dev_plugin_dirs: Vec::new(),
            force_disabled: false,
        };
        let build = build_plugin_runtime(&settings, &ctx);
        assert!(build.registry.is_empty());
        assert_eq!(build.validation_failures.len(), 1);
        // Same check for an unsafe version field.
        let settings = Settings {
            plugins: Some(PluginsSettingsConfig {
                enabled: Some(true),
                enabled_plugins: Some(vec![EnabledPluginEntry {
                    name: "ok".into(),
                    version: Some("../../../etc".into()),
                    scope: Some("project".into()),
                }]),
                ..Default::default()
            }),
            ..Settings::default()
        };
        let build = build_plugin_runtime(&settings, &ctx);
        assert!(build.registry.is_empty());
        assert_eq!(build.validation_failures.len(), 1);
    }
}
