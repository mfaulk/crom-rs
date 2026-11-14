//! M8 plugin CLI commands.
//!
//! These commands are shells around `crom-core::plugins`. They
//! parse, validate, install, enable/disable, list, show, prune, init,
//! and reload plugins, and they manipulate `enabled_plugins` entries
//! inside the appropriate settings file.

use std::fs;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use crom_core::{
    build_plugin_runtime, install_to_cache, parse_and_validate_source, plugin_data_dir,
    prune_orphans, redacted_display, remove_data_dir, uninstall_cache, EnabledPluginEntry,
    LocalPluginMarketplace, PluginInstall, PluginInstallLayout, PluginLoadContext, PluginScope,
    PluginSource, SettingSource, Settings,
};

#[derive(Subcommand, Debug)]
pub enum PluginCommand {
    /// Install a plugin from a local path or marketplace entry.
    Install(PluginInstallArgs),
    /// Remove an installed plugin.
    Uninstall(PluginUninstallArgs),
    /// Enable an installed plugin in the given scope.
    Enable(PluginEnableArgs),
    /// Disable an installed plugin in the given scope.
    Disable(PluginEnableArgs),
    /// List installed plugins.
    List(PluginListArgs),
    /// Show details of one installed plugin.
    Show(PluginShowArgs),
    /// Parse and validate a plugin source directory.
    Validate(PluginValidateArgs),
    /// Reload plugin manifests and components for the workspace.
    Reload(PluginReloadArgs),
    /// Create a new plugin skeleton.
    Init(PluginInitArgs),
    /// Remove orphaned plugin cache versions.
    Prune(PluginPruneArgs),
    /// Inspect or register local plugin marketplaces.
    #[command(subcommand)]
    Marketplace(PluginMarketplaceCommand),
}

#[derive(Subcommand, Debug)]
pub enum PluginMarketplaceCommand {
    /// Add a local marketplace JSON file to the workspace.
    Add(MarketplaceAddArgs),
    /// List registered marketplaces.
    List(WorkspaceArgs),
    /// Show one marketplace's plugins.
    Show(MarketplaceShowArgs),
}

#[derive(Parser, Debug)]
pub struct WorkspaceArgs {
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
    /// Settings sources to consult.
    #[arg(long, default_value = "user-and-project")]
    pub settings: String,
}

