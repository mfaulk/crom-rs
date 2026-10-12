//! Plugin system (M8).
//!
//! Plugins package reusable agent behaviour — skills, slash commands,
//! agents, hooks, MCP servers, settings defaults, helper binaries, and
//! resources — into installable, versioned, namespaced bundles. M8
//! implements local-only install/cache/load with explicit
//! opt-in for hooks, MCP, and bin PATH.

pub mod install;
pub mod loader;
pub mod manifest;
pub mod marketplace;
pub mod registry;
pub mod runtime;
pub mod validate;
pub mod vars;

pub use install::{
    discover_manifest_path, install_to_cache, make_install, parse_and_validate_source,
    prune_orphans, remove_data_dir, uninstall_cache, PluginInstallError, PluginInstallLayout,
    PruneOutcome,
};
pub use loader::{
    load_plugin_components, LoadedPluginComponents, PluginComponentFailure, PluginHookCommand,
    PluginHookMatcherEntry, PluginHooksFile, PluginLoadError, PluginLoadOptions, PluginLoadedHook,
    PLUGIN_HOOK_DEFAULT_TIMEOUT,
};
pub use manifest::{
    is_plugin_name_safe, is_plugin_version_safe, resolve_safe_plugin_path, PluginAuthor,
    PluginManifest, PluginManifestError, COMPAT_PLUGIN_MANIFEST_PATH, MAX_MANIFEST_BYTES,
    MAX_PLUGIN_NAME_LEN, PLUGIN_MANIFEST_PATH,
};
pub use marketplace::{
    LocalMarketplaceError, LocalPluginMarketplace, LocalPluginMarketplaceEntry,
    PluginMarketplaceOwner,
};
pub use registry::{
    plugin_cache_dir, plugin_data_dir, PluginComponentKind, PluginComponentSummary, PluginInstall,
    PluginRegistry, PluginRuntimePaths, PluginScope, PluginSource, PluginStatus, PluginSummary,
};
pub use runtime::{
    build_plugin_runtime, layout_for_scope, merge_plugin_defaults_into_settings, redacted_display,
    EffectivePluginConfig, PluginLoadContext, PluginRuntimeBuild, PluginValidationFailure,
};
pub use validate::{validate_plugin_paths, PluginValidationError};
pub use vars::{substitute_str, substitute_value, PluginSubstError, PluginVarContext};
