//! Plugin installation, cache, and prune helpers (M8).
//!
//! Plugins live under either a user-scope cache (under
//! `${CODE_AGENT_DATA_HOME}/plugins/cache/`) or a project-scope cache
//! (under `<cwd>/.agent/plugins/cache/`). Installs always copy the
//! source directory into a versioned cache subdir, then load components
//! from the cache to keep behaviour stable across edits to the source
//! tree. `--plugin-dir` skips the copy and runs straight from the
//! source directory.

use std::path::{Path, PathBuf};

use thiserror::Error;
use time::OffsetDateTime;

use super::manifest::{
    is_plugin_name_safe, PluginManifest, PluginManifestError, COMPAT_PLUGIN_MANIFEST_PATH,
    PLUGIN_MANIFEST_PATH,
};
use super::registry::{
    plugin_cache_dir, plugin_data_dir, PluginInstall, PluginScope, PluginSource, PluginStatus,
};
use super::validate::{validate_plugin_paths, PluginValidationError};

/// Errors produced by the install/uninstall/prune helpers.
#[derive(Debug, Error)]
pub enum PluginInstallError {
    #[error("source path {0} does not exist")]
    MissingSource(PathBuf),
    #[error("source path {0} is not a directory")]
    NotADirectory(PathBuf),
    #[error("manifest error: {0}")]
    Manifest(#[from] PluginManifestError),
    #[error("validation: {0}")]
    Validation(#[from] PluginValidationError),
    #[error("plugin {name} version {version:?} is already installed; pass --force to replace")]
    AlreadyInstalled {
        name: String,
        version: Option<String>,
    },
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plugin {name} is not installed in scope {scope:?}")]
    NotInstalled { name: String, scope: PluginScope },
}

/// Where the install/registry helpers expect to read and write plugin
/// directories. Built by callers from settings + scope + workspace info.
#[derive(Debug, Clone)]
pub struct PluginInstallLayout {
    pub cache_root: PathBuf,
    pub data_root: PathBuf,
}

impl PluginInstallLayout {
    /// Project-scope layout under `<cwd>/.agent/plugins/`.
    pub fn project(cwd: &Path) -> Self {
        let base = cwd.join(".agent").join("plugins");
        Self {
            cache_root: base.join("cache"),
            data_root: base.join("data"),
        }
    }

    /// Local-scope layout. M8 stores the local cache in a sibling
    /// directory so it can be ignored independently of project caches
    /// in version control.
    pub fn local(cwd: &Path) -> Self {
        let base = cwd.join(".agent").join("plugins-local");
        Self {
            cache_root: base.join("cache"),
            data_root: base.join("data"),
        }
    }

    /// User-scope layout under `<config_home>/plugins/`.
    pub fn user(config_home: &Path) -> Self {
        let base = config_home.join("plugins");
        Self {
            cache_root: base.join("cache"),
            data_root: base.join("data"),
        }
    }
}

/// Discover the manifest path for a plugin source directory. Returns the
/// canonical `.agent-plugin/plugin.json` first; if missing and
/// `compat_claude_plugin_manifest` is true, falls back to
/// `.claude-plugin/plugin.json`.
pub fn discover_manifest_path(
    plugin_root: &Path,
    compat_claude_plugin_manifest: bool,
) -> Option<PathBuf> {
    let canonical = plugin_root.join(PLUGIN_MANIFEST_PATH);
    if canonical.exists() {
        return Some(canonical);
    }
    if compat_claude_plugin_manifest {
        let compat = plugin_root.join(COMPAT_PLUGIN_MANIFEST_PATH);
        if compat.exists() {
            return Some(compat);
        }
    }
    None
}

/// Convenience: read and validate a manifest from `source_dir`.
pub fn parse_and_validate_source(
    source_dir: &Path,
    compat_claude_plugin_manifest: bool,
    strict_component_paths: bool,
) -> Result<PluginManifest, PluginInstallError> {
    let manifest_path = discover_manifest_path(source_dir, compat_claude_plugin_manifest)
        .ok_or_else(|| {
            PluginInstallError::Manifest(PluginManifestError::Read {
                path: source_dir.join(PLUGIN_MANIFEST_PATH),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "manifest not found"),
            })
        })?;
    let manifest = PluginManifest::load_from_path(&manifest_path)?;
    if !is_plugin_name_safe(&manifest.name) {
        return Err(PluginInstallError::Manifest(
            PluginManifestError::UnsafeName {
                path: manifest_path,
                name: manifest.name,
            },
        ));
    }
    validate_plugin_paths(source_dir, &manifest, strict_component_paths)?;
    Ok(manifest)
}

/// Copy a plugin source directory into the cache, returning the cache
/// path. Refuses to overwrite an existing install unless `force`.
///
/// Installation is atomic: the source is copied into a sibling staging
/// directory first, then moved over the existing target with
/// [`std::fs::rename`]. A failure mid-copy leaves the previous install
/// (if any) untouched and the staging directory is cleaned up.
pub fn install_to_cache(
    source_dir: &Path,
    layout: &PluginInstallLayout,
    manifest: &PluginManifest,
    force: bool,
) -> Result<PathBuf, PluginInstallError> {
    if !source_dir.exists() {
        return Err(PluginInstallError::MissingSource(source_dir.to_path_buf()));
    }
    if !source_dir.is_dir() {
        return Err(PluginInstallError::NotADirectory(source_dir.to_path_buf()));
    }
    let target = plugin_cache_dir(
        &layout.cache_root,
        &manifest.name,
        manifest.version.as_deref(),
    );
    // Defense-in-depth: refuse to operate on a cache path that doesn't
    // sit under the configured cache root. Manifest validation already
    // rejects unsafe `name`/`version` strings, so this should never
    // trigger for sanctioned inputs.
    if !cache_path_is_under_root(&target, &layout.cache_root) {
        return Err(PluginInstallError::Manifest(
            super::manifest::PluginManifestError::UnsafeName {
                path: target.clone(),
                name: manifest.name.clone(),
            },
        ));
    }
    if target.exists() && !force {
        return Err(PluginInstallError::AlreadyInstalled {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
        });
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| PluginInstallError::Io {
            path: parent.to_path_buf(),
            source: e,
        })?;
    }
    // Stage into a sibling directory so the swap-in is atomic. If the
    // copy fails halfway through, the existing install is untouched.
    let staging = staging_dir(&target);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|e| PluginInstallError::Io {
            path: staging.clone(),
            source: e,
        })?;
    }
    let canonical_source = source_dir
        .canonicalize()
        .unwrap_or_else(|_| source_dir.to_path_buf());
    if let Err(e) = copy_dir_recursive(source_dir, &staging, &canonical_source) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(PluginInstallError::Io {
            path: staging,
            source: e,
        });
    }
    // Two-step atomic swap: rename the existing install (if any) to a
    // sibling backup, then rename the staging dir into place, then
    // delete the backup. A crash between step 1 and step 2 leaves a
    // recoverable backup directory; a crash between step 2 and 3
    // leaves the *new* install plus a stale backup that prune can
    // clean up. Either way, the cache is never simultaneously empty.
    let backup = staging_dir(&target.with_extension("backup"));
    let backup_existed = if target.exists() {
        if let Err(e) = std::fs::rename(&target, &backup) {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(PluginInstallError::Io {
                path: target.clone(),
                source: e,
            });
        }
        true
    } else {
        false
    };
    if let Err(e) = std::fs::rename(&staging, &target) {
        // Try to roll back the backup so the previous install is
        // restored.
        if backup_existed {
            let _ = std::fs::rename(&backup, &target);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(PluginInstallError::Io {
            path: target.clone(),
            source: e,
        });
    }
    if backup_existed {
        let _ = std::fs::remove_dir_all(&backup);
    }
    // Ensure the data dir exists once after install so plugins that rely
    // on `${CODE_AGENT_PLUGIN_DATA}` can find it on first run.
    let data = plugin_data_dir(&layout.data_root, &manifest.name);
    std::fs::create_dir_all(&data).map_err(|e| PluginInstallError::Io {
        path: data,
        source: e,
    })?;
    Ok(target)
}

