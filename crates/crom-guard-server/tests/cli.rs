use std::path::Path;
use std::process::{Command, Output};

use crom_guard::{AuditRecord, AuditSink, DecisionId, FileAuditSink, RecordKind, Timestamp};

fn guard(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_crom-guard"))
        .args(args)
        .output()
        .unwrap()
}

fn write_log(path: &Path, n: u64) -> crom_guard::AuditReceipt {
    let sink = FileAuditSink::open(path).unwrap();
    let mut last = None;
    for i in 1..=n {
        let r = AuditRecord::new(
            RecordKind::Decision,
            DecisionId::new(format!("dec-{i}")).unwrap(),
            Timestamp::from_unix_millis(i),
        );
        last = Some(futures::executor::block_on(sink.append(&r)).unwrap());
    }
    last.unwrap()
}

/// `audit verify` exits 0 and prints the record count and head for an intact log.
#[test]
fn verify_intact_log_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let head = write_log(&path, 3);
    let anchor = format!("{}:{}", head.seq, head.head_hash);
    let out = guard(&[
        "audit",
        "verify",
        path.to_str().unwrap(),
        "--anchor",
        &anchor,
    ]);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("3 records"), "{stdout}");
    assert!(stdout.contains(&head.head_hash), "{stdout}");
}

/// `audit verify` exits 1 and names the failing line when a record was edited.
#[test]
fn verify_tampered_log_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    write_log(&path, 2);
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("\"dec-2\"", "\"dec-9\"");
    std::fs::write(&path, edited).unwrap();
    let out = guard(&["audit", "verify", path.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr).unwrap().contains("line 2"));
}

/// `audit verify` exits 1 when a client-held anchor is past the end of the log.
#[test]
fn verify_truncated_log_fails_against_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let head = write_log(&path, 2);
    let first_line = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    std::fs::write(&path, format!("{first_line}\n")).unwrap();
    let anchor = format!("{}:{}", head.seq, head.head_hash);
    let out = guard(&[
        "audit",
        "verify",
        path.to_str().unwrap(),
        "--anchor",
        &anchor,
    ]);
    assert_eq!(out.status.code(), Some(1));
}

/// Bad usage exits 2.
#[test]
fn usage_errors_exit_2() {
    assert_eq!(guard(&[]).status.code(), Some(2));
    assert_eq!(guard(&["audit"]).status.code(), Some(2));
    assert_eq!(guard(&["audit", "verify"]).status.code(), Some(2));
    assert_eq!(
        guard(&["audit", "verify", "x", "--anchor", "nope"])
            .status
            .code(),
        Some(2)
    );
    assert_eq!(guard(&["serve"]).status.code(), Some(2));
    assert_eq!(guard(&["serve", "--bogus", "x"]).status.code(), Some(2));
    assert_eq!(guard(&["policy", "validate"]).status.code(), Some(2));
}

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");

/// `policy validate` exits 0 for the reference policies and 1 for a broken policy.
#[test]
fn policy_validate() {
    let dir = tempfile::tempdir().unwrap();
    let schema = dir.path().join("s.cedarschema");
    let good = dir.path().join("good.cedar");
    let bad = dir.path().join("bad.cedar");
    std::fs::write(&schema, SCHEMA).unwrap();
    std::fs::write(&good, POLICIES).unwrap();
    std::fs::write(&bad, "permit (principal, action, resource) when { x };").unwrap();
    let run = |p: &Path| {
        guard(&[
            "policy",
            "validate",
            "--namespace",
            "Support",
            "--schema",
            schema.to_str().unwrap(),
            "--policies",
            p.to_str().unwrap(),
        ])
    };
    assert_eq!(run(&good).status.code(), Some(0));
    let out = run(&bad);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("policy invalid"));
}

/// `serve` exits 1 with a reason when its configuration cannot be loaded.
#[test]
fn serve_refuses_bad_config() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing.toml");
    let out = guard(&["serve", "--config", missing.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stderr)
        .unwrap()
        .contains("refusing to start"));
}