#[derive(Parser, Debug)]
pub struct PluginInstallArgs {
    /// Path to the plugin directory or `<entry>@<marketplace>` token.
    pub source: String,
    #[arg(long, default_value = "project")]
    pub scope: String,
    #[arg(long)]
    pub enable: bool,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub force: bool,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginUninstallArgs {
    pub plugin: String,
    #[arg(long, default_value = "project")]
    pub scope: String,
    #[arg(long)]
    pub keep_data: bool,
    #[arg(long)]
    pub yes: bool,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginEnableArgs {
    pub plugin: String,
    #[arg(long, default_value = "project")]
    pub scope: String,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginListArgs {
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginShowArgs {
    pub plugin: String,
    #[arg(long)]
    pub json: bool,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginValidateArgs {
    pub source: PathBuf,
    #[arg(long)]
    pub compat_claude_plugin_manifest: bool,
    #[arg(long)]
    pub strict: bool,
}

#[derive(Parser, Debug)]
pub struct PluginReloadArgs {
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct PluginInitArgs {
    pub name: String,
    #[arg(long, default_value = "basic")]
    pub template: String,
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,
}

#[derive(Parser, Debug)]
pub struct PluginPruneArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub yes: bool,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct MarketplaceAddArgs {
    pub source: PathBuf,
    #[arg(long, default_value = "project")]
    pub scope: String,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

#[derive(Parser, Debug)]
pub struct MarketplaceShowArgs {
    pub name: String,
    #[command(flatten)]
    pub workspace: WorkspaceArgs,
}

fn parse_scope(s: &str) -> Result<PluginScope> {
    PluginScope::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --scope: {s}"))
}

fn parse_setting_source(s: &str) -> Result<SettingSource> {
    SettingSource::from_str_lenient(s).ok_or_else(|| anyhow!("invalid --settings: {s}"))
}

fn user_config_home() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("CROM_CONFIG_HOME") {
        return Some(PathBuf::from(p));
    }
    if let Ok(p) = std::env::var("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(p).join("crom"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home).join(".config").join("crom"));
    }
    None
}

fn settings_path_for_scope(cwd: &Path, scope: PluginScope) -> Result<PathBuf> {
    match scope {
        PluginScope::Project | PluginScope::Managed => Ok(cwd.join(".agent").join("settings.json")),
        PluginScope::Local => Ok(cwd.join(".agent").join("settings.local.json")),
        PluginScope::User => user_config_home()
            .map(|h| h.join("settings.json"))
            .ok_or_else(|| anyhow!("could not resolve user-config home (set CROM_CONFIG_HOME)")),
    }
}

fn read_settings_json(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
}

fn write_settings_json(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(value)?;
    fs::write(path, format!("{pretty}\n")).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[allow(dead_code)]
fn load_settings(cwd: &Path) -> Result<Settings> {
    load_settings_with_source(cwd, SettingSource::UserAndProject)
}

fn load_settings_with_source(cwd: &Path, source: SettingSource) -> Result<Settings> {
    let mut s = crom_core::settings::load(cwd, source).context("load settings")?;
    // settings.local.json (M8) — supplements project settings without
    // overriding user/project settings on scalars (used for personal,
    // gitignored config). Only consult it when the project source is
    // included.
    if matches!(
        source,
        SettingSource::Project | SettingSource::UserAndProject
    ) {
        let local_path = cwd.join(".agent").join("settings.local.json");
        if local_path.exists() {
            let bytes =
                fs::read(&local_path).with_context(|| format!("read {}", local_path.display()))?;
            if !bytes.is_empty() {
                let local: crom_core::SettingsFile = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parse {}", local_path.display()))?;
                apply_local_settings_overlay(&mut s, local);
            }
        }
    }
    Ok(s)
}

fn apply_local_settings_overlay(s: &mut Settings, local: crom_core::SettingsFile) {
    if let Some(plugins) = local.plugins {
        let target = s.plugins.get_or_insert_with(Default::default);
        // settings.local.json must NOT silently override safety toggles
        // already set in user/project. List-like fields concatenate
        // (additive); scalar toggles only fill in unset values.
        if target.enabled.is_none() {
            target.enabled = plugins.enabled;
        }
        if let Some(list) = plugins.enabled_plugins {
            let merged = target.enabled_plugins.get_or_insert_with(Vec::new);
            for entry in list {
                if let Some(existing) = merged.iter_mut().find(|e| e.name == entry.name) {
                    *existing = entry;
                } else {
                    merged.push(entry);
                }
            }
        }
        if let Some(list) = plugins.disabled_plugins {
            let merged = target.disabled_plugins.get_or_insert_with(Vec::new);
            for n in list {
                if !merged.contains(&n) {
                    merged.push(n);
                }
            }
        }
        if target.allow_local_plugin_dirs.is_none() {
            target.allow_local_plugin_dirs = plugins.allow_local_plugin_dirs;
        }
        if target.allow_plugin_bin_path.is_none() {
            target.allow_plugin_bin_path = plugins.allow_plugin_bin_path;
        }
        if target.allow_plugin_hooks.is_none() {
            target.allow_plugin_hooks = plugins.allow_plugin_hooks;
        }
        if target.allow_plugin_mcp.is_none() {
            target.allow_plugin_mcp = plugins.allow_plugin_mcp;
        }
        if target.allow_plugin_settings.is_none() {
            target.allow_plugin_settings = plugins.allow_plugin_settings;
        }
        if target.compat_claude_plugin_manifest.is_none() {
            target.compat_claude_plugin_manifest = plugins.compat_claude_plugin_manifest;
        }
        if target.max_plugins.is_none() {
            target.max_plugins = plugins.max_plugins;
        }
        if target.strict_component_paths.is_none() {
            target.strict_component_paths = plugins.strict_component_paths;
        }
        if let Some(list) = plugins.marketplaces {
            let merged = target.marketplaces.get_or_insert_with(Vec::new);
            for p in list {
                if !merged.contains(&p) {
                    merged.push(p);
                }
            }
        }
    }
    if let Some(pc) = local.plugin_config {
        for (k, v) in pc {
            s.plugin_config.insert(k, v);
        }
    }
}

fn upsert_enabled_entry(
    settings_value: &mut serde_json::Value,
    entry: &EnabledPluginEntry,
    enabled: bool,
) -> Result<()> {
    let obj = settings_value
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root must be a JSON object"))?;
    let plugins = obj
        .entry("plugins".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let pmap = plugins
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json `plugins` must be an object"))?;
    // Always record the install in `enabled_plugins` so prune knows
    // which cache versions to keep, even when the plugin is added in
    // a disabled state on first install. The `disabled_plugins` list
    // gates whether the runtime *loads* it.
    let enabled_list = pmap
        .entry("enabled_plugins".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let enabled_arr = enabled_list
        .as_array_mut()
        .ok_or_else(|| anyhow!("settings.json `plugins.enabled_plugins` must be a list"))?;
    enabled_arr.retain(|item| {
        item.get("name")
            .and_then(|v| v.as_str())
            .map(|n| n != entry.name)
            .unwrap_or(true)
    });
    enabled_arr.push(serde_json::json!({
        "name": entry.name,
        "version": entry.version,
        "scope": entry.scope,
    }));
    if enabled {
        // Make sure the plugin is not also flagged disabled.
        if let Some(serde_json::Value::Array(d)) = pmap.get_mut("disabled_plugins") {
            d.retain(|v| v.as_str().map(|s| s != entry.name).unwrap_or(true));
        }
    } else {
        let list = pmap
            .entry("disabled_plugins".to_string())
            .or_insert_with(|| serde_json::Value::Array(Vec::new()));
        let arr = list
            .as_array_mut()
            .ok_or_else(|| anyhow!("settings.json `plugins.disabled_plugins` must be a list"))?;
        if !arr.iter().any(|v| v.as_str() == Some(entry.name.as_str())) {
            arr.push(serde_json::Value::String(entry.name.clone()));
        }
    }
    Ok(())
}

fn remove_enabled_entry(settings_value: &mut serde_json::Value, name: &str) {
    if let Some(p) = settings_value
        .get_mut("plugins")
        .and_then(|v| v.as_object_mut())
    {
        if let Some(serde_json::Value::Array(arr)) = p.get_mut("enabled_plugins") {
            arr.retain(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(|n| n != name)
                    .unwrap_or(true)
            });
        }
        if let Some(serde_json::Value::Array(d)) = p.get_mut("disabled_plugins") {
            d.retain(|v| v.as_str().map(|s| s != name).unwrap_or(true));
        }
    }
}

fn print_warning_banner() {
    eprintln!(
        "Warning: plugins can add skills, commands, agents, hooks, MCP servers, and helper binaries."
    );
    eprintln!("Only install plugins from sources you trust.");
}

fn print_install_summary(install: &PluginInstall) {
    println!(
        "[plugin] installed {}{} to {} scope",
        install.name,
        install
            .version
            .as_deref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default(),
        install.scope.as_str()
    );
}

pub async fn handle_plugin_command(cmd: PluginCommand) -> Result<()> {
    match cmd {
        PluginCommand::Install(args) => cmd_install(args).await,
        PluginCommand::Uninstall(args) => cmd_uninstall(args).await,
        PluginCommand::Enable(args) => cmd_set_enabled(args, true).await,
        PluginCommand::Disable(args) => cmd_set_enabled(args, false).await,
        PluginCommand::List(args) => cmd_list(args).await,
        PluginCommand::Show(args) => cmd_show(args).await,
        PluginCommand::Validate(args) => cmd_validate(args).await,
        PluginCommand::Reload(args) => cmd_reload(args).await,
        PluginCommand::Init(args) => cmd_init(args).await,
        PluginCommand::Prune(args) => cmd_prune(args).await,
        PluginCommand::Marketplace(sub) => match sub {
            PluginMarketplaceCommand::Add(a) => cmd_marketplace_add(a).await,
            PluginMarketplaceCommand::List(a) => cmd_marketplace_list(a).await,
            PluginMarketplaceCommand::Show(a) => cmd_marketplace_show(a).await,
        },
    }
}

async fn cmd_install(args: PluginInstallArgs) -> Result<()> {
    let source = parse_setting_source(&args.workspace.settings)?;
    let scope = parse_scope(&args.scope)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let layout = match scope {
        PluginScope::Project | PluginScope::Managed => PluginInstallLayout::project(&cwd),
        PluginScope::Local => PluginInstallLayout::local(&cwd),
        PluginScope::User => {
            let home =
                user_config_home().ok_or_else(|| anyhow!("user config home not resolvable"))?;
            PluginInstallLayout::user(&home)
        }
    };
    print_warning_banner();

    // Resolve the source. Format: `path` or `entry@marketplace`.
    let strict_marketplace = settings_strict_component_paths_with_source(&cwd, source);
    let (source_dir, marketplace_entry) = if args.source.contains('@') {
        let (entry, market) = args
            .source
            .split_once('@')
            .ok_or_else(|| anyhow!("expected <entry>@<marketplace>"))?;
        let market_path = resolve_marketplace_path_with_source(&cwd, market, source)?;
        let market = LocalPluginMarketplace::load(&market_path)
            .with_context(|| format!("load marketplace {}", market_path.display()))?;
        let m_entry = market
            .find(entry)
            .ok_or_else(|| anyhow!("marketplace {} has no entry {entry}", market.name))?;
        if strict_marketplace && market.entry_escapes_parent(m_entry) {
            return Err(anyhow!(
                "marketplace {} entry {} source {:?} escapes the marketplace directory; \
                 disable strict_component_paths or move the source under the marketplace tree",
                market.name,
                m_entry.name,
                m_entry.source,
            ));
        }
        let resolved = market
            .resolved_source(m_entry)
            .canonicalize()
            .with_context(|| {
                format!(
                    "marketplace entry {} source {} could not be canonicalized",
                    m_entry.name,
                    market.resolved_source(m_entry).display()
                )
            })?;
        (resolved, Some((market.name.clone(), m_entry.name.clone())))
    } else {
        (
            PathBuf::from(&args.source)
                .canonicalize()
                .context("--source")?,
            None,
        )
    };

    let manifest = parse_and_validate_source(&source_dir, false, true)?;
    if args.dry_run {
        println!(
            "[plugin] would install {}{} to {} scope from {}",
            manifest.name,
            manifest
                .version
                .as_deref()
                .map(|v| format!("@{v}"))
                .unwrap_or_default(),
            scope.as_str(),
            redacted_display(&source_dir)
        );
        return Ok(());
    }

    let cache_path = install_to_cache(&source_dir, &layout, &manifest, args.force)?;
    let data_dir = plugin_data_dir(&layout.data_root, &manifest.name);
    let source = if let Some((market, entry)) = marketplace_entry.clone() {
        PluginSource::LocalMarketplace {
            marketplace: market,
            entry,
            source_path: source_dir.clone(),
        }
    } else {
        PluginSource::LocalPath(source_dir.clone())
    };
    let install = crom_core::make_install(
        manifest.name.clone(),
        manifest.clone(),
        cache_path,
        data_dir,
        scope,
        args.enable,
        source,
    );

    // Update settings.json for the chosen scope.
    let settings_path = settings_path_for_scope(&cwd, scope)?;
    let mut value = read_settings_json(&settings_path)?;
    upsert_enabled_entry(
        &mut value,
        &EnabledPluginEntry {
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            scope: Some(scope.as_str().to_string()),
        },
        args.enable,
    )?;
    write_settings_json(&settings_path, &value)?;
    print_install_summary(&install);
    if let Some((m, entry)) = marketplace_entry {
        println!("[plugin] installed from marketplace {} entry {}", m, entry);
    }
    Ok(())
}

fn resolve_marketplace_path(cwd: &Path, name_or_path: &str) -> Result<PathBuf> {
    resolve_marketplace_path_with_source(cwd, name_or_path, SettingSource::UserAndProject)
}

fn resolve_marketplace_path_with_source(
    cwd: &Path,
    name_or_path: &str,
    source: SettingSource,
) -> Result<PathBuf> {
    // Look up by registered name FIRST so a local directory whose name
    // happens to match a registered marketplace cannot mask the
    // intended file (Gemini review feedback). Absolute paths still
    // win — they are an unambiguous user choice.
    let candidate = PathBuf::from(name_or_path);
    if candidate.is_absolute() && candidate.exists() {
        return Ok(candidate);
    }
    if let Ok(settings) = load_settings_with_source(cwd, source) {
        if let Some(p) = &settings.plugins {
            if let Some(list) = &p.marketplaces {
                for path in list {
                    let resolved = if path.is_absolute() {
                        path.clone()
                    } else {
                        cwd.join(path)
                    };
                    if let Ok(market) = LocalPluginMarketplace::load(&resolved) {
                        if market.name == name_or_path {
                            return Ok(resolved);
                        }
                    }
                }
            }
        }
    }
    let project_local = cwd.join(name_or_path);
    if project_local.exists() {
        return Ok(project_local);
    }
    Err(anyhow!(
        "marketplace {name_or_path:?} not found (looked up by registered name and path)"
    ))
}

/// Return the effective `strict_component_paths` flag from project +
/// user settings. Defaults to `true` so misconfigured marketplaces are
/// rejected unless the operator explicitly opts in.
#[allow(dead_code)]
fn settings_strict_component_paths(cwd: &Path) -> bool {
    settings_strict_component_paths_with_source(cwd, SettingSource::UserAndProject)
}

fn settings_strict_component_paths_with_source(cwd: &Path, source: SettingSource) -> bool {
    load_settings_with_source(cwd, source)
        .ok()
        .and_then(|s| s.plugins.as_ref().and_then(|p| p.strict_component_paths))
        .unwrap_or(true)
}

async fn cmd_uninstall(args: PluginUninstallArgs) -> Result<()> {
    let scope = parse_scope(&args.scope)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let layout = match scope {
        PluginScope::Project | PluginScope::Managed => PluginInstallLayout::project(&cwd),
        PluginScope::Local => PluginInstallLayout::local(&cwd),
        PluginScope::User => {
            let home =
                user_config_home().ok_or_else(|| anyhow!("user config home not resolvable"))?;
            PluginInstallLayout::user(&home)
        }
    };

    if !args.yes && !args.keep_data && std::io::stdin().is_terminal() {
        eprint!(
            "About to uninstall {} (delete data dir). Continue? [y/N] ",
            args.plugin
        );
        let _ = std::io::stderr().flush();
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf).ok();
        if !matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("aborted");
            return Ok(());
        }
    }

    // Resolve the version from the *requested scope's* settings file
    // first, so that uninstalling from one scope can't accidentally
    // operate on another scope's installation when the same plugin is
    // present in multiple scopes. Fall back to whichever version
    // exists in the cache only if the scope's settings file has no
    // matching entry.
    let settings_path = settings_path_for_scope(&cwd, scope)?;
    // Read+parse settings BEFORE doing any destructive work so a
    // malformed settings.json fails the uninstall instead of
    // half-completing it (Codex review feedback). We then mutate the
    // already-parsed value in place and write it back at the end.
    let mut value = read_settings_json(&settings_path)?;
    let scoped_version = value
        .get("plugins")
        .and_then(|p| p.get("enabled_plugins"))
        .and_then(|l| l.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("name").and_then(|n| n.as_str()) == Some(args.plugin.as_str()))
        })
        .and_then(|e| e.get("version"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let version = scoped_version.or_else(|| pick_installed_version(&layout, &args.plugin));

    uninstall_cache(&layout, &args.plugin, version.as_deref())?;
    if !args.keep_data {
        remove_data_dir(&layout, &args.plugin)?;
    }
    remove_enabled_entry(&mut value, &args.plugin);
    write_settings_json(&settings_path, &value)?;
    println!(
        "[plugin] uninstalled {} from {} scope{}",
        args.plugin,
        scope.as_str(),
        if args.keep_data { " (kept data)" } else { "" }
    );
    Ok(())
}

async fn cmd_set_enabled(args: PluginEnableArgs, enabled: bool) -> Result<()> {
    let scope = parse_scope(&args.scope)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let settings_path = settings_path_for_scope(&cwd, scope)?;
    let mut value = read_settings_json(&settings_path)?;
    // We need an entry to point at. If absent, look for any installed
    // version in the cache.
    let layout = match scope {
        PluginScope::Project | PluginScope::Managed => PluginInstallLayout::project(&cwd),
        PluginScope::Local => PluginInstallLayout::local(&cwd),
        PluginScope::User => {
            let home =
                user_config_home().ok_or_else(|| anyhow!("user config home not resolvable"))?;
            PluginInstallLayout::user(&home)
        }
    };
    let version = pick_installed_version(&layout, &args.plugin);
    upsert_enabled_entry(
        &mut value,
        &EnabledPluginEntry {
            name: args.plugin.clone(),
            version,
            scope: Some(scope.as_str().to_string()),
        },
        enabled,
    )?;
    write_settings_json(&settings_path, &value)?;
    println!(
        "[plugin] {} {}",
        if enabled { "enabled" } else { "disabled" },
        args.plugin
    );
    Ok(())
}

fn pick_installed_version(layout: &PluginInstallLayout, name: &str) -> Option<String> {
    let plugin_dir = layout.cache_root.join(name);
    if !plugin_dir.exists() {
        return None;
    }
    let mut versions: Vec<String> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&plugin_dir) {
        for entry in rd.flatten() {
            if let Some(s) = entry.path().file_name().and_then(|s| s.to_str()) {
                if s != "unversioned" {
                    versions.push(s.to_string());
                }
            }
        }
    }
    versions.sort_by(|a, b| compare_version_strings(a, b));
    versions.into_iter().next_back()
}

async fn cmd_list(args: PluginListArgs) -> Result<()> {
    let source = parse_setting_source(&args.workspace.settings)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let settings = load_settings_with_source(&cwd, source)?;
    let ctx = PluginLoadContext {
        cwd: cwd.clone(),
        user_config_home: user_config_home(),
        dev_plugin_dirs: Vec::new(),
        force_disabled: false,
    };
    let build = build_plugin_runtime(&settings, &ctx);
    if args.json {
        let summaries = build.registry.summaries();
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }
    if build.registry.is_empty() {
        println!("(no plugins)");
    } else {
        for (_, install) in build.registry.iter() {
            print_one_summary(install);
        }
    }
    if args.all {
        for f in &build.validation_failures {
            println!(
                "[error] {}: {}",
                f.plugin.as_deref().unwrap_or("<unknown>"),
                f.reason
            );
        }
    }
    Ok(())
}

fn print_one_summary(install: &PluginInstall) {
    let comp = &install.components;
    println!(
        "{}{} ({}) [{}]",
        install.name,
        install
            .version
            .as_deref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default(),
        install.scope.as_str(),
        match &install.status {
            crom_core::PluginStatus::Loaded => "loaded".to_string(),
            crom_core::PluginStatus::Disabled => "disabled".to_string(),
            crom_core::PluginStatus::Error { reason } => format!("error: {reason}"),
        }
    );
    println!(
        "  skills: {}  commands: {}  agents: {}  hooks: {}  mcp: {}  bin: {}{}",
        comp.skills,
        comp.commands,
        comp.agents,
        comp.hooks,
        comp.mcp_servers,
        comp.bin_entries,
        if comp.settings_defaults {
            "  settings: defaults"
        } else {
            ""
        }
    );
}

async fn cmd_show(args: PluginShowArgs) -> Result<()> {
    let source = parse_setting_source(&args.workspace.settings)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let settings = load_settings_with_source(&cwd, source)?;
    let ctx = PluginLoadContext {
        cwd: cwd.clone(),
        user_config_home: user_config_home(),
        dev_plugin_dirs: Vec::new(),
        force_disabled: false,
    };
    let build = build_plugin_runtime(&settings, &ctx);
    let install = build
        .registry
        .get(&args.plugin)
        .ok_or_else(|| anyhow!("plugin {} not found", args.plugin))?;
    if args.json {
        let summary = install.summary();
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }
    print_one_summary(install);
    println!("  source: {}", redacted_display(&install.root));
    if let Some(desc) = &install.manifest.description {
        println!("  description: {desc}");
    }
    if let Some(license) = &install.manifest.license {
        println!("  license: {license}");
    }
    if !install.manifest.keywords.is_empty() {
        println!("  keywords: {}", install.manifest.keywords.join(", "));
    }
    println!("  manifest path: {}", install.root.display());
    Ok(())
}

async fn cmd_validate(args: PluginValidateArgs) -> Result<()> {
    let source = args.source.canonicalize().context("--source")?;
    let manifest =
        parse_and_validate_source(&source, args.compat_claude_plugin_manifest, args.strict)
            .with_context(|| format!("validate {}", source.display()))?;
    println!(
        "ok {}{} ({} skills, {} commands, {} agents)",
        manifest.name,
        manifest
            .version
            .as_deref()
            .map(|v| format!("@{v}"))
            .unwrap_or_default(),
        manifest.skills.len(),
        manifest.commands.len(),
        manifest.agents.len()
    );
    Ok(())
}

async fn cmd_reload(args: PluginReloadArgs) -> Result<()> {
    let source = parse_setting_source(&args.workspace.settings)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let settings = load_settings_with_source(&cwd, source)?;
    let ctx = PluginLoadContext {
        cwd: cwd.clone(),
        user_config_home: user_config_home(),
        dev_plugin_dirs: Vec::new(),
        force_disabled: false,
    };
    let build = build_plugin_runtime(&settings, &ctx);
    let total_components: usize = build
        .registry
        .iter()
        .map(|(_, p)| p.components.total())
        .sum();
    println!(
        "[plugin] reloaded {} plugin(s), {} component(s)",
        build.registry.len(),
        total_components
    );
    for (_, install) in build.registry.iter() {
        print_one_summary(install);
    }
    Ok(())
}

async fn cmd_init(args: PluginInitArgs) -> Result<()> {
    let plugin_dir = args.cwd.join(&args.name);
    if plugin_dir.exists() {
        return Err(anyhow!(
            "plugin path {} already exists",
            plugin_dir.display()
        ));
    }
    fs::create_dir_all(plugin_dir.join(".agent-plugin"))?;
    let mut manifest = serde_json::json!({
        "name": args.name,
        "version": "0.1.0",
        "description": format!("{} plugin", args.name),
        "license": "MIT",
        "keywords": []
    });
    let template = args.template.as_str();
    let include_skill = matches!(template, "skill" | "full");
    let include_command = matches!(template, "skill" | "full");
    let include_agent = matches!(template, "agent" | "full");
    let include_mcp = matches!(template, "mcp" | "full");
    let include_hooks = matches!(template, "full");
    if include_skill {
        manifest["skills"] = serde_json::Value::String("./skills/".into());
        let skill_dir = plugin_dir
            .join("skills")
            .join(format!("{}-hello", args.name));
        fs::create_dir_all(&skill_dir)?;
        fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {}-hello\ndescription: Example skill body for {}\n---\nHello from {}!\n",
                args.name, args.name, args.name
            ),
        )?;
    }
    if include_command {
        manifest["commands"] = serde_json::Value::String("./commands/".into());
        fs::create_dir_all(plugin_dir.join("commands"))?;
        fs::write(
            plugin_dir.join("commands/hello.md"),
            "---\ndescription: Example slash command\n---\nHello from the example plugin command.\n",
        )?;
    }
    if include_agent {
        manifest["agents"] = serde_json::Value::String("./agents/".into());
        fs::create_dir_all(plugin_dir.join("agents"))?;
        fs::write(
            plugin_dir.join("agents/example.md"),
            "---\nname: example\ndescription: Example subagent\n---\nYou are the example subagent.\n",
        )?;
    }
    if include_mcp {
        manifest["mcpServers"] = serde_json::Value::String("./.mcp.json".into());
        fs::write(
            plugin_dir.join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "example": {"type": "stdio", "command": "echo"}
                }
            })
            .to_string(),
        )?;
    }
    if include_hooks {
        manifest["hooks"] = serde_json::Value::String("./hooks/hooks.json".into());
        fs::create_dir_all(plugin_dir.join("hooks"))?;
        fs::write(
            plugin_dir.join("hooks/hooks.json"),
            serde_json::json!({
                "hooks": {}
            })
            .to_string(),
        )?;
    }
    fs::write(
        plugin_dir.join(".agent-plugin/plugin.json"),
        serde_json::to_string_pretty(&manifest)? + "\n",
    )?;
    fs::write(
        plugin_dir.join("README.md"),
        format!("# {}\n\nA reusable agent plugin.\n", args.name),
    )?;
    println!("[plugin] initialized at {}", plugin_dir.display());
    Ok(())
}

