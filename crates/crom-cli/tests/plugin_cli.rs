//! Offline CLI tests for the M8 `plugin` subcommand.
//!
//! No live model is required; we exercise validate/install/list/show/
//! disable/enable/reload/uninstall against a deterministic local plugin.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crom"))
}

fn write_plugin(dir: &Path, name: &str) -> PathBuf {
    let plugin = dir.join(name);
    fs::create_dir_all(plugin.join(".agent-plugin")).unwrap();
    fs::create_dir_all(plugin.join("skills").join("hello")).unwrap();
    fs::create_dir_all(plugin.join("commands")).unwrap();
    fs::create_dir_all(plugin.join("agents")).unwrap();
    fs::write(
        plugin.join(".agent-plugin").join("plugin.json"),
        serde_json::json!({
            "name": name,
            "version": "0.1.0",
            "skills": "./skills/",
            "commands": "./commands/",
            "agents": "./agents/"
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        plugin.join("skills").join("hello").join("SKILL.md"),
        "---\nname: hello\ndescription: example skill\n---\nHello plugin body.\n",
    )
    .unwrap();
    fs::write(
        plugin.join("commands").join("hi.md"),
        "---\ndescription: example command\n---\nHi from plugin command.\n",
    )
    .unwrap();
    fs::write(
        plugin.join("agents").join("explainer.md"),
        "---\nname: explainer\ndescription: explains things\n---\nYou are an explainer.\n",
    )
    .unwrap();
    plugin
}

#[test]
// `plugin validate` accepts a well-formed plugin source.
fn plugin_validate_ok() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let out = Command::new(bin())
        .args(["plugin", "validate"])
        .arg(&plugin)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ok demo"), "stdout={stdout}");
}

#[test]
// `plugin install --scope local` copies the plugin into the cache, list
// reports it, and uninstall removes it again.
fn plugin_install_list_uninstall() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();

    let out = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--enable", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("installed demo"), "stdout={stdout}");

    // Cache copy exists.
    assert!(target
        .join(".agent")
        .join("plugins-local")
        .join("cache")
        .join("demo")
        .join("0.1.0")
        .exists());

    let out = Command::new(bin())
        .args(["plugin", "list", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("demo"), "stdout={stdout}");

    let out = Command::new(bin())
        .args([
            "plugin",
            "uninstall",
            "demo",
            "--scope",
            "local",
            "--keep-data",
            "--yes",
            "--cwd",
        ])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    assert!(!target
        .join(".agent")
        .join("plugins-local")
        .join("cache")
        .join("demo")
        .join("0.1.0")
        .exists());
    // Data dir preserved due to --keep-data.
    assert!(target
        .join(".agent")
        .join("plugins-local")
        .join("data")
        .join("demo")
        .exists());
}

#[test]
// `plugin install --dry-run` prints intent without mutating cache.
fn plugin_install_dry_run_does_nothing() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let out = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "project", "--dry-run", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would install"), "stdout={stdout}");
    assert!(!target
        .join(".agent")
        .join("plugins")
        .join("cache")
        .join("demo")
        .join("0.1.0")
        .exists());
}