/// `serve` starts from a config file, answers on the unix admin socket, and exits 0 on SIGTERM.
#[cfg(unix)]
#[test]
fn serve_starts_and_stops_on_sigterm() {
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    let dir = tempfile::tempdir().unwrap();
    let private = |name: &str, body: &str| {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    std::fs::write(dir.path().join("s.cedarschema"), SCHEMA).unwrap();
    std::fs::write(dir.path().join("p.cedar"), POLICIES).unwrap();
    private("creds.json", r#"{"credentials": []}"#);
    let token = "adm_cli_0123456789abcdef0123456789abcdef";
    private("admin.token", token);
    let config = dir.path().join("guard.toml");
    std::fs::write(
        &config,
        r#"
        [admin]
        listen = "unix:admin.sock"
        token_file = "admin.token"
        app_id = "app"
        [policy]
        namespace = "Support"
        schema = "s.cedarschema"
        policies = "p.cedar"
        [credentials]
        file = "creds.json"
        [audit]
        path = "audit.jsonl"
        "#,
    )
    .unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_crom-guard"))
        .args([
            "serve",
            "--config",
            config.to_str().unwrap(),
            "--agent-listen",
            "127.0.0.1:0",
        ])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let sock = dir.path().join("admin.sock");
    let mut response = String::new();
    for _ in 0..500 {
        if let Ok(mut s) = UnixStream::connect(&sock) {
            write!(
                s,
                "GET /admin/health HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {token}\r\n\
                 Connection: close\r\n\r\n"
            )
            .unwrap();
            s.read_to_string(&mut response).unwrap();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    assert_eq!(child.wait().unwrap().code(), Some(0));
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(!stderr.contains(token), "{stderr}");
    assert!(!sock.exists());
}

fn example(path: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/guard-support")
        .join(path)
}

/// The reference guard.toml and policies copied into a 0700 state
/// directory, with a 0600 token and credentials file.
#[cfg(unix)]
fn reference_state(root: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let state = root.join("state");
    std::fs::create_dir_all(state.join("policies")).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::copy(example("guard.toml"), state.join("guard.toml")).unwrap();
    for f in ["policies/support.cedarschema", "policies/support.cedar"] {
        std::fs::copy(example(f), state.join(f)).unwrap();
    }
    for (name, body) in [
        ("admin.token", "adm_cli_0123456789abcdef0123456789abcdef"),
        ("credentials.json", r#"{"credentials": []}"#),
    ] {
        std::fs::write(state.join(name), body).unwrap();
        std::fs::set_permissions(state.join(name), std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    state
}

/// `check` exits 0 for the reference isolation profile (the shipped guard.toml and agent/crom-settings.json).
#[cfg(target_os = "linux")] // `--crom-settings` fails `platform` elsewhere
#[test]
fn check_passes_reference_profile() {
    let dir = tempfile::tempdir().unwrap();
    let state = reference_state(dir.path());
    let crom = dir.path().join("crom");
    let ws = dir.path().join("workspace");
    std::fs::create_dir(&crom).unwrap();
    std::fs::create_dir(&ws).unwrap();
    std::fs::copy(
        example("agent/crom-settings.json"),
        crom.join("settings.json"),
    )
    .unwrap();
    let run = |settings: &Path| {
        Command::new(env!("CARGO_BIN_EXE_crom-guard"))
            .args([
                "check",
                "--config",
                state.join("guard.toml").to_str().unwrap(),
                "--crom-settings",
                settings.to_str().unwrap(),
                "--crom-workspace",
                ws.to_str().unwrap(),
            ])
            .env("CROM_CONFIG_HOME", &crom)
            .output()
            .unwrap()
    };
    let out = run(&crom.join("settings.json"));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(
        stdout.contains("ok: deployment matches the isolation profile"),
        "{stdout}"
    );
    assert!(!stdout.contains("adm_cli_"), "{stdout}");

    // The same settings at a path Crom would not read fail.
    let out = run(&example("agent/crom-settings.json"));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("FAIL: crom-settings"));
}

/// `check` exits 1 and names the failed check when no agent isolation is declared or the state directory is open to others.
#[cfg(unix)]
#[test]
fn check_fails_closed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let state = reference_state(dir.path());
    let config = state.join("guard.toml");
    let out = guard(&["check", "--config", config.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("FAIL: agent-isolation"));

    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = guard(&[
        "check",
        "--config",
        config.to_str().unwrap(),
        "--agent-user",
        "65534",
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("FAIL: state-dir"));
}

/// `check` usage errors, including an unknown `--agent-user`, exit 2.
#[test]
fn check_usage_errors_exit_2() {
    assert_eq!(guard(&["check"]).status.code(), Some(2));
    assert_eq!(guard(&["check", "--bogus", "x"]).status.code(), Some(2));
    assert_eq!(
        guard(&[
            "check",
            "--config",
            "x",
            "--agent-user",
            "no-such-user-crom"
        ])
        .status
        .code(),
        Some(2)
    );
}