async fn cmd_prune(args: PluginPruneArgs) -> Result<()> {
    let source = parse_setting_source(&args.workspace.settings)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let settings = load_settings_with_source(&cwd, source)?;
    let project = PluginInstallLayout::project(&cwd);
    let local = PluginInstallLayout::local(&cwd);
    let user = user_config_home().map(|h| PluginInstallLayout::user(&h));
    let keep: Vec<(String, Option<String>)> = settings
        .plugins
        .as_ref()
        .and_then(|p| p.enabled_plugins.as_ref())
        .map(|list| {
            list.iter()
                .map(|e| (e.name.clone(), e.version.clone()))
                .collect()
        })
        .unwrap_or_default();
    let mut removed = Vec::new();
    let mut failed: Vec<(String, Option<String>, std::io::Error)> = Vec::new();
    for layout in std::iter::once(&project)
        .chain(std::iter::once(&local))
        .chain(user.as_ref())
    {
        let outcome = prune_orphans(layout, &keep, args.dry_run)?;
        removed.extend(outcome.removed);
        failed.extend(outcome.failed);
    }
    if removed.is_empty() && failed.is_empty() {
        println!("[plugin] no orphans");
    } else {
        for (n, v) in &removed {
            println!(
                "[plugin] {} {}{}",
                if args.dry_run {
                    "would remove"
                } else {
                    "removed"
                },
                n,
                v.as_deref().map(|s| format!("@{s}")).unwrap_or_default()
            );
        }
        for (n, v, e) in &failed {
            eprintln!(
                "[plugin] failed to remove {}{}: {}",
                n,
                v.as_deref().map(|s| format!("@{s}")).unwrap_or_default(),
                e
            );
        }
    }
    if !failed.is_empty() {
        return Err(anyhow!(
            "{} plugin cache entr{} could not be removed",
            failed.len(),
            if failed.len() == 1 { "y" } else { "ies" }
        ));
    }
    Ok(())
}

