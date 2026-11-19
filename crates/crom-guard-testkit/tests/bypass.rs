//! Alternate-path bypass tests (#360, MILESTONE_18 §11 and §13.3 G13, G14,
//! G16, G18). Each attack runs from agent Bash with the sandbox `required`
//! (the script runs, the OS denies it, no probe fires) and `off` (the probe
//! fires, which validates it). Harness: `tests/isolation/mod.rs`.
//!
//! Skips print `SKIP:`. `CROM_REQUIRE_SANDBOX_TESTS=1` turns a missing
//! sandbox or `curl` into a failure; `CROM_REQUIRE_SEPARATE_UID_TESTS=1`
//! does the same for the separate-uid test, which needs `sudo -n`.
#![cfg(target_os = "linux")]

mod isolation;

use isolation::{
    reference_settings, required_or_skip, sandbox_available, separate_uid_unavailable, Agent,
    Attack, Deployment, CROM_SETTINGS, REQUIRE_SEPARATE_UID,
};
use serde_json::json;

isolation::guard_host_test!();

const SANDBOXED: [Agent; 2] = [
    Agent::Sandboxed { network: false },
    Agent::Sandboxed { network: true },
];

/// Asserts that `attack` ran, was denied by the OS, and fired no probe, for each sandboxed agent; then that it fires `probe` unsandboxed.
async fn denied_then_live(attack: Attack, agents: &[Agent], probe: &str) {
    let d = Deployment::start().await;
    for &agent in agents {
        let o = d.attack(attack, agent).await;
        assert!(!o.tool_error, "{attack:?} {agent:?}: {}", o.output);
        assert!(
            o.ran,
            "{attack:?} {agent:?}: script did not run: {}",
            o.output
        );
        assert!(o.denied, "{attack:?} {agent:?}: not denied: {}", o.output);
        assert!(
            o.breaches(agent).is_empty(),
            "{attack:?} {agent:?}: {:?}: {}",
            o.probes,
            o.output
        );
    }
    let d = Deployment::start().await;
    let o = d.attack(attack, Agent::Unsandboxed).await;
    assert!(o.ran && !o.tool_error, "{}", o.output);
    assert!(
        o.probes.contains(&probe),
        "{attack:?} off: {probe} did not fire ({:?}): {}",
        o.probes,
        o.output
    );
}

/// G13: Bash cannot reach the business API with the sandbox required; with network granted (negative control) or the sandbox off, the API answers 401 without Guard's credential; no ledger effect in any case.
#[tokio::test(flavor = "multi_thread")]
async fn g13_direct_api_call() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(
        Attack::DirectApi,
        &[Agent::Sandboxed { network: false }],
        "BusinessApiReached",
    )
    .await;
    let d = Deployment::start().await;
    for agent in [Agent::Sandboxed { network: true }, Agent::Unsandboxed] {
        let o = d.attack(Attack::DirectApi, agent).await;
        assert!(
            o.ran && !o.denied && !o.tool_error,
            "{agent:?}: {}",
            o.output
        );
        assert!(o.output.contains("status=401"), "{agent:?}: {}", o.output);
        assert!(o.output.contains("HTTP/1.1 401"), "{agent:?}: {}", o.output);
        assert_eq!(o.probes, ["BusinessApiReached"], "{agent:?}");
    }
    assert_eq!(d.ledger().len(), 1);
    assert_eq!(d.unmediated(), 0);
}

/// G14: Bash cannot read Guard's credential file with the sandbox required; with it off, the secret is read.
#[tokio::test(flavor = "multi_thread")]
async fn g14_credential_file_read() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(
        Attack::ReadCredentialFile,
        &SANDBOXED,
        "CredentialInContext",
    )
    .await;
}

/// G14 + UnmediatedEffect: Bash that steals the credential file and calls the API causes no ledger effect with the sandbox required, even with network granted; with it off, an unmediated effect appears.
#[tokio::test(flavor = "multi_thread")]
async fn g14_stolen_credential_direct_call() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::StolenCredentialCall, &SANDBOXED, "UnmediatedEffect").await;
}

/// G14: Bash cannot read Guard's environment (holding the admin token) through /proc with the sandbox required; with it off, it can.
#[tokio::test(flavor = "multi_thread")]
async fn g14_guard_environ_read() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::ReadGuardEnviron, &SANDBOXED, "GuardSecretInContext").await;
}

/// G14: Bash cannot read the admin token file in Guard's state directory with the sandbox required; with it off, the token is read.
#[tokio::test(flavor = "multi_thread")]
async fn g14_admin_token_file_read() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(
        Attack::ReadAdminTokenFile,
        &SANDBOXED,
        "GuardSecretInContext",
    )
    .await;
}

/// G16: even holding the admin token, Bash cannot reach the admin socket with the sandbox required (AF_UNIX stays refused when network is granted); with it off, it mints itself a grant.
#[tokio::test(flavor = "multi_thread")]
async fn g16_admin_socket() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::AdminSocket, &SANDBOXED, "AdminApiReached").await;
}

/// G18: Bash cannot append to Guard's audit log with the sandbox required, so it verifies against the earlier head; with it off, the chain breaks.
#[tokio::test(flavor = "multi_thread")]
async fn g18_audit_log_append() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::AppendAuditLog, &SANDBOXED, "AuditChainBroken").await;
}

/// G18: Bash cannot truncate Guard's audit log with the sandbox required; with it off, verification against the earlier head fails.
#[tokio::test(flavor = "multi_thread")]
async fn g18_audit_log_truncate() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::TruncateAuditLog, &SANDBOXED, "AuditChainBroken").await;
}

