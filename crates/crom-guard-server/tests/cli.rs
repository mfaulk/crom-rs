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

/// Bad usage exits 2, and no arguments still refuses to start without an agent endpoint.
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
}