async fn cmd_marketplace_add(args: MarketplaceAddArgs) -> Result<()> {
    let scope = parse_scope(&args.scope)?;
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let source = args.source.canonicalize().context("--source")?;
    let market = LocalPluginMarketplace::load(&source)?;
    let settings_path = settings_path_for_scope(&cwd, scope)?;
    let mut value = read_settings_json(&settings_path)?;
    let plugins = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json root must be a JSON object"))?
        .entry("plugins".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    let pmap = plugins
        .as_object_mut()
        .ok_or_else(|| anyhow!("settings.json `plugins` must be an object"))?;
    let arr = pmap
        .entry("marketplaces".to_string())
        .or_insert_with(|| serde_json::Value::Array(Vec::new()));
    let arr_mut = arr
        .as_array_mut()
        .ok_or_else(|| anyhow!("settings.json `plugins.marketplaces` must be a list"))?;
    let path_str = source.display().to_string();
    if !arr_mut
        .iter()
        .any(|v| v.as_str() == Some(path_str.as_str()))
    {
        arr_mut.push(serde_json::Value::String(path_str));
    }
    write_settings_json(&settings_path, &value)?;
    println!(
        "[plugin] marketplace added {} ({} entries)",
        market.name,
        market.plugins.len()
    );
    Ok(())
}

async fn cmd_marketplace_list(args: WorkspaceArgs) -> Result<()> {
    let source = parse_setting_source(&args.settings)?;
    let cwd = args.cwd.canonicalize().context("--cwd")?;
    let settings = load_settings_with_source(&cwd, source)?;
    let list = settings
        .plugins
        .as_ref()
        .and_then(|p| p.marketplaces.clone())
        .unwrap_or_default();
    if list.is_empty() {
        println!("(no marketplaces)");
        return Ok(());
    }
    for path in list {
        let resolved = if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(&path)
        };
        match LocalPluginMarketplace::load(&resolved) {
            Ok(m) => println!(
                "{} ({} entries) at {}",
                m.name,
                m.plugins.len(),
                redacted_display(&resolved)
            ),
            Err(e) => println!("[error] {}: {e}", resolved.display()),
        }
    }
    Ok(())
}

