//! Plugin registry types: enabled plugins, scopes, sources, statuses,
//! component summaries, and runtime paths.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::manifest::PluginManifest;

/// Where an enabled-plugin entry was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginScope {
    User,
    Project,
    Local,
    Managed,
}

impl PluginScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
            Self::Managed => "managed",
        }
    }

    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            "local" => Some(Self::Local),
            "managed" => Some(Self::Managed),
            _ => None,
        }
    }
}

/// Where a plugin was installed from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginSource {
    /// Plugin installed from a local directory and copied into the cache.
    LocalPath(PathBuf),
    /// Plugin loaded directly from a developer path (no cache copy).
    DevPath(PathBuf),
    /// Plugin running from its cache copy. `original_source` is a
    /// best-effort string describing where the cache came from.
    InstalledCache {
        cache_path: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original_source: Option<String>,
    },
    /// Plugin installed via a local marketplace entry.
    LocalMarketplace {
        marketplace: String,
        entry: String,
        source_path: PathBuf,
    },
}

/// Operational status of a registered plugin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginStatus {
    Loaded,
    Disabled,
    Error { reason: String },
}

/// Component counts produced after a plugin has been loaded into the
/// runtime.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginComponentSummary {
    #[serde(default)]
    pub skills: usize,
    #[serde(default)]
    pub commands: usize,
    #[serde(default)]
    pub agents: usize,
    #[serde(default)]
    pub hooks: usize,
    #[serde(default)]
    pub mcp_servers: usize,
    #[serde(default)]
    pub settings_defaults: bool,
    #[serde(default)]
    pub bin_entries: usize,
}

impl PluginComponentSummary {
    pub fn total(&self) -> usize {
        self.skills + self.commands + self.agents + self.hooks + self.mcp_servers + self.bin_entries
    }
}

/// Kind of plugin component, used in events and CLI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginComponentKind {
    Skill,
    Command,
    Agent,
    Hook,
    McpServer,
    SettingDefault,
    Bin,
}

impl PluginComponentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skill => "skill",
            Self::Command => "command",
            Self::Agent => "agent",
            Self::Hook => "hook",
            Self::McpServer => "mcp_server",
            Self::SettingDefault => "setting_default",
            Self::Bin => "bin",
        }
    }
}

/// Resolved root and persistent data directories for an installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuntimePaths {
    pub root: PathBuf,
    pub data: PathBuf,
}

/// Compact projection of a registered plugin used in event payloads,
/// `SystemInit`, and CLI listings. Excludes the full manifest body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub scope: PluginScope,
    pub enabled: bool,
    pub status: PluginStatus,
    pub component_summary: PluginComponentSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PluginSource>,
}

/// One installed plugin known to the registry.
#[derive(Debug, Clone)]
pub struct PluginInstall {
    pub name: String,
    pub manifest: PluginManifest,
    pub root: PathBuf,
    pub data_dir: PathBuf,
    pub scope: PluginScope,
    pub enabled: bool,
    pub version: Option<String>,
    pub installed_at_unix: i64,
    pub source: PluginSource,
    pub status: PluginStatus,
    pub components: PluginComponentSummary,
}

impl PluginInstall {
    pub fn runtime_paths(&self) -> PluginRuntimePaths {
        PluginRuntimePaths {
            root: self.root.clone(),
            data: self.data_dir.clone(),
        }
    }

    pub fn summary(&self) -> PluginSummary {
        PluginSummary {
            name: self.name.clone(),
            version: self.version.clone(),
            scope: self.scope,
            enabled: self.enabled,
            status: self.status.clone(),
            component_summary: self.components.clone(),
            source: Some(self.source.clone()),
        }
    }
}

/// In-memory registry of plugins discovered for the current process.
#[derive(Default, Debug, Clone)]
pub struct PluginRegistry {
    pub plugins: BTreeMap<String, PluginInstall>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&PluginInstall> {
        self.plugins.get(name)
    }

    pub fn get_mut(&mut self, name: &str) -> Option<&mut PluginInstall> {
        self.plugins.get_mut(name)
    }

    pub fn insert(&mut self, install: PluginInstall) {
        self.plugins.insert(install.name.clone(), install);
    }

    pub fn remove(&mut self, name: &str) -> Option<PluginInstall> {
        self.plugins.remove(name)
    }

    /// Sorted summaries used for `plugin list` output and SystemInit.
    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.plugins.values().map(PluginInstall::summary).collect()
    }

    /// Sorted iteration over `(name, install)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PluginInstall)> {
        self.plugins.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut PluginInstall)> {
        self.plugins.iter_mut().map(|(k, v)| (k.as_str(), v))
    }

    pub fn names(&self) -> Vec<String> {
        self.plugins.keys().cloned().collect()
    }

    pub fn enabled_iter(&self) -> impl Iterator<Item = (&str, &PluginInstall)> {
        self.plugins
            .iter()
            .filter(|(_, p)| p.enabled)
            .map(|(k, v)| (k.as_str(), v))
    }

    /// Drop everything. Used by `--no-plugins`.
    pub fn clear(&mut self) {
        self.plugins.clear();
    }

    /// Compose plugin runtime paths for a name.
    pub fn runtime_paths(&self, name: &str) -> Option<PluginRuntimePaths> {
        self.plugins.get(name).map(PluginInstall::runtime_paths)
    }
}

/// Compute a deterministic plugin data directory under `data_root` for
/// `<name>`. Plugin-data is keyed only on the plugin name (not version)
/// so user state survives upgrades.
pub fn plugin_data_dir(data_root: &Path, name: &str) -> PathBuf {
    data_root.join(name)
}

/// Compute a versioned cache directory under `cache_root`.
pub fn plugin_cache_dir(cache_root: &Path, name: &str, version: Option<&str>) -> PathBuf {
    let v = version.unwrap_or("unversioned");
    cache_root.join(name).join(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Scope::from_str_lenient handles canonical labels.
    fn scope_lenient_round_trip() {
        for s in ["user", "project", "local", "managed"] {
            let parsed = PluginScope::from_str_lenient(s).expect("parses");
            assert_eq!(parsed.as_str(), s);
        }
        assert!(PluginScope::from_str_lenient("nope").is_none());
    }

    #[test]
    // The component summary `total()` aggregates all kinds (settings_defaults
    // is a flag, not counted as a component).
    fn component_summary_total() {
        let s = PluginComponentSummary {
            skills: 1,
            commands: 2,
            agents: 3,
            hooks: 4,
            mcp_servers: 5,
            settings_defaults: true,
            bin_entries: 6,
        };
        assert_eq!(s.total(), 21);
    }
}