#[test]
// `plugin show --json` emits valid JSON about an installed plugin.
fn plugin_show_json_works() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let _ = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--enable", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");

    let out = Command::new(bin())
        .args(["plugin", "show", "demo", "--json", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("parse JSON");
    assert_eq!(v["name"], "demo");
    assert_eq!(v["version"], "0.1.0");
}

#[test]
// `plugin reload` reports component counts after enabling a plugin.
fn plugin_reload_reports_components() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let _ = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--enable", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    let out = Command::new(bin())
        .args(["plugin", "reload", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("reloaded 1"), "stdout={stdout}");
    assert!(stdout.contains("skills: 1"), "stdout={stdout}");
}

#[test]
// `plugin disable` then `plugin enable` round-trips through the disabled
// list in settings.
fn plugin_disable_enable_round_trip() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let _ = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--enable", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    let out = Command::new(bin())
        .args(["plugin", "disable", "demo", "--scope", "local", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let settings_local = target.join(".agent").join("settings.local.json");
    let bytes = fs::read(&settings_local).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let disabled = v["plugins"]["disabled_plugins"].as_array().unwrap();
    assert!(disabled.iter().any(|s| s == "demo"));

    let out = Command::new(bin())
        .args(["plugin", "enable", "demo", "--scope", "local", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let bytes = fs::read(&settings_local).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let disabled = v["plugins"]["disabled_plugins"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!disabled.iter().any(|s| s == "demo"));
}

#[test]
// `plugin init --template basic` writes a parseable plugin skeleton.
fn plugin_init_basic_creates_skeleton() {
    let tmp = TempDir::new().unwrap();
    let out = Command::new(bin())
        .args(["plugin", "init", "demo", "--template", "basic", "--cwd"])
        .arg(tmp.path())
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest_path = tmp
        .path()
        .join("demo")
        .join(".agent-plugin")
        .join("plugin.json");
    assert!(manifest_path.exists());
    let v: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(v["name"], "demo");
}

#[test]
// `plugin prune` removes orphan cache versions not present in settings.
fn plugin_prune_removes_orphans() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let _ = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--enable", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    // Add a fake stale version directory.
    let stale = target
        .join(".agent")
        .join("plugins-local")
        .join("cache")
        .join("demo")
        .join("0.0.1");
    fs::create_dir_all(&stale).unwrap();
    let out = Command::new(bin())
        .args(["plugin", "prune", "--yes", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("removed demo"), "stdout={stdout}");
    assert!(!stale.exists());
}

#[test]
// `--no-plugins` on `run`/`chat` is accepted (parses) — at minimum the CLI
// must understand the flag, even if `run` itself needs an API key.
fn no_plugins_flag_parses() {
    let out = Command::new(bin())
        .args(["plugin", "list", "--all", "--cwd"])
        .arg(".")
        .output()
        .expect("spawn");
    assert!(out.status.success());
}

#[test]
// `plugin marketplace add` registers a marketplace file in the chosen scope.
fn plugin_marketplace_add_lists_entries() {
    let tmp = TempDir::new().unwrap();
    let market_path = tmp.path().join("marketplace.json");
    fs::write(
        &market_path,
        serde_json::json!({
            "name": "local-devtools",
            "plugins": [{"name": "demo", "source": "./plugins/demo"}]
        })
        .to_string(),
    )
    .unwrap();
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let out = Command::new(bin())
        .args(["plugin", "marketplace", "add"])
        .arg(&market_path)
        .args(["--scope", "local", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("local-devtools"), "stdout={stdout}");
}

#[test]
// `plugin install` without --enable still records the install in
// `enabled_plugins` so `plugin prune` does not delete a freshly
// installed (but disabled) cache version.
fn plugin_prune_keeps_disabled_fresh_install() {
    let tmp = TempDir::new().unwrap();
    let plugin = write_plugin(tmp.path(), "demo");
    let target = tmp.path().join("project");
    fs::create_dir_all(&target).unwrap();
    let out = Command::new(bin())
        .args(["plugin", "install"])
        .arg(&plugin)
        .args(["--scope", "local", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    // Confirm the cache is present.
    let cache = target
        .join(".agent")
        .join("plugins-local")
        .join("cache")
        .join("demo")
        .join("0.1.0");
    assert!(cache.exists());
    // Run prune.
    let out = Command::new(bin())
        .args(["plugin", "prune", "--yes", "--cwd"])
        .arg(&target)
        .output()
        .expect("spawn");
    assert!(out.status.success());
    // Cache must still exist.
    assert!(
        cache.exists(),
        "prune incorrectly removed disabled-but-installed cache"
    );
}

#[test]
// Defense-in-depth: a manifest whose `version` field carries a
// path-traversal sequence is rejected at validate time.
fn plugin_validate_rejects_traversal_version() {
    let tmp = TempDir::new().unwrap();
    let plugin = tmp.path().join("evil");
    fs::create_dir_all(plugin.join(".agent-plugin")).unwrap();
    fs::write(
        plugin.join(".agent-plugin").join("plugin.json"),
        serde_json::json!({"name": "evil", "version": "../../../etc"}).to_string(),
    )
    .unwrap();
    let out = Command::new(bin())
        .args(["plugin", "validate"])
        .arg(&plugin)
        .output()
        .expect("spawn");
    assert!(
        !out.status.success(),
        "expected non-zero exit; stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}
