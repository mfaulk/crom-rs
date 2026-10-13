//! Local plugin marketplace files (M8).
//!
//! Marketplaces are local-only, offline JSON files describing one or
//! more plugins available to install from neighbouring directories. No
//! network fetch, no signature verification, no dependency resolution.

use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginMarketplaceOwner {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalPluginMarketplaceEntry {
    pub name: String,
    pub source: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalPluginMarketplace {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<PluginMarketplaceOwner>,
    #[serde(default)]
    pub plugins: Vec<LocalPluginMarketplaceEntry>,
    /// Where the file was loaded from. Required so relative `source`
    /// paths can be resolved later.
    #[serde(skip)]
    pub loaded_from: PathBuf,
}

#[derive(Debug, Error)]
pub enum LocalMarketplaceError {
    #[error("read marketplace at {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("marketplace at {path}: invalid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("marketplace at {path} missing required field {field}")]
    MissingField { path: PathBuf, field: &'static str },
    #[error("marketplace at {path}: entry {entry}: {message}")]
    InvalidEntry {
        path: PathBuf,
        entry: String,
        message: String,
    },
    #[error("marketplace {marketplace} has no entry named {entry:?}")]
    UnknownEntry { marketplace: String, entry: String },
}

#[derive(Debug, Default, Deserialize)]
struct RawMarketplace {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    owner: Option<PluginMarketplaceOwner>,
    #[serde(default)]
    plugins: Vec<RawEntry>,
}

#[derive(Debug, Default, Deserialize)]
struct RawEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    source: Option<PathBuf>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

impl LocalPluginMarketplace {
    /// Read and parse a marketplace file.
    pub fn load(path: &Path) -> Result<Self, LocalMarketplaceError> {
        let bytes = std::fs::read(path).map_err(|e| LocalMarketplaceError::Read {
            path: path.to_path_buf(),
            source: e,
        })?;
        let raw: RawMarketplace =
            serde_json::from_slice(&bytes).map_err(|e| LocalMarketplaceError::InvalidJson {
                path: path.to_path_buf(),
                source: e,
            })?;
        let name = raw
            .name
            .ok_or_else(|| LocalMarketplaceError::MissingField {
                path: path.to_path_buf(),
                field: "name",
            })?;
        let mut plugins = Vec::with_capacity(raw.plugins.len());
        for (i, entry) in raw.plugins.into_iter().enumerate() {
            let entry_name =
                entry
                    .name
                    .clone()
                    .ok_or_else(|| LocalMarketplaceError::InvalidEntry {
                        path: path.to_path_buf(),
                        entry: i.to_string(),
                        message: "missing `name`".to_string(),
                    })?;
            let source = entry
                .source
                .ok_or_else(|| LocalMarketplaceError::InvalidEntry {
                    path: path.to_path_buf(),
                    entry: entry_name.clone(),
                    message: "missing `source`".to_string(),
                })?;
            plugins.push(LocalPluginMarketplaceEntry {
                name: entry_name,
                source,
                description: entry.description,
                version: entry.version,
            });
        }
        Ok(LocalPluginMarketplace {
            name,
            owner: raw.owner,
            plugins,
            loaded_from: path.to_path_buf(),
        })
    }

    /// Resolve a marketplace entry's `source` path against the directory
    /// containing the marketplace file. Absolute sources are honored as
    /// supplied so users can point at out-of-tree plugin checkouts when
    /// strict mode is off.
    pub fn resolved_source(&self, entry: &LocalPluginMarketplaceEntry) -> PathBuf {
        let parent = self
            .loaded_from
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        if entry.source.is_absolute() {
            return entry.source.clone();
        }
        parent.join(&entry.source)
    }

    /// Returns true iff the resolved source for `entry` would escape
    /// the marketplace file's parent directory. Used by strict-mode
    /// installations to refuse out-of-tree sources.
    ///
    /// Both lexical and canonical checks are performed: a relative
    /// path containing `..` is rejected without disk I/O, but a path
    /// like `plugins/link` where `link` is a symlink pointing outside
    /// the marketplace tree is also rejected after canonicalization.
    pub fn entry_escapes_parent(&self, entry: &LocalPluginMarketplaceEntry) -> bool {
        if entry.source.is_absolute() {
            return true;
        }
        for c in entry.source.components() {
            if matches!(c, Component::ParentDir) {
                return true;
            }
        }
        let parent = match self.loaded_from.parent() {
            Some(p) => p,
            None => return false,
        };
        let canonical_parent = match parent.canonicalize() {
            Ok(p) => p,
            Err(_) => return false,
        };
        let candidate = parent.join(&entry.source);
        if let Ok(canonical_candidate) = candidate.canonicalize() {
            if !canonical_candidate.starts_with(&canonical_parent) {
                return true;
            }
        }
        false
    }

    pub fn find(&self, name: &str) -> Option<&LocalPluginMarketplaceEntry> {
        self.plugins.iter().find(|e| e.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // A complete marketplace file parses with all entries.
    fn parses_marketplace() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("marketplace.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "name": "local-devtools",
                "owner": {"name": "DevTools"},
                "plugins": [
                    {"name": "rust-quality", "source": "./plugins/rust-quality", "version": "1.0.0"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let m = LocalPluginMarketplace::load(&path).unwrap();
        assert_eq!(m.name, "local-devtools");
        assert_eq!(m.plugins.len(), 1);
        let entry = m.find("rust-quality").unwrap();
        assert_eq!(entry.version.as_deref(), Some("1.0.0"));
        let resolved = m.resolved_source(entry);
        assert!(resolved.ends_with("plugins/rust-quality"));
    }

    #[test]
    // Missing top-level name rejected.
    fn missing_name_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("m.json");
        std::fs::write(&path, "{}").unwrap();
        let err = LocalPluginMarketplace::load(&path).unwrap_err();
        assert!(matches!(
            err,
            LocalMarketplaceError::MissingField { field: "name", .. }
        ));
    }

    #[test]
    // Entry-relative source paths that try to escape via `..` are flagged.
    fn entry_escape_detected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("m.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "name": "x",
                "plugins": [
                    {"name": "a", "source": "../etc"}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let m = LocalPluginMarketplace::load(&path).unwrap();
        assert!(m.entry_escapes_parent(&m.plugins[0]));
    }
}
