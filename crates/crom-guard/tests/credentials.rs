#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crom_guard::{
    ActionId, BrokerError, CredentialBroker, CredentialLease, CredentialLoadError, EnvCredential,
    EnvCredentialBroker, FileCredentialBroker,
};
use futures::executor::block_on;

const SECRET: &str = "sk-guard-CANARY-7f3a9c";
const ROTATED: &str = "sk-guard-CANARY-rotated-51be";

fn action(s: &str) -> ActionId {
    s.parse().unwrap()
}

fn creds_json(secret: &str) -> String {
    serde_json::json!({"credentials": [
        {"id": "business-api", "secret": secret, "actions": ["customer.read", "refund.issue"]},
        {"id": "export-api", "secret": "sk-other", "actions": ["customer.export"]}
    ]})
    .to_string()
}

fn write(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn file_with(text: &str, mode: u32) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    write(&path, text, mode);
    (dir, path)
}

fn secret_of(lease: &CredentialLease) -> String {
    lease.with_secret(str::to_owned)
}

/// A 0600 file owned by Guard's uid loads, and each action leases its own credential.
#[test]
fn loads_and_leases_by_action() {
    let (_d, path) = file_with(&creds_json(SECRET), 0o600);
    let b = FileCredentialBroker::open(&path).unwrap();
    let l = block_on(b.lease(&action("refund.issue"))).unwrap();
    assert_eq!(l.credential_id(), "business-api");
    assert_eq!(secret_of(&l), SECRET);
    let e = block_on(b.lease(&action("customer.export"))).unwrap();
    assert_eq!(e.credential_id(), "export-api");
    assert_eq!(b.credential_ids(), ["business-api", "export-api"]);
    assert_eq!(
        block_on(b.lease(&action("invoice.list"))).err(),
        Some(BrokerError::NotConfigured(action("invoice.list")))
    );
}

/// Files readable or writable by group or others are refused.
#[test]
fn refuses_group_or_world_accessible_files() {
    for mode in [0o640, 0o604, 0o660, 0o644, 0o620, 0o602] {
        let (_d, path) = file_with(&creds_json(SECRET), mode);
        assert!(
            matches!(
                FileCredentialBroker::open(&path),
                Err(CredentialLoadError::Permissions { .. })
            ),
            "mode {mode:o}"
        );
    }
    let (_d, path) = file_with(&creds_json(SECRET), 0o400);
    assert!(FileCredentialBroker::open(&path).is_ok());
}

/// A symlink to a valid 0600 file, and a directory, are refused as non-regular files.
#[test]
fn refuses_symlinks_and_directories() {
    let (dir, target) = file_with(&creds_json(SECRET), 0o600);
    let link = dir.path().join("link.json");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(matches!(
        FileCredentialBroker::open(&link),
        Err(CredentialLoadError::NotRegularFile { .. })
    ));
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        FileCredentialBroker::open(&sub),
        Err(CredentialLoadError::NotRegularFile { .. })
    ));
}

/// Duplicate ids, an action mapped twice, empty secrets, unknown fields, and oversize files are rejected.
#[test]
fn invalid_configurations_rejected() {
    let cases = [
        serde_json::json!({"credentials": [
            {"id": "a", "secret": "s1", "actions": ["refund.issue"]},
            {"id": "a", "secret": "s2", "actions": ["customer.read"]}]}),
        serde_json::json!({"credentials": [
            {"id": "a", "secret": "s1", "actions": ["refund.issue"]},
            {"id": "b", "secret": "s2", "actions": ["refund.issue"]}]}),
        serde_json::json!({"credentials": [{"id": "a", "secret": "", "actions": ["refund.issue"]}]}),
        serde_json::json!({"credentials": [{"id": "a", "secret": "s", "actions": []}]}),
        serde_json::json!({"credentials": [{"id": "a b", "secret": "s", "actions": ["refund.issue"]}]}),
        serde_json::json!({"credentials": [{"id": "a", "secret": "s", "actions": ["refund.issue"], "extra": 1}]}),
    ];
    for c in cases {
        let (_d, path) = file_with(&c.to_string(), 0o600);
        assert!(FileCredentialBroker::open(&path).is_err(), "{c}");
    }
    let big = format!(
        "{{\"credentials\": [], \"pad\": \"{}\"}}",
        "x".repeat(70 * 1024)
    );
    let (_d, path) = file_with(&big, 0o600);
    assert!(matches!(
        FileCredentialBroker::open(&path),
        Err(CredentialLoadError::TooLarge { .. })
    ));
}