async fn cmd_marketplace_show(args: MarketplaceShowArgs) -> Result<()> {
    let cwd = args.workspace.cwd.canonicalize().context("--cwd")?;
    let market_path = resolve_marketplace_path(&cwd, &args.name)?;
    let market = LocalPluginMarketplace::load(&market_path)?;
    println!("{} (file: {})", market.name, redacted_display(&market_path));
    if let Some(o) = &market.owner {
        if let Some(n) = &o.name {
            println!("  owner: {n}");
        }
    }
    for entry in &market.plugins {
        println!(
            "  {}{}: {}",
            entry.name,
            entry
                .version
                .as_deref()
                .map(|v| format!("@{v}"))
                .unwrap_or_default(),
            entry.source.display()
        );
        if let Some(d) = &entry.description {
            println!("    {d}");
        }
    }
    Ok(())
}

/// Produce a [`crom_core::PluginRegistry`] for a normal `run`/`chat` invocation.
/// `dev_plugin_dirs` carries `--plugin-dir` values; `force_disabled`
/// honors `--no-plugins`.
pub fn build_for_invocation(
    cwd: &Path,
    settings: &Settings,
    dev_plugin_dirs: Vec<PathBuf>,
    force_disabled: bool,
) -> crom_core::PluginRuntimeBuild {
    let ctx = PluginLoadContext {
        cwd: cwd.to_path_buf(),
        user_config_home: user_config_home(),
        dev_plugin_dirs,
        force_disabled,
    };
    build_plugin_runtime(settings, &ctx)
}

/// Why a plugin-supplied component could not be merged into the
/// in-memory registries. Returned by [`apply_to_registries`] so callers
/// can choose to log, surface to UI, or abort. `apply_to_registries`
/// always logs to stderr in addition to returning these.
#[derive(Debug)]
pub enum RegistrationWarning {
    Skill(String),
    Agent(String),
    McpServer { name: String, sanitized: String },
}

impl std::fmt::Display for RegistrationWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistrationWarning::Skill(msg) => write!(f, "skipping skill: {msg}"),
            RegistrationWarning::Agent(msg) => write!(f, "skipping agent: {msg}"),
            RegistrationWarning::McpServer { name, sanitized } => write!(
                f,
                "skipping MCP server {name} — sanitized name {sanitized} already in use"
            ),
        }
    }
}

/// Apply a [`crom_core::PluginRuntimeBuild`] to the in-memory registries used by the
/// agent. Returns the merged plugin registry so the caller can attach
/// it to `AgentOptions`, plus a list of [`RegistrationWarning`] entries
/// for each skipped component (duplicate skill/agent names, MCP server
/// name collisions, etc.). Each warning is also printed to stderr so the
/// default behavior matches the previous log-and-continue behavior.
///
/// MCP server collisions: plugin-namespaced names use `plugin.server`
/// shape; MCP itself rejects `.` so the name is sanitized to
/// `plugin_server`. If that sanitized form already exists in the
/// caller's MCP options, the plugin entry is skipped to avoid silent
/// override of a project/user-configured server.
pub fn apply_to_registries(
    build: crom_core::PluginRuntimeBuild,
    skills: &mut crom_core::SkillRegistry,
    slash_defs: &mut Vec<crom_core::CustomSlashCommandDefinition>,
    agents: &mut crom_core::AgentRegistry,
    mcp: &mut crom_mcp::McpOptions,
) -> (crom_core::PluginRegistry, Vec<RegistrationWarning>) {
    let mut warnings: Vec<RegistrationWarning> = Vec::new();
    for skill in build.aggregated.skills {
        if let Err(e) = skills.register(skill) {
            let msg = e.to_string();
            eprintln!("[plugin] skipping skill: {msg}");
            warnings.push(RegistrationWarning::Skill(msg));
        }
    }
    slash_defs.extend(build.aggregated.commands);
    for agent in build.aggregated.agents {
        if let Err(e) = agents.register_filesystem(agent) {
            let msg = e.to_string();
            eprintln!("[plugin] skipping agent: {msg}");
            warnings.push(RegistrationWarning::Agent(msg));
        }
    }
    let mut sanitized_seen: std::collections::BTreeSet<String> =
        mcp.servers.keys().cloned().collect();
    for (name, cfg) in build.aggregated.mcp_servers {
        let sanitized = name.replace('.', "_");
        if sanitized_seen.contains(&sanitized) {
            eprintln!(
                "[plugin] skipping MCP server {name} — sanitized name {sanitized} already in use"
            );
            warnings.push(RegistrationWarning::McpServer {
                name: name.clone(),
                sanitized: sanitized.clone(),
            });
            continue;
        }
        sanitized_seen.insert(sanitized.clone());
        mcp.servers.insert(sanitized, cfg);
    }
    (build.registry, warnings)
}

/// Project the parts of a [`Settings`] back into a [`crom_core::SettingsFile`] so
/// it can be JSON round-tripped (used by the plugin-defaults merge in
/// `resolve_common`). Lossy by design: only the fields plugin defaults
/// can affect are emitted.
pub fn settings_to_file(s: &Settings) -> crom_core::SettingsFile {
    crom_core::SettingsFile {
        model: s.model.clone(),
        permission_mode: s
            .permission_mode
            .as_ref()
            .map(|p| format!("{p:?}").to_ascii_lowercase()),
        allowed_tools: if s.allowed_tools.is_empty() {
            None
        } else {
            Some(s.allowed_tools.clone())
        },
        disallowed_tools: if s.disallowed_tools.is_empty() {
            None
        } else {
            Some(s.disallowed_tools.clone())
        },
        tool_limits: Some(s.tool_limits.clone()),
        hooks: Some(s.hooks.clone()),
        project_instructions_mode: Some(s.project_instructions_mode),
        max_turns: s.max_turns,
        max_tokens: s.max_tokens,
        checkpointing: Some(s.checkpointing.clone()),
        validation: Some(s.validation.clone()),
        mcp: s.mcp.clone(),
        tool_search: Some(s.tool_search.clone()),
        agents: s.agents.clone(),
        skills: s.skills.clone(),
        slash_commands: s.slash_commands.clone(),
        compaction: s.compaction.clone(),
        plugins: s.plugins.clone(),
        plugin_config: if s.plugin_config.is_empty() {
            None
        } else {
            Some(s.plugin_config.clone())
        },
        // Round-trip the effective UI/recovery shape so plugin defaults can
        // merge into them without erasing user customizations. The merge
        // helper only fills *unset* fields, so emitting the current values
        // here means user-set keys win over plugin defaults.
        ui: Some(ui_to_settings(&s.ui)),
        recovery: Some(recovery_to_settings(&s.recovery)),
        auth: Some(s.auth.clone()),
        model_config: Some(s.model_config.clone()),
        planning: s.planning.clone(),
        repair: Some(s.repair.clone()),
        diagnostics: Some(s.diagnostics.clone()),
        completion: Some(s.completion.clone()),
    }
}