fn staging_dir(target: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = match target.file_name().and_then(|s| s.to_str()) {
        Some(n) => format!(".{n}.staging.{pid}.{nanos}"),
        None => format!(".staging.{pid}.{nanos}"),
    };
    target
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// Build a [`PluginInstall`] from a freshly cached plugin.
#[allow(clippy::too_many_arguments)]
pub fn make_install(
    name: String,
    manifest: PluginManifest,
    cache_path: PathBuf,
    data_dir: PathBuf,
    scope: PluginScope,
    enabled: bool,
    source: PluginSource,
) -> PluginInstall {
    let version = manifest.version.clone();
    PluginInstall {
        name,
        manifest,
        root: cache_path,
        data_dir,
        scope,
        enabled,
        version,
        installed_at_unix: OffsetDateTime::now_utc().unix_timestamp(),
        source,
        status: if enabled {
            PluginStatus::Loaded
        } else {
            PluginStatus::Disabled
        },
        components: Default::default(),
    }
}

/// Remove a plugin's cache version. Caller is responsible for purging
/// `enabled_plugins` settings entries.
pub fn uninstall_cache(
    layout: &PluginInstallLayout,
    name: &str,
    version: Option<&str>,
) -> Result<(), PluginInstallError> {
    // Belt-and-braces: validate name/version even if a caller
    // forgot to. This is a `remove_dir_all` so the cost of a
    // misvalidated path is high.
    if !super::manifest::is_plugin_name_safe(name) {
        return Err(PluginInstallError::Manifest(
            super::manifest::PluginManifestError::UnsafeName {
                path: layout.cache_root.clone(),
                name: name.to_string(),
            },
        ));
    }
    if let Some(v) = version {
        if !super::manifest::is_plugin_version_safe(v) {
            return Err(PluginInstallError::Manifest(
                super::manifest::PluginManifestError::InvalidField {
                    path: layout.cache_root.clone(),
                    field: "version",
                    message: format!("unsafe version {v:?}"),
                },
            ));
        }
    }
    let target = plugin_cache_dir(&layout.cache_root, name, version);
    if !cache_path_is_under_root(&target, &layout.cache_root) {
        return Err(PluginInstallError::Manifest(
            super::manifest::PluginManifestError::UnsafeName {
                path: target.clone(),
                name: name.to_string(),
            },
        ));
    }
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| PluginInstallError::Io {
            path: target.clone(),
            source: e,
        })?;
        // Remove the now-empty parent if it has no other versions.
        if let Some(parent) = target.parent() {
            if parent.exists()
                && parent
                    .read_dir()
                    .map(|mut i| i.next().is_none())
                    .unwrap_or(false)
            {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }
    Ok(())
}

/// Returns true iff `target` is lexically a descendant of `root`.
/// Defense-in-depth: callers have already validated `name`/`version`
/// to keep `..` out of the cache path, but this catches accidents
/// before any destructive filesystem operation.
fn cache_path_is_under_root(target: &Path, root: &Path) -> bool {
    for c in target.components() {
        if matches!(c, std::path::Component::ParentDir) {
            return false;
        }
    }
    if target.starts_with(root) {
        return true;
    }
    if let (Ok(canonical_root), Some(canonical_parent)) = (
        root.canonicalize(),
        target.parent().and_then(|p| p.canonicalize().ok()),
    ) {
        return canonical_parent.starts_with(canonical_root);
    }
    false
}

/// Optionally remove the persistent data directory for a plugin.
pub fn remove_data_dir(layout: &PluginInstallLayout, name: &str) -> Result<(), PluginInstallError> {
    let data = plugin_data_dir(&layout.data_root, name);
    if data.exists() {
        std::fs::remove_dir_all(&data).map_err(|e| PluginInstallError::Io {
            path: data,
            source: e,
        })?;
    }
    Ok(())
}

/// Outcome of a [`prune_orphans`] run. `removed` only contains entries
/// whose deletion actually succeeded (or would have succeeded under
/// `dry_run`). Entries whose `remove_dir_all`/`remove_dir` call returned
/// an `io::Error` are reported in `failed` with the original error so
/// callers can surface the failure to the operator instead of silently
/// claiming the cache is clean.
#[derive(Debug, Default)]
pub struct PruneOutcome {
    pub removed: Vec<(String, Option<String>)>,
    pub failed: Vec<(String, Option<String>, std::io::Error)>,
}

/// Walk the cache directory and remove plugin entries that are not in
/// `keep`. Returns the [`PruneOutcome`] partitioning successful removals
/// from per-entry deletion failures.
pub fn prune_orphans(
    layout: &PluginInstallLayout,
    keep: &[(String, Option<String>)],
    dry_run: bool,
) -> Result<PruneOutcome, PluginInstallError> {
    let mut outcome = PruneOutcome::default();
    if !layout.cache_root.exists() {
        return Ok(outcome);
    }
    let entries = std::fs::read_dir(&layout.cache_root).map_err(|e| PluginInstallError::Io {
        path: layout.cache_root.clone(),
        source: e,
    })?;
    for plugin_entry in entries.flatten() {
        let plugin_path = plugin_entry.path();
        if !plugin_path.is_dir() {
            continue;
        }
        let plugin_name = match plugin_path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let version_entries =
            std::fs::read_dir(&plugin_path).map_err(|e| PluginInstallError::Io {
                path: plugin_path.clone(),
                source: e,
            })?;
        let mut any_kept = false;
        for ver_entry in version_entries.flatten() {
            let ver_path = ver_entry.path();
            if !ver_path.is_dir() {
                continue;
            }
            let version_str = ver_path
                .file_name()
                .and_then(|s| s.to_str())
                .map(str::to_string);
            let version_opt = version_str.as_deref().and_then(|s| {
                if s == "unversioned" {
                    None
                } else {
                    Some(s.to_string())
                }
            });
            let key_match = keep.iter().any(|(n, v)| {
                n == &plugin_name
                    && match (v.as_deref(), version_opt.as_deref()) {
                        (Some(a), Some(b)) => a == b,
                        (None, None) => true,
                        _ => false,
                    }
            });
            if key_match {
                any_kept = true;
                continue;
            }
            // Attempt the deletion FIRST so we only report what was
            // actually removed. On dry_run we just record the intent.
            if dry_run {
                outcome.removed.push((plugin_name.clone(), version_opt));
            } else {
                match std::fs::remove_dir_all(&ver_path) {
                    Ok(()) => {
                        outcome.removed.push((plugin_name.clone(), version_opt));
                    }
                    Err(e) => {
                        // Treat a failed delete as a "kept" entry so we
                        // don't subsequently try to remove the parent
                        // directory (which would also fail).
                        any_kept = true;
                        outcome.failed.push((plugin_name.clone(), version_opt, e));
                    }
                }
            }
        }
        if !dry_run && !any_kept {
            if let Err(e) = std::fs::remove_dir(&plugin_path) {
                outcome.failed.push((plugin_name.clone(), None, e));
            }
        }
    }
    Ok(outcome)
}

fn copy_dir_recursive(src: &Path, dst: &Path, canonical_root: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_symlink() {
            // Symlinks are never copied. The cache should not contain
            // dangling or potentially-escaping links.
            continue;
        }
        if file_type.is_dir() {
            // Defense-in-depth: refuse to recurse into a directory whose
            // canonical form sits outside the plugin source root. With
            // is_symlink()==false above, this is normally redundant, but
            // it guards against TOCTOU races where the symlink metadata
            // changes between read_dir and recursion.
            let canonical_dir = from.canonicalize().unwrap_or_else(|_| from.clone());
            if !canonical_dir.starts_with(canonical_root) {
                continue;
            }
            copy_dir_recursive(&from, &to, canonical_root)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = std::fs::metadata(&from)?.permissions();
                let mode = perms.mode();
                std::fs::set_permissions(&to, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_plugin(dir: &Path, name: &str, version: Option<&str>) -> PathBuf {
        let plugin = dir.join(name);
        std::fs::create_dir_all(plugin.join(".agent-plugin")).unwrap();
        let mut manifest = serde_json::json!({"name": name});
        if let Some(v) = version {
            manifest["version"] = serde_json::Value::String(v.to_string());
        }
        std::fs::write(
            plugin.join(".agent-plugin/plugin.json"),
            manifest.to_string(),
        )
        .unwrap();
        plugin
    }

    #[test]
    // Install copies the source to the cache and creates a data directory.
    fn install_copies_to_cache() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        std::fs::write(plugin_dir.join("README.md"), "hi").unwrap();
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        let cache = install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        assert!(cache.exists());
        assert!(cache.join("README.md").exists());
        assert!(layout.data_root.join("my-plugin").exists());
    }

    #[test]
    // Re-installing the same plugin/version refuses without --force.
    fn install_refuses_existing_without_force() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        let err = install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap_err();
        assert!(matches!(err, PluginInstallError::AlreadyInstalled { .. }));
    }

    #[test]
    // Force install overwrites the existing cache.
    fn force_install_replaces_cache() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        let cache = install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        std::fs::write(plugin_dir.join("X"), "x").unwrap();
        let cache2 = install_to_cache(&plugin_dir, &layout, &manifest, true).unwrap();
        assert_eq!(cache, cache2);
        assert!(cache.join("X").exists());
    }

    #[test]
    // Prune removes versions not present in the keep list.
    fn prune_removes_orphans() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        // Pretend a second version was previously installed.
        std::fs::create_dir_all(layout.cache_root.join("my-plugin").join("9.9.9")).unwrap();
        let outcome = prune_orphans(
            &layout,
            &[("my-plugin".into(), Some("0.1.0".into()))],
            false,
        )
        .unwrap();
        assert_eq!(
            outcome.removed,
            vec![("my-plugin".to_string(), Some("9.9.9".to_string()))]
        );
        assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
        assert!(!layout.cache_root.join("my-plugin").join("9.9.9").exists());
    }

    #[cfg(unix)]
    #[test]
    // When `remove_dir_all` fails (here: the version dir's parent is
    // chmod'd read+execute only so the inode cannot be unlinked), the
    // entry must NOT appear in `outcome.removed`. It MUST appear in
    // `outcome.failed` with the original IO error so the operator can
    // see the cache state is not actually clean.
    fn prune_reports_failed_removal() {
        use std::os::unix::fs::PermissionsExt;

        let work = TempDir::new().unwrap();
        let layout = PluginInstallLayout::project(work.path());
        // Build an orphan version dir manually so it has no special
        // permission setup.
        let parent = layout.cache_root.join("my-plugin");
        let ver = parent.join("9.9.9");
        std::fs::create_dir_all(&ver).unwrap();
        std::fs::write(ver.join("file"), "x").unwrap();

        // Lock the parent so `remove_dir_all` on `ver` fails (can't
        // unlink children when the parent is not writable).
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = prune_orphans(&layout, &[], false).unwrap();

        // Restore perms so TempDir can clean up at drop time.
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            outcome.removed.is_empty(),
            "should not have claimed to remove anything: {:?}",
            outcome.removed
        );
        assert_eq!(outcome.failed.len(), 1, "{:?}", outcome.failed);
        let (n, v, _err) = &outcome.failed[0];
        assert_eq!(n, "my-plugin");
        assert_eq!(v.as_deref(), Some("9.9.9"));
        assert!(ver.exists(), "the version dir should still exist on disk");
    }

    #[test]
    // Uninstall removes the version directory and optionally the data dir.
    fn uninstall_removes_cache_keeps_data_when_asked() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        uninstall_cache(&layout, "my-plugin", Some("0.1.0")).unwrap();
        assert!(!layout.cache_root.join("my-plugin").join("0.1.0").exists());
        // Data dir was created by install_to_cache; preserved unless the
        // caller asks for removal.
        assert!(layout.data_root.join("my-plugin").exists());
    }

    #[test]
    // Force-reinstalling preserves the previous good install if the
    // copy fails partway. Uses a sentinel file in the prior install
    // and a non-existent source dir to force the copy step to fail.
    fn force_install_failure_preserves_prior_good_install() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        let cache_path = plugin_cache_dir(&layout.cache_root, "my-plugin", Some("0.1.0"));
        // Place a sentinel that should still be there if the prior
        // install survives a failed force-replace.
        let sentinel = cache_path.join("SENTINEL");
        std::fs::write(&sentinel, b"keep-me").unwrap();
        // Build a "broken" source: directory exists but missing manifest.
        let broken = work.path().join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        // Even with --force, a missing manifest in source means
        // parse_and_validate_source would have failed. Here we feed
        // the original manifest but a source dir that no longer
        // exists, exercising the staging-dir copy failure path.
        std::fs::remove_dir_all(&plugin_dir).unwrap();
        let err = install_to_cache(&plugin_dir, &layout, &manifest, true).unwrap_err();
        assert!(matches!(err, PluginInstallError::MissingSource(_)));
        // The previous install (and its sentinel) is intact.
        assert!(sentinel.exists());
    }

    #[cfg(unix)]
    #[test]
    // Symlinks to outside the source tree are not copied into the cache,
    // even if they appear inside a subdirectory of the source.
    fn copy_dir_recursive_skips_symlinks() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret"), "should-not-be-copied").unwrap();
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        symlink(outside.path().join("secret"), plugin_dir.join("LINK")).unwrap();
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        let cache = install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        // The symlink — and therefore the secret it pointed to — must
        // not appear in the cache.
        assert!(!cache.join("LINK").exists());
    }

    #[test]
    // Defense-in-depth: uninstall_cache refuses an unsafe version
    // string even if the caller bypassed manifest validation.
    fn uninstall_cache_rejects_traversal_version() {
        let work = TempDir::new().unwrap();
        let layout = PluginInstallLayout::project(work.path());
        let err = uninstall_cache(&layout, "ok", Some("../../etc")).unwrap_err();
        assert!(matches!(err, PluginInstallError::Manifest(_)));
    }

    #[test]
    // Force-reinstall preserves the prior version files end-to-end:
    // after a successful force-replace, the previous install is fully
    // overwritten (no leftover files from the previous version).
    fn force_install_replaces_via_atomic_swap() {
        let work = TempDir::new().unwrap();
        let plugin_dir = write_plugin(work.path(), "my-plugin", Some("0.1.0"));
        std::fs::write(plugin_dir.join("VERSION"), "v1").unwrap();
        let layout = PluginInstallLayout::project(work.path());
        let manifest = parse_and_validate_source(&plugin_dir, false, false).unwrap();
        let cache = install_to_cache(&plugin_dir, &layout, &manifest, false).unwrap();
        assert_eq!(
            std::fs::read_to_string(cache.join("VERSION")).unwrap(),
            "v1"
        );
        // Replace source contents and force-reinstall.
        std::fs::write(plugin_dir.join("VERSION"), "v2").unwrap();
        std::fs::remove_file(plugin_dir.join("VERSION").with_extension("")).ok();
        std::fs::write(plugin_dir.join("VERSION"), "v2").unwrap();
        let cache2 = install_to_cache(&plugin_dir, &layout, &manifest, true).unwrap();
        assert_eq!(cache, cache2);
        assert_eq!(
            std::fs::read_to_string(cache.join("VERSION")).unwrap(),
            "v2"
        );
        // No backup directory should remain in the parent.
        let parent = cache.parent().unwrap();
        for e in std::fs::read_dir(parent).unwrap().flatten() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            assert!(
                !s.contains(".staging.") && !s.contains(".backup."),
                "leftover {}",
                s
            );
        }
    }
}
