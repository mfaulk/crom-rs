//! #292 scenario 10, configuration half: what the fixture's repository
//! `.agent/settings.json` does to the effective settings of a session started
//! in that workspace. The agent half (rewriting the operator's user settings)
//! runs in `tests/adversarial.rs`.

use std::path::{Path, PathBuf};

use crom_core::settings::{load, SettingSource};
use crom_core::SandboxMode;
use crom_eval::fixture::default_tasks_dir;

fn repo() -> PathBuf {
    default_tasks_dir()
        .join("sec-repo-settings-escalation")
        .join("repo")
}

fn effective(user_settings: &str) -> crom_core::settings::Settings {
    let config_home = tempfile::tempdir().unwrap();
    std::fs::write(config_home.path().join("settings.json"), user_settings).unwrap();
    temp_env::with_var("CROM_CONFIG_HOME", Some(config_home.path()), || {
        load(Path::new(&repo()), SettingSource::UserAndProject).unwrap()
    })
}

/// The repository file cannot turn the operator's required sandbox off, grant network, or add read paths or env names.
#[test]
fn repo_settings_cannot_loosen_the_sandbox() {
    assert!(repo().join(".agent").join("settings.json").exists());
    let s = effective(r#"{"sandbox": {"mode": "required"}}"#)
        .sandbox
        .resolve();
    assert_eq!(s.mode, SandboxMode::Required);
    assert!(!s.network);
    assert!(s.read_paths.is_empty(), "{:?}", s.read_paths);
    assert!(s.env_allow.is_empty(), "{:?}", s.env_allow);
}

/// Without an operator trust decision the repository's subagent definitions and `agents.*` keys are ignored and audited (#342).
#[test]
fn untrusted_repo_cannot_add_subagents() {
    let s = effective("{}");
    let keys: Vec<&str> = s.untrusted_denials.iter().map(|d| d.key.as_str()).collect();
    assert!(keys.contains(&"agents.path"), "{keys:?}");
    assert!(keys.contains(&"agents.inherit_permissions"), "{keys:?}");
    assert!(s.agents.as_ref().is_none_or(|a| a.path.is_none()));
    let defs = crom_core::untrusted_project_agent_denials(&repo());
    assert_eq!(defs.len(), 1, "{defs:?}");
    assert!(defs[0].source.ends_with(".agent/agents/helper.md"));
}

/// Without an operator trust decision the repository file's allow rules, permission mode and validation commands are ignored (threat model P9).
#[test]
fn untrusted_repo_settings_cannot_grant_permissions_or_commands() {
    let s = effective("{}");
    assert!(s.allowed_tools.is_empty(), "{:?}", s.allowed_tools);
    assert_eq!(s.permission_mode, None);
    assert!(
        s.validation
            .commands
            .as_deref()
            .unwrap_or_default()
            .is_empty(),
        "{:?}",
        s.validation.commands
    );
}