fn ui_to_settings(ui: &crom_core::UiConfig) -> crom_core::UiSettingsConfig {
    crom_core::UiSettingsConfig {
        renderer: Some(ui.renderer.as_str().to_string()),
        fallback_renderer: Some(ui.fallback_renderer.as_str().to_string()),
        confirm_exit_while_running: Some(ui.confirm_exit_while_running),
        show_subagent_events: Some(ui.show_subagent_events),
        show_tool_json: Some(ui.show_tool_json),
        status_line: Some(crom_core::StatusLineSettingsConfig {
            enabled: Some(ui.status_line.enabled),
            template: Some(ui.status_line.template.clone()),
            refresh_interval_ms: Some(ui.status_line.refresh_interval_ms),
        }),
        composer: Some(crom_core::ComposerSettingsConfig {
            multiline: Some(ui.composer.multiline),
            submit_key: Some(match ui.composer.submit_key {
                crom_core::SubmitKey::Enter => "enter".to_string(),
                crom_core::SubmitKey::CtrlEnter => "ctrl_enter".to_string(),
            }),
            history_limit: Some(ui.composer.history_limit),
            bracketed_paste: Some(ui.composer.bracketed_paste),
        }),
        notifications: Some(crom_core::NotificationSettingsConfig {
            enabled: Some(ui.notifications.enabled),
            on_approval_required: Some(ui.notifications.on_approval_required),
            on_run_complete: Some(ui.notifications.on_run_complete),
            on_run_failed: Some(ui.notifications.on_run_failed),
            on_validation_failed: Some(ui.notifications.on_validation_failed),
            on_long_command_complete: Some(ui.notifications.on_long_command_complete),
            on_subagent_complete: Some(ui.notifications.on_subagent_complete),
        }),
    }
}

fn recovery_to_settings(r: &crom_core::RecoveryConfig) -> crom_core::RecoverySettingsConfig {
    crom_core::RecoverySettingsConfig {
        enabled: Some(r.enabled),
        stale_lock_after_seconds: Some(r.stale_lock_after_seconds),
        auto_mark_interrupted: Some(r.auto_mark_interrupted),
    }
}

/// Apply a merged-back [`crom_core::SettingsFile`] to a [`Settings`] struct,
/// preserving fields that were already set. Only fields that were
/// previously unset or empty are populated from the file. Used by the
/// plugin-defaults merge — plugin defaults must never override
/// user/project/local/CLI configuration.
pub fn apply_settings_file_overlay(s: &mut Settings, file: crom_core::SettingsFile) {
    if s.model.is_none() {
        s.model = file.model;
    }
    if s.max_turns.is_none() {
        s.max_turns = file.max_turns;
    }
    if s.max_tokens.is_none() {
        s.max_tokens = file.max_tokens;
    }
    if let Some(lim) = file.tool_limits {
        if s.tool_limits.max_read_bytes.is_none() {
            s.tool_limits.max_read_bytes = lim.max_read_bytes;
        }
        if s.tool_limits.max_output_bytes.is_none() {
            s.tool_limits.max_output_bytes = lim.max_output_bytes;
        }
        if s.tool_limits.bash_timeout_seconds.is_none() {
            s.tool_limits.bash_timeout_seconds = lim.bash_timeout_seconds;
        }
        if s.tool_limits.max_glob_results.is_none() {
            s.tool_limits.max_glob_results = lim.max_glob_results;
        }
        if s.tool_limits.max_grep_results.is_none() {
            s.tool_limits.max_grep_results = lim.max_grep_results;
        }
    }
    if let Some(h) = file.hooks {
        if s.hooks.block_env_file_writes.is_none() {
            s.hooks.block_env_file_writes = h.block_env_file_writes;
        }
        if s.hooks.block_dangerous_bash.is_none() {
            s.hooks.block_dangerous_bash = h.block_dangerous_bash;
        }
        if s.hooks.audit_log.is_none() {
            s.hooks.audit_log = h.audit_log;
        }
        if s.hooks.secret_redaction.is_none() {
            s.hooks.secret_redaction = h.secret_redaction;
        }
        if s.hooks.max_patch_size_bytes.is_none() {
            s.hooks.max_patch_size_bytes = h.max_patch_size_bytes;
        }
    }
    if let Some(v) = file.validation {
        if s.validation.commands.is_none() {
            s.validation.commands = v.commands;
        }
        if s.validation.timeout_seconds.is_none() {
            s.validation.timeout_seconds = v.timeout_seconds;
        }
        if s.validation.rewind_on_failure.is_none() {
            s.validation.rewind_on_failure = v.rewind_on_failure;
        }
    }
    // Skip: allowed_tools/disallowed_tools and per-subsystem block
    // settings already accumulate via the user-and-project loader; we
    // don't touch them here so plugin defaults can never widen them.
    //
    // M9: `ui` and `recovery` are also intentionally NOT applied here.
    // `settings_to_file` projects the effective UiConfig/RecoveryConfig
    // back into a SettingsFile shape so the JSON merge can't quietly
    // discard the user's customizations, but we never let plugin
    // defaults override the user's terminal/UI behaviour — letting a
    // plugin flip the renderer to "plain" or change submit_key would be
    // a surprise that's hard to debug. Plugin authors who need UI
    // behaviour should expose it through their own settings keys.
    let _ = file.ui;
    let _ = file.recovery;
}