/// The secret never appears in broker Debug output or in any load or lease error, even when the parser would quote it.
#[test]
fn canary_never_in_errors_or_debug() {
    let mut outputs = Vec::new();
    // serde_json quotes the offending string in "invalid type" messages.
    for bad in [
        format!("{{\"credentials\": \"{SECRET}\"}}"),
        format!("{{\"credentials\": [{{\"id\": \"a\", \"secret\": \"{SECRET}\", \"actions\": \"{SECRET}\"}}]}}"),
        format!("{{\"credentials\": [{{\"id\": \"{SECRET} x\", \"secret\": \"{SECRET}\", \"actions\": [\"refund.issue\"]}}]}}"),
        format!("{{\"credentials\": [{{\"id\": \"a\", \"secret\": \"{SECRET}\", \"actions\": [\"{SECRET} \"]}}]}}"),
        format!("{{\"credentials\": [{{\"id\": \"a\", \"secret\": \"{SECRET}\""),
    ] {
        let (_d, path) = file_with(&bad, 0o600);
        let err = FileCredentialBroker::open(&path).expect_err("malformed file must fail");
        outputs.push(format!("{err} {err:?}"));
    }
    let (_d, path) = file_with(&creds_json(SECRET), 0o644);
    let err = FileCredentialBroker::open(&path).err().unwrap();
    outputs.push(format!("{err} {err:?}"));

    let (_d, path) = file_with(&creds_json(SECRET), 0o600);
    let b = FileCredentialBroker::open(&path).unwrap();
    outputs.push(format!("{b:?}"));
    let miss = block_on(b.lease(&action("invoice.list"))).err().unwrap();
    outputs.push(format!("{miss} {miss:?}"));
    let lease = block_on(b.lease(&action("refund.issue"))).unwrap();
    outputs.push(lease.credential_id().to_owned());

    std::env::set_var("GUARD_TEST_LEAK_SCAN_SECRET", SECRET);
    let env = EnvCredentialBroker::from_env(vec![EnvCredential {
        id: "business-api".into(),
        var: "GUARD_TEST_LEAK_SCAN_SECRET".into(),
        actions: vec![action("refund.issue")],
    }])
    .unwrap();
    outputs.push(format!("{env:?}"));

    for o in &outputs {
        assert!(!o.contains(SECRET), "secret leaked into: {o}");
        assert!(!o.contains("CANARY"), "secret fragment leaked into: {o}");
    }
}

/// Reload rotates the credential for new leases, existing leases keep the old value, and a failed reload keeps the last good table.
#[test]
fn rotation_on_reload() {
    let (_d, path) = file_with(&creds_json(SECRET), 0o600);
    let b = FileCredentialBroker::open(&path).unwrap();
    let old = block_on(b.lease(&action("refund.issue"))).unwrap();

    write(&path, &creds_json(ROTATED), 0o600);
    b.reload().unwrap();
    let new = block_on(b.lease(&action("refund.issue"))).unwrap();
    assert_eq!(secret_of(&old), SECRET);
    assert_eq!(secret_of(&new), ROTATED);

    write(&path, "{not json", 0o600);
    assert!(b.reload().is_err());
    write(&path, &creds_json("sk-world-readable"), 0o644);
    assert!(b.reload().is_err());
    assert_eq!(
        secret_of(&block_on(b.lease(&action("refund.issue"))).unwrap()),
        ROTATED
    );
}

/// The environment broker leases from its variables, reloads on rotation, and reports a missing variable by name only.
#[test]
fn env_broker_leases_and_rotates() {
    let var = "GUARD_TEST_ENV_BROKER_SECRET";
    std::env::set_var(var, SECRET);
    let spec = vec![EnvCredential {
        id: "business-api".into(),
        var: var.into(),
        actions: vec![action("refund.issue")],
    }];
    let b = EnvCredentialBroker::from_env(spec).unwrap();
    assert_eq!(
        secret_of(&block_on(b.lease(&action("refund.issue"))).unwrap()),
        SECRET
    );
    std::env::set_var(var, ROTATED);
    b.reload().unwrap();
    assert_eq!(
        secret_of(&block_on(b.lease(&action("refund.issue"))).unwrap()),
        ROTATED
    );

    let missing = EnvCredentialBroker::from_env(vec![EnvCredential {
        id: "x".into(),
        var: "GUARD_TEST_DEFINITELY_UNSET".into(),
        actions: vec![action("refund.issue")],
    }]);
    assert_eq!(
        missing.err(),
        Some(CredentialLoadError::MissingEnv(
            "GUARD_TEST_DEFINITELY_UNSET".into()
        ))
    );
}