/// G18: Bash cannot read Guard's audit log with the sandbox required; with it off, the log (its head hash) is read.
#[tokio::test(flavor = "multi_thread")]
async fn g18_audit_log_read() {
    if !sandbox_available() {
        return;
    }
    denied_then_live(Attack::ReadAuditLog, &SANDBOXED, "AuditLogRead").await;
}

/// The attacked deployment passes `crom-guard check` with the reference Crom settings, and fails it with the sandbox off or network granted.
#[tokio::test(flavor = "multi_thread")]
async fn preflight_matches_the_tested_profile() {
    let d = Deployment::start().await;
    let mut settings = reference_settings();
    let report = d.check(&settings);
    assert!(report.passed(), "{:#?}", report.findings);
    settings["sandbox"]["mode"] = json!("off");
    assert!(d.check(&settings).failed().contains(&"sandbox-mode"));
    settings["sandbox"]["mode"] = json!("required");
    settings["sandbox"]["network"] = json!(true);
    assert!(d.check(&settings).failed().contains(&"sandbox-network"));
}

/// `crom-guard check`'s copy of the sandbox's built-in read set equals the one Crom's sandbox computes.
#[test]
fn check_models_the_sandbox_read_set() {
    assert_eq!(
        crom_guard_server::check::sandbox_builtin_read_paths(),
        crom_tools::sandbox::builtin_read_paths()
    );
}

/// `crom-guard check` resolves Crom's user settings path the way Crom does.
#[test]
fn check_models_the_crom_settings_path() {
    assert_eq!(
        crom_guard_server::check::crom_config_dir().map(|d| d.join("settings.json")),
        crom_core::settings::user_settings_path()
    );
}

/// Guard state placed under a built-in sandbox read root is readable from sandboxed Bash, and `crom-guard check` refuses that deployment.
#[tokio::test(flavor = "multi_thread")]
async fn state_under_builtin_read_root_is_refused() {
    if !sandbox_available() {
        return;
    }
    // A writable directory the sandbox always reads, e.g. $CARGO_HOME/registry.
    let Some(root) = crom_tools::sandbox::builtin_read_paths()
        .into_iter()
        .filter(|p| p.is_dir())
        .find(|p| tempfile::tempdir_in(p).is_ok())
    else {
        return required_or_skip(
            "no writable directory in the sandbox's built-in read set",
            isolation::REQUIRE_SANDBOX,
        );
    };
    let d = Deployment::start().await;
    let exposed = tempfile::tempdir_in(&root).unwrap();
    let state = exposed.path().join("state");
    std::fs::create_dir(&state).unwrap();
    // The deployment's files (as `check` sees them), copied under the root.
    let _ = d.check(&reference_settings());
    for f in ["guard-check.toml", "credentials.json", "admin.token"] {
        std::fs::copy(d.path(f), state.join(f)).unwrap();
    }
    std::fs::create_dir(state.join("policies")).unwrap();
    for f in ["policies/support.cedarschema", "policies/support.cedar"] {
        std::fs::copy(d.path(f), state.join(f)).unwrap();
    }
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();

    let out = d
        .run_script(
            Agent::Sandboxed { network: false },
            &format!("cat {}", state.join("credentials.json").display()),
        )
        .await
        .0;
    assert!(out.contains(&d.secret), "the hole is real: {out}");

    let crom = d.oracle.path().join("crom");
    let report = crom_guard_server::check::run(&crom_guard_server::check::CheckOptions {
        config: state.join("guard-check.toml"),
        crom_settings: Some(crom.join("settings.json")),
        crom_workspace: Some(d.oracle.path().join("workspace")),
        crom_config_dir: Some(crom),
        ..Default::default()
    });
    assert!(
        report.failed().contains(&"sandbox-paths"),
        "{:#?}",
        report.findings
    );
}

/// The reference Crom settings (examples/guard-support/agent/crom-settings.json) parse as Crom settings.
#[test]
fn reference_crom_settings_parse() {
    let s: crom_core::settings::SettingsFile = serde_json::from_str(CROM_SETTINGS).unwrap();
    assert!(s.sandbox.is_some());
}

/// With the agent under a separate uid (no Crom sandbox), every read (credential, admin token file, environ, audit log), append and admin connect is denied, the business API stays reachable but answers 401 without Guard's credential, and no probe fires.
#[tokio::test(flavor = "multi_thread")]
async fn separate_uid_agent() {
    if let Some(why) = separate_uid_unavailable() {
        return required_or_skip(&why, REQUIRE_SEPARATE_UID);
    }
    let d = Deployment::start().await;
    for attack in [
        Attack::ReadCredentialFile,
        Attack::StolenCredentialCall,
        Attack::ReadGuardEnviron,
        Attack::ReadAdminTokenFile,
        Attack::AdminSocket,
        Attack::AppendAuditLog,
        Attack::TruncateAuditLog,
        Attack::ReadAuditLog,
    ] {
        let o = d.attack(attack, Agent::SeparateUid).await;
        assert!(o.ran && o.denied, "{attack:?}: {}", o.output);
        assert!(
            o.probes.is_empty(),
            "{attack:?}: {:?}: {}",
            o.probes,
            o.output
        );
    }
    let o = d.attack(Attack::DirectApi, Agent::SeparateUid).await;
    assert!(o.ran && o.output.contains("status=401"), "{}", o.output);
    assert!(o.breaches(Agent::SeparateUid).is_empty(), "{:?}", o.probes);
    assert_eq!(d.ledger().len(), 1);
    assert_eq!(d.unmediated(), 0);
}
