//! Cross-cutting plugin path validation.

use std::path::{Path, PathBuf};

use thiserror::Error;

use super::manifest::{resolve_safe_plugin_path, PluginManifest, PluginManifestError};

#[derive(Debug, Error)]
pub enum PluginValidationError {
    #[error("plugin path {field} {value:?} escapes plugin root {root:?}")]
    EscapingPath {
        field: &'static str,
        value: String,
        root: PathBuf,
    },
    #[error("manifest: {0}")]
    Manifest(#[from] PluginManifestError),
    #[error("plugin {field} path {value:?} does not exist")]
    Missing { field: &'static str, value: String },
}

/// Validate every component path declared by `manifest` against
/// `plugin_root`, ensuring it stays inside the root and (when
/// `strict_component_paths`) actually exists. Returns success and lets
/// the caller decide whether to install/load.
pub fn validate_plugin_paths(
    plugin_root: &Path,
    manifest: &PluginManifest,
    strict_component_paths: bool,
) -> Result<(), PluginValidationError> {
    for (field, rel) in manifest.declared_paths() {
        let resolved =
            resolve_safe_plugin_path(plugin_root, &rel).map_err(PluginValidationError::Manifest)?;
        if strict_component_paths && !resolved.exists() {
            return Err(PluginValidationError::Missing {
                field,
                value: rel.display().to_string(),
            });
        }
        // Symlink escape: canonicalize twice to be sure. resolve_safe
        // already canonicalized; re-check explicitly when the file exists.
        if resolved.exists() {
            let canonical_root =
                plugin_root
                    .canonicalize()
                    .map_err(|_| PluginValidationError::EscapingPath {
                        field,
                        value: rel.display().to_string(),
                        root: plugin_root.to_path_buf(),
                    })?;
            let canonical_resolved =
                resolved
                    .canonicalize()
                    .map_err(|_| PluginValidationError::EscapingPath {
                        field,
                        value: rel.display().to_string(),
                        root: plugin_root.to_path_buf(),
                    })?;
            if !canonical_resolved.starts_with(&canonical_root) {
                return Err(PluginValidationError::EscapingPath {
                    field,
                    value: rel.display().to_string(),
                    root: plugin_root.to_path_buf(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // A plugin with all component paths inside the root validates.
    fn valid_plugin_passes() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("skills/x")).unwrap();
        std::fs::write(dir.path().join("skills/x/SKILL.md"), "hi").unwrap();
        let bytes = serde_json::json!({
            "name": "ok",
            "skills": "./skills/"
        })
        .to_string();
        let manifest = PluginManifest::parse(
            bytes.as_bytes(),
            &dir.path().join(".agent-plugin/plugin.json"),
        )
        .unwrap();
        validate_plugin_paths(dir.path(), &manifest, true).unwrap();
    }

    #[test]
    // strict_component_paths flags missing component path.
    fn strict_missing_path_errors() {
        let dir = TempDir::new().unwrap();
        let bytes = serde_json::json!({
            "name": "ok",
            "skills": "./skills/"
        })
        .to_string();
        let manifest = PluginManifest::parse(
            bytes.as_bytes(),
            &dir.path().join(".agent-plugin/plugin.json"),
        )
        .unwrap();
        let err = validate_plugin_paths(dir.path(), &manifest, true).unwrap_err();
        assert!(matches!(err, PluginValidationError::Missing { .. }));
    }

    #[cfg(unix)]
    #[test]
    // A symlink that points outside the plugin root is rejected.
    fn symlink_escape_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("evil")).unwrap();
        std::fs::write(outside.path().join("evil").join("x"), "x").unwrap();
        let dir = TempDir::new().unwrap();
        symlink(outside.path().join("evil"), dir.path().join("skills")).unwrap();
        let bytes = serde_json::json!({
            "name": "ok",
            "skills": "./skills/"
        })
        .to_string();
        let manifest = PluginManifest::parse(
            bytes.as_bytes(),
            &dir.path().join(".agent-plugin/plugin.json"),
        )
        .unwrap();
        let err = validate_plugin_paths(dir.path(), &manifest, true).unwrap_err();
        assert!(
            matches!(err, PluginValidationError::EscapingPath { .. })
                || matches!(err, PluginValidationError::Manifest(_))
        );
    }
}