/// Compare two version strings using a numeric component split. Falls
/// back to lexical compare for non-numeric segments. Used by
/// [`pick_installed_version`] so `0.10.0` is recognized as newer than
/// `0.9.0`.
fn compare_version_strings(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(av), Some(bv)) => {
                let an = av.parse::<u64>().ok();
                let bn = bv.parse::<u64>().ok();
                let ord = match (an, bn) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    _ => av.cmp(bv),
                };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::{
        AgentDefinition, LoadedPluginComponents, PluginRegistry, PluginRuntimeBuild,
        SkillDefinition, SkillRegistry, SkillSource,
    };

    #[test]
    // Numeric version comparison: 0.10.0 > 0.9.0 (lexical comparison would say
    // the opposite).
    fn version_compare_handles_minor_double_digits() {
        assert_eq!(
            compare_version_strings("0.10.0", "0.9.0"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_version_strings("1.0.0", "1.0.0"),
            std::cmp::Ordering::Equal
        );
    }

    fn dummy_skill(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            description: "d".into(),
            instructions: "body".into(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            resources: Vec::new(),
            metadata: serde_json::Value::Null,
            source: SkillSource::Project,
            root_dir: std::path::PathBuf::from("/tmp"),
        }
    }

    #[test]
    // apply_to_registries returns a RegistrationWarning for a plugin-supplied
    // skill whose name collides with one already in the skill registry,
    // instead of silently dropping the duplicate.
    fn apply_to_registries_surfaces_skill_collision() {
        let mut skills = SkillRegistry::new();
        // Seed an existing skill so the plugin's same-named skill collides.
        skills.register(dummy_skill("dup")).unwrap();
        let mut slash_defs: Vec<crom_core::CustomSlashCommandDefinition> = Vec::new();
        let mut agents = crom_core::AgentRegistry::new();
        let mut mcp = crom_mcp::McpOptions::default();

        let mut aggregated = LoadedPluginComponents::default();
        aggregated.skills.push(dummy_skill("dup"));
        let build = PluginRuntimeBuild {
            registry: PluginRegistry::default(),
            aggregated,
            validation_failures: Vec::new(),
        };

        let (_reg, warnings) =
            apply_to_registries(build, &mut skills, &mut slash_defs, &mut agents, &mut mcp);
        assert_eq!(warnings.len(), 1, "expected 1 warning, got {warnings:?}");
        assert!(
            matches!(&warnings[0], RegistrationWarning::Skill(_)),
            "expected a Skill warning, got {:?}",
            warnings[0]
        );
    }

    fn dummy_agent(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: "d".into(),
            prompt: "p".into(),
            ..AgentDefinition::default()
        }
    }

    #[test]
    // apply_to_registries also surfaces agent-name collisions as warnings
    // rather than dropping the duplicate registration error.
    fn apply_to_registries_surfaces_agent_collision() {
        let mut skills = SkillRegistry::new();
        let mut slash_defs: Vec<crom_core::CustomSlashCommandDefinition> = Vec::new();
        let mut agents = crom_core::AgentRegistry::new();
        // Pre-register one filesystem agent; the plugin tries to register
        // a second one with the same name via filesystem origin, which
        // trips the duplicate-name check.
        agents.register_filesystem(dummy_agent("agent-x")).unwrap();
        let mut mcp = crom_mcp::McpOptions::default();

        let mut aggregated = LoadedPluginComponents::default();
        aggregated.agents.push(dummy_agent("agent-x"));
        let build = PluginRuntimeBuild {
            registry: PluginRegistry::default(),
            aggregated,
            validation_failures: Vec::new(),
        };

        let (_reg, warnings) =
            apply_to_registries(build, &mut skills, &mut slash_defs, &mut agents, &mut mcp);
        assert_eq!(warnings.len(), 1, "expected 1 warning, got {warnings:?}");
        assert!(
            matches!(&warnings[0], RegistrationWarning::Agent(_)),
            "expected an Agent warning, got {:?}",
            warnings[0]
        );
    }

    // --- compare_version_strings: numeric-aware ordering -----------------

    #[test]
    // Differing segment counts: a strict prefix is the *smaller* version,
    // and trailing non-numeric (pre-release-ish) segments fall back to
    // lexical compare instead of panicking.
    fn version_compare_segment_counts_and_non_numeric() {
        use std::cmp::Ordering;
        // 1.2 is older than 1.2.0 (fewer segments => Less).
        assert_eq!(compare_version_strings("1.2", "1.2.0"), Ordering::Less);
        assert_eq!(compare_version_strings("1.2.0", "1.2"), Ordering::Greater);
        // Numeric segments dominate lexical: "9" < "10".
        assert_eq!(compare_version_strings("1.9.0", "1.10.0"), Ordering::Less);
        // Non-numeric segment falls back to lexical compare (no parse panic).
        assert_eq!(
            compare_version_strings("1.0.0-alpha", "1.0.0-beta"),
            Ordering::Less
        );
        // A numeric segment is *not* equal to a non-numeric one; the
        // fallback lexical compare decides deterministically.
        assert_eq!(
            compare_version_strings("1.x", "1.0"),
            "x".cmp("0"),
            "non-numeric vs numeric segment must use lexical fallback"
        );
    }

    // --- upsert_enabled_entry: happy + idempotent + unhappy --------------

    fn entry(name: &str) -> EnabledPluginEntry {
        EnabledPluginEntry {
            name: name.to_string(),
            version: Some("1.0.0".to_string()),
            scope: Some("project".to_string()),
        }
    }

    #[test]
    // Enabling records the install in enabled_plugins and keeps the plugin
    // out of disabled_plugins.
    fn upsert_enabled_records_install_and_not_disabled() {
        let mut v = serde_json::json!({});
        upsert_enabled_entry(&mut v, &entry("alpha"), true).unwrap();
        let arr = v["plugins"]["enabled_plugins"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "alpha");
        assert_eq!(arr[0]["version"], "1.0.0");
        // No disabled list created (or it does not contain the plugin).
        assert!(v["plugins"]
            .get("disabled_plugins")
            .and_then(|d| d.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true));
    }

    #[test]
    // Disabling still records the install (so prune keeps the cache) but
    // also adds the plugin to disabled_plugins so the runtime won't load it.
    fn upsert_disabled_records_install_and_marks_disabled() {
        let mut v = serde_json::json!({});
        upsert_enabled_entry(&mut v, &entry("beta"), false).unwrap();
        let enabled = v["plugins"]["enabled_plugins"].as_array().unwrap();
        assert_eq!(
            enabled.len(),
            1,
            "install must be recorded even when disabled"
        );
        let disabled = v["plugins"]["disabled_plugins"].as_array().unwrap();
        assert_eq!(disabled, &[serde_json::Value::String("beta".into())]);
    }

    #[test]
    // Re-upserting the same plugin must not duplicate the enabled entry or
    // the disabled marker; the latest record replaces the prior one.
    fn upsert_is_idempotent_on_repeat() {
        let mut v = serde_json::json!({});
        upsert_enabled_entry(&mut v, &entry("gamma"), false).unwrap();
        upsert_enabled_entry(&mut v, &entry("gamma"), false).unwrap();
        assert_eq!(v["plugins"]["enabled_plugins"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["plugins"]["disabled_plugins"].as_array().unwrap().len(),
            1
        );
    }

    #[test]
    // Enabling a plugin that was previously disabled clears it from
    // disabled_plugins (re-enable path).
    fn upsert_enable_clears_prior_disabled_marker() {
        let mut v = serde_json::json!({});
        upsert_enabled_entry(&mut v, &entry("delta"), false).unwrap();
        upsert_enabled_entry(&mut v, &entry("delta"), true).unwrap();
        let disabled = v["plugins"]["disabled_plugins"].as_array().unwrap();
        assert!(
            !disabled.iter().any(|x| x == "delta"),
            "re-enable must remove the disabled marker, got {disabled:?}"
        );
    }

    #[test]
    // Malformed settings: a non-object root is rejected with a clear error
    // rather than silently producing a corrupt file.
    fn upsert_rejects_non_object_root() {
        let mut v = serde_json::json!([1, 2, 3]);
        let err = upsert_enabled_entry(&mut v, &entry("x"), true).unwrap_err();
        assert!(
            err.to_string().contains("root must be a JSON object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Malformed settings: `plugins` present but not an object is rejected.
    fn upsert_rejects_non_object_plugins() {
        let mut v = serde_json::json!({ "plugins": "not-an-object" });
        let err = upsert_enabled_entry(&mut v, &entry("x"), true).unwrap_err();
        assert!(
            err.to_string().contains("`plugins` must be an object"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Malformed settings: `enabled_plugins` present but not a list is
    // rejected (unexpected-type input).
    fn upsert_rejects_non_list_enabled_plugins() {
        let mut v = serde_json::json!({ "plugins": { "enabled_plugins": {} } });
        let err = upsert_enabled_entry(&mut v, &entry("x"), true).unwrap_err();
        assert!(
            err.to_string()
                .contains("`plugins.enabled_plugins` must be a list"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Malformed settings on the disabled path: `disabled_plugins` present
    // but not a list is rejected when disabling.
    fn upsert_rejects_non_list_disabled_plugins() {
        let mut v = serde_json::json!({ "plugins": { "disabled_plugins": 7 } });
        let err = upsert_enabled_entry(&mut v, &entry("x"), false).unwrap_err();
        assert!(
            err.to_string()
                .contains("`plugins.disabled_plugins` must be a list"),
            "unexpected error: {err}"
        );
    }

    // --- remove_enabled_entry --------------------------------------------

    #[test]
    // Removal drops the plugin from both enabled_plugins and
    // disabled_plugins, and leaves co-resident plugins untouched.
    fn remove_enabled_entry_clears_both_lists() {
        let mut v = serde_json::json!({});
        upsert_enabled_entry(&mut v, &entry("keep"), true).unwrap();
        upsert_enabled_entry(&mut v, &entry("drop"), false).unwrap();
        remove_enabled_entry(&mut v, "drop");
        let enabled = v["plugins"]["enabled_plugins"].as_array().unwrap();
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0]["name"], "keep");
        let disabled = v["plugins"]["disabled_plugins"].as_array().unwrap();
        assert!(!disabled.iter().any(|x| x == "drop"));
    }

    #[test]
    // Removing from a settings value with no `plugins` key is a no-op and
    // must not panic.
    fn remove_enabled_entry_tolerates_missing_plugins() {
        let mut v = serde_json::json!({ "model": "x" });
        remove_enabled_entry(&mut v, "anything");
        assert_eq!(v, serde_json::json!({ "model": "x" }));
    }

    // --- apply_local_settings_overlay: safety semantics ------------------

    fn settings_with_plugins(p: crom_core::PluginsSettingsConfig) -> Settings {
        Settings {
            plugins: Some(p),
            ..Settings::default()
        }
    }

    #[test]
    // settings.local.json must NOT override a safety toggle already set in
    // user/project (only fills unset scalars).
    fn local_overlay_does_not_override_set_safety_toggle() {
        let mut s = settings_with_plugins(crom_core::PluginsSettingsConfig {
            allow_plugin_bin_path: Some(false), // already locked down
            ..Default::default()
        });
        let local = crom_core::SettingsFile {
            plugins: Some(crom_core::PluginsSettingsConfig {
                allow_plugin_bin_path: Some(true), // local tries to widen
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_local_settings_overlay(&mut s, local);
        assert_eq!(
            s.plugins.unwrap().allow_plugin_bin_path,
            Some(false),
            "local overlay must never widen a safety toggle already set"
        );
    }

    #[test]
    // When the target toggle is unset, the local overlay fills it in.
    fn local_overlay_fills_unset_safety_toggle() {
        let mut s = Settings::default();
        let local = crom_core::SettingsFile {
            plugins: Some(crom_core::PluginsSettingsConfig {
                allow_plugin_mcp: Some(true),
                max_plugins: Some(5),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_local_settings_overlay(&mut s, local);
        let p = s.plugins.unwrap();
        assert_eq!(p.allow_plugin_mcp, Some(true));
        assert_eq!(p.max_plugins, Some(5));
    }

    #[test]
    // enabled_plugins is additive: a new plugin is appended, and a local
    // entry that shares a name replaces (upserts) the target's entry rather
    // than duplicating it.
    fn local_overlay_enabled_plugins_upsert_and_append() {
        let mut s = settings_with_plugins(crom_core::PluginsSettingsConfig {
            enabled_plugins: Some(vec![EnabledPluginEntry {
                name: "shared".into(),
                version: Some("1.0.0".into()),
                scope: Some("project".into()),
            }]),
            ..Default::default()
        });
        let local = crom_core::SettingsFile {
            plugins: Some(crom_core::PluginsSettingsConfig {
                enabled_plugins: Some(vec![
                    EnabledPluginEntry {
                        name: "shared".into(),
                        version: Some("2.0.0".into()), // newer; should replace
                        scope: Some("local".into()),
                    },
                    EnabledPluginEntry {
                        name: "fresh".into(),
                        version: Some("0.1.0".into()),
                        scope: Some("local".into()),
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_local_settings_overlay(&mut s, local);
        let list = s.plugins.unwrap().enabled_plugins.unwrap();
        assert_eq!(list.len(), 2, "shared must upsert, fresh must append");
        let shared = list.iter().find(|e| e.name == "shared").unwrap();
        assert_eq!(shared.version.as_deref(), Some("2.0.0"));
        assert!(list.iter().any(|e| e.name == "fresh"));
    }

    #[test]
    // disabled_plugins and marketplaces concatenate without introducing
    // duplicates.
    fn local_overlay_lists_dedup() {
        let mut s = settings_with_plugins(crom_core::PluginsSettingsConfig {
            disabled_plugins: Some(vec!["a".into()]),
            marketplaces: Some(vec![PathBuf::from("/m/one.json")]),
            ..Default::default()
        });
        let local = crom_core::SettingsFile {
            plugins: Some(crom_core::PluginsSettingsConfig {
                disabled_plugins: Some(vec!["a".into(), "b".into()]),
                marketplaces: Some(vec![
                    PathBuf::from("/m/one.json"),
                    PathBuf::from("/m/two.json"),
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_local_settings_overlay(&mut s, local);
        let p = s.plugins.unwrap();
        assert_eq!(p.disabled_plugins.unwrap(), vec!["a", "b"]);
        assert_eq!(
            p.marketplaces.unwrap(),
            vec![PathBuf::from("/m/one.json"), PathBuf::from("/m/two.json")]
        );
    }

    #[test]
    // plugin_config keys from local overlay are merged into the target map.
    fn local_overlay_merges_plugin_config() {
        let mut s = Settings::default();
        s.plugin_config.insert("kept".into(), serde_json::json!(1));
        let mut pc = std::collections::BTreeMap::new();
        pc.insert("added".to_string(), serde_json::json!("v"));
        let local = crom_core::SettingsFile {
            plugin_config: Some(pc),
            ..Default::default()
        };
        apply_local_settings_overlay(&mut s, local);
        assert_eq!(s.plugin_config.get("kept"), Some(&serde_json::json!(1)));
        assert_eq!(s.plugin_config.get("added"), Some(&serde_json::json!("v")));
    }

    // --- read_settings_json: parsing happy/unhappy (file-backed) ---------

    #[test]
    // A missing settings file resolves to an empty object rather than an
    // error, so first-time installs don't blow up.
    fn read_settings_missing_file_is_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.json");
        let v = read_settings_json(&path).unwrap();
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    // An empty (zero-byte) settings file is treated as an empty object.
    fn read_settings_empty_file_is_empty_object() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"").unwrap();
        let v = read_settings_json(&path).unwrap();
        assert_eq!(v, serde_json::json!({}));
    }

    #[test]
    // Malformed JSON is rejected with a context-bearing parse error, and
    // the error references the offending path.
    fn read_settings_malformed_json_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{ not valid json ").unwrap();
        let err = read_settings_json(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("parse") && msg.contains("settings.json"),
            "expected a parse error naming the file, got: {msg}"
        );
    }

    // --- user_config_home: env precedence (nextest-safe scoping) ---------

    #[test]
    // CROM_CONFIG_HOME takes precedence over XDG_CONFIG_HOME and HOME.
    // Uses temp_env scoped mutation (no raw std::env::set_var) so the test
    // is safe under nextest's process-per-test isolation.
    fn user_config_home_prefers_explicit_override() {
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", Some("/explicit/cfg")),
                ("XDG_CONFIG_HOME", Some("/xdg")),
                ("HOME", Some("/home/u")),
            ],
            || {
                assert_eq!(user_config_home(), Some(PathBuf::from("/explicit/cfg")));
            },
        );
    }

    #[test]
    // With no explicit override, XDG_CONFIG_HOME is used and suffixed with
    // the app subdir.
    fn user_config_home_uses_xdg_then_home() {
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", None::<&str>),
                ("XDG_CONFIG_HOME", Some("/xdg")),
                ("HOME", Some("/home/u")),
            ],
            || {
                assert_eq!(user_config_home(), Some(PathBuf::from("/xdg/crom")));
            },
        );
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
                ("HOME", Some("/home/u")),
            ],
            || {
                assert_eq!(
                    user_config_home(),
                    Some(PathBuf::from("/home/u/.config/crom"))
                );
            },
        );
    }

    #[test]
    // With none of the env vars set, there is no resolvable config home.
    fn user_config_home_none_when_env_absent() {
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
                ("HOME", None::<&str>),
            ],
            || {
                assert_eq!(user_config_home(), None);
            },
        );
    }

    // --- settings_path_for_scope: scope -> path mapping ------------------

    #[test]
    // Project/Managed scopes write to .agent/settings.json; Local writes to
    // settings.local.json; all are confined under the workspace dir.
    fn settings_path_for_scope_maps_project_and_local() {
        let cwd = Path::new("/ws");
        let proj = settings_path_for_scope(cwd, PluginScope::Project).unwrap();
        assert_eq!(proj, PathBuf::from("/ws/.agent/settings.json"));
        let managed = settings_path_for_scope(cwd, PluginScope::Managed).unwrap();
        assert_eq!(managed, PathBuf::from("/ws/.agent/settings.json"));
        let local = settings_path_for_scope(cwd, PluginScope::Local).unwrap();
        assert_eq!(local, PathBuf::from("/ws/.agent/settings.local.json"));
        // The resolved path stays inside the workspace (no traversal).
        assert!(proj.starts_with(cwd));
        assert!(local.starts_with(cwd));
    }

    #[test]
    // User scope requires a resolvable config home; with the env cleared it
    // errors instead of silently picking a path outside the user's control.
    fn settings_path_for_scope_user_requires_config_home() {
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", None::<&str>),
                ("XDG_CONFIG_HOME", None::<&str>),
                ("HOME", None::<&str>),
            ],
            || {
                let err = settings_path_for_scope(Path::new("/ws"), PluginScope::User).unwrap_err();
                assert!(
                    err.to_string().contains("user-config home"),
                    "unexpected error: {err}"
                );
            },
        );
    }

    // --- parse_scope: lenient parse + rejection --------------------------

    #[test]
    // Known scope names parse case-insensitively; an unknown token is
    // rejected with the offending value echoed back.
    fn parse_scope_accepts_known_rejects_unknown() {
        assert!(matches!(
            parse_scope("Project").unwrap(),
            PluginScope::Project
        ));
        assert!(matches!(parse_scope("local").unwrap(), PluginScope::Local));
        let err = parse_scope("bogus").unwrap_err();
        assert!(err.to_string().contains("bogus"), "got: {err}");
    }
}
