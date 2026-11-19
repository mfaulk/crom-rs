use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crom_guard::{
    verify_file, verify_log, verify_log_with, ActionId, AgentId, AppId, AuditError, AuditIdentity,
    AuditReceipt, AuditRecord, AuditSink, AuditStorage, ChainedAuditSink, DecisionId, DecisionKind,
    DenyReason, Enforcement, EntityUid, FileAuditSink, GrantId, Outcome, PolicyId, RecordKind,
    RunId, TenantId, Timestamp, UserId, VerifyErrorKind, AUDIT_FORMAT, GENESIS_HASH,
};
use sha2::{Digest, Sha256};

const SCHEMA: &str = include_str!("../schemas/guard-audit-v1.schema.json");

fn decision_record(n: u64) -> AuditRecord {
    let mut r = AuditRecord::new(
        RecordKind::Decision,
        DecisionId::new(format!("dec-{n}")).unwrap(),
        Timestamp::from_unix_millis(1_700_000_000_000 + n),
    );
    let grant = GrantId::new("grant-1").unwrap();
    r.identity = Some(AuditIdentity {
        issuer: AppId::new("support-app").unwrap(),
        tenant: TenantId::new("acme").unwrap(),
        delegator: UserId::new("alice").unwrap(),
        agent: AgentId::new("agent-7").unwrap(),
        run_id: RunId::new("run-1").unwrap(),
        grant_id: grant.clone(),
        grant_chain: vec![grant],
    });
    r.action = Some(ActionId::new("refund").unwrap());
    r.resource = Some(EntityUid::new("Support::Invoice", "inv-1").unwrap());
    r.args_digest = Some("ab".repeat(32));
    r.args_logged
        .insert("amount_cents".into(), serde_json::json!(1250));
    r.decision = Some(DecisionKind::Allow);
    r.determining_policies = vec![PolicyId::new("p0").unwrap()];
    r.policy_set_hash = Some("cd".repeat(32));
    r.schema_hash = Some("ef".repeat(32));
    r.enforcement = Some(Enforcement::GuardPolicy);
    r.duration_us.policy_us = Some(42);
    r.untrusted.tool_use_id = Some("toolu_1".into());
    r.untrusted.client_info = Some(serde_json::json!({"name": "client", "version": 1.5}));
    r
}

fn outcome_record(n: u64) -> AuditRecord {
    let mut r = decision_record(n);
    r.record = RecordKind::Outcome;
    r.outcome = Some(Outcome::Executed);
    r.downstream_ref = Some(format!("eff-{n}"));
    r.duration_us.execute_us = Some(900);
    r
}

fn denied_record(n: u64) -> AuditRecord {
    let mut r = decision_record(n);
    r.decision = Some(DecisionKind::Deny);
    r.reason = Some(DenyReason::ChainDeny {
        grant_id: GrantId::new("grant-0").unwrap(),
    });
    r.enforcement = Some(Enforcement::GuardChain);
    r.outcome = Some(Outcome::Blocked);
    r
}

fn append(sink: &dyn AuditSink, r: &AuditRecord) -> Result<AuditReceipt, AuditError> {
    futures::executor::block_on(sink.append(r))
}

/// Writes `n` decision records to a fresh log and returns its path and receipts.
fn write_log(dir: &Path, n: u64) -> (PathBuf, Vec<AuditReceipt>) {
    let path = dir.join("audit.jsonl");
    let sink = FileAuditSink::open(&path).unwrap();
    let receipts = (1..=n)
        .map(|i| append(&sink, &decision_record(i)).unwrap())
        .collect();
    (path, receipts)
}

fn lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_lines(path: &Path, lines: &[String]) {
    let mut s = lines.join("\n");
    s.push('\n');
    fs::write(path, s).unwrap();
}

fn kind_at(path: &Path, anchors: &[AuditReceipt]) -> (u64, VerifyErrorKind) {
    let e = verify_file(path, anchors).expect_err("verify should fail");
    (e.line, e.kind)
}

/// Independent re-implementation of the documented hash rule: drop the
/// trailing `,"hash":"<hex>"` member and hash the remaining object bytes.
fn spec_hash(line: &str) -> String {
    let (body, _) = line.rsplit_once(",\"hash\":\"").unwrap();
    let mut input = body.as_bytes().to_vec();
    input.push(b'}');
    hex::encode(Sha256::digest(&input))
}

/// Rewrites a line's hash member so it is self-consistent (an attacker
/// re-hashing an edited record).
fn rehash(line: &str) -> String {
    let (body, _) = line.rsplit_once(",\"hash\":\"").unwrap();
    format!("{body},\"hash\":\"{}\"}}", spec_hash(line))
}

/// Appends get strictly increasing seq numbers starting at 1, and each receipt's head hash is that record's hash.
#[test]
fn appends_assign_seq_and_head_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 3);
    assert_eq!(
        receipts.iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let summary = verify_file(&path, &receipts).unwrap();
    assert_eq!(summary.records, 3);
    assert_eq!(summary.head.as_ref(), receipts.last());
    let ls = lines(&path);
    for (line, receipt) in ls.iter().zip(&receipts) {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["hash"], receipt.head_hash.as_str());
        assert_eq!(v["format"], AUDIT_FORMAT);
    }
}

/// The first record links to the all-zero genesis hash and each later record links to its predecessor's hash.
#[test]
fn records_link_prev_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 3);
    assert_eq!(GENESIS_HASH, "0".repeat(64));
    let prevs: Vec<String> = lines(&path)
        .iter()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap()["prev_hash"].to_string())
        .collect();
    assert_eq!(prevs[0], format!("\"{GENESIS_HASH}\""));
    assert_eq!(prevs[1], format!("\"{}\"", receipts[0].head_hash));
    assert_eq!(prevs[2], format!("\"{}\"", receipts[1].head_hash));
}

/// The stored hash equals SHA-256 over the line minus its trailing hash member, as documented.
#[test]
fn hash_is_reproducible_from_documented_rule() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 2);
    for (line, receipt) in lines(&path).iter().zip(&receipts) {
        assert!(line.starts_with(&format!(
            "{{\"format\":\"{AUDIT_FORMAT}\",\"seq\":{},\"prev_hash\":\"",
            receipt.seq
        )));
        assert!(line.ends_with(&format!(",\"hash\":\"{}\"}}", receipt.head_hash)));
        assert_eq!(spec_hash(line), receipt.head_hash);
    }
}

/// An in-place edit of a record field is detected as a hash mismatch on that line.
#[test]
fn edit_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls[1] = ls[1].replace("\"amount_cents\":1250", "\"amount_cents\":9999");
    write_lines(&path, &ls);
    assert!(matches!(
        kind_at(&path, &[]),
        (2, VerifyErrorKind::HashMismatch)
    ));
}

/// An edit whose line hash is recomputed still breaks the next record's prev_hash link.
#[test]
fn rehashed_edit_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls[1] = rehash(&ls[1].replace("\"allow\"", "\"deny\""));
    write_lines(&path, &ls);
    assert!(matches!(
        kind_at(&path, &[]),
        (3, VerifyErrorKind::PrevHashMismatch)
    ));
}

/// Deleting a middle record is detected as a seq gap.
#[test]
fn deletion_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls.remove(1);
    write_lines(&path, &ls);
    assert!(matches!(
        kind_at(&path, &[]),
        (
            2,
            VerifyErrorKind::SeqMismatch {
                expected: 2,
                found: 3
            }
        )
    ));
}

/// Deleting the first record is detected because the log must start at seq 1.
#[test]
fn deletion_of_first_record_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls.remove(0);
    write_lines(&path, &ls);
    assert!(matches!(
        kind_at(&path, &[]),
        (
            1,
            VerifyErrorKind::SeqMismatch {
                expected: 1,
                found: 2
            }
        )
    ));
}

/// Swapping two records is detected as a seq mismatch.
#[test]
fn reordering_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls.swap(1, 2);
    write_lines(&path, &ls);
    assert!(matches!(
        kind_at(&path, &[]),
        (
            2,
            VerifyErrorKind::SeqMismatch {
                expected: 2,
                found: 3
            }
        )
    ));
}

/// Inserting a duplicated record, or one renumbered and re-hashed to fit, is detected.
#[test]
fn insertion_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let orig = lines(&path);

    let mut dup = orig.clone();
    dup.insert(2, orig[1].clone());
    write_lines(&path, &dup);
    assert!(matches!(
        kind_at(&path, &[]),
        (
            3,
            VerifyErrorKind::SeqMismatch {
                expected: 3,
                found: 2
            }
        )
    ));

    let mut forged = orig.clone();
    let fake = rehash(&orig[1].replace("\"seq\":2,", "\"seq\":3,"));
    forged.insert(2, fake);
    write_lines(&path, &forged);
    assert!(matches!(
        kind_at(&path, &[]),
        (3, VerifyErrorKind::PrevHashMismatch)
    ));
}

/// A log cut mid-record (as after a crash or a byte-level truncation) is reported as a partial trailing record.
#[test]
fn mid_record_truncation_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 3);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 10]).unwrap();
    assert!(matches!(
        kind_at(&path, &[]),
        (3, VerifyErrorKind::PartialTrailingRecord)
    ));
}

/// Dropping whole trailing records verifies on its own but is detected against a client-held receipt.
#[test]
fn tail_truncation_is_detected_with_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 3);
    let mut ls = lines(&path);
    ls.pop();
    write_lines(&path, &ls);
    assert_eq!(verify_file(&path, &[]).unwrap().records, 2);
    assert!(matches!(
        kind_at(&path, &receipts[2..]),
        (3, VerifyErrorKind::AnchorMissing { seq: 3 })
    ));
}

/// A fully rewritten, self-consistent chain verifies on its own but fails against a client-held receipt.
#[test]
fn rewritten_chain_is_detected_with_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 2);
    let forged_dir = tempfile::tempdir().unwrap();
    let forged_path = forged_dir.path().join("audit.jsonl");
    let forged = FileAuditSink::open(&forged_path).unwrap();
    append(&forged, &denied_record(1)).unwrap();
    append(&forged, &decision_record(2)).unwrap();
    drop(forged);
    fs::copy(&forged_path, &path).unwrap();
    assert!(verify_file(&path, &[]).is_ok());
    assert!(matches!(
        kind_at(&path, &receipts[..1]),
        (1, VerifyErrorKind::AnchorMismatch { seq: 1 })
    ));
}

/// A re-hashed line with an unknown field or a wrong format tag fails schema checks.
#[test]
fn schema_violations_are_detected() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 1);
    let orig = lines(&path);

    let extra = rehash(&orig[0].replace("\"seq\":1,", "\"seq\":1,\"note\":\"x\","));
    write_lines(&path, &[extra]);
    assert!(matches!(
        kind_at(&path, &[]),
        (1, VerifyErrorKind::Schema(_))
    ));

    let bad_enum = rehash(&orig[0].replace("\"allow\"", "\"maybe\""));
    write_lines(&path, &[bad_enum]);
    assert!(matches!(
        kind_at(&path, &[]),
        (1, VerifyErrorKind::Schema(_))
    ));

    let format = rehash(&orig[0].replace(AUDIT_FORMAT, "guard-audit/v0"));
    write_lines(&path, &[format]);
    assert!(matches!(
        kind_at(&path, &[]),
        (1, VerifyErrorKind::WrongFormat(_))
    ));

    write_lines(&path, &["not json".to_owned()]);
    assert!(matches!(
        kind_at(&path, &[]),
        (1, VerifyErrorKind::Malformed(_))
    ));
}

/// An empty log verifies with zero records and no head.
#[test]
fn empty_log_verifies() {
    let summary = verify_log(Cursor::new(Vec::new()), &[]).unwrap();
    assert_eq!(summary.records, 0);
    assert!(summary.head.is_none());
}

/// The verifier hands every record to the visitor so effects can be correlated by decision id and downstream ref.
#[test]
fn visitor_supports_ledger_correlation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = FileAuditSink::open(&path).unwrap();
    append(&sink, &decision_record(1)).unwrap();
    append(&sink, &outcome_record(1)).unwrap();
    let mut executed = Vec::new();
    let file = io::BufReader::new(fs::File::open(&path).unwrap());
    verify_log_with(file, &[], |entry| {
        if entry.record.outcome == Some(Outcome::Executed) {
            executed.push((
                entry.seq,
                entry.record.decision_id.as_str().to_owned(),
                entry.record.downstream_ref.clone(),
            ));
        }
    })
    .unwrap();
    assert_eq!(
        executed,
        vec![(2, "dec-1".to_owned(), Some("eff-1".to_owned()))]
    );
}

/// Reopening a log resumes the chain from its verified head.
#[test]
fn reopen_resumes_chain() {
    let dir = tempfile::tempdir().unwrap();
    let (path, receipts) = write_log(dir.path(), 2);
    let sink = FileAuditSink::open(&path).unwrap();
    assert_eq!(sink.head().as_ref(), receipts.last());
    let r3 = append(&sink, &decision_record(3)).unwrap();
    assert_eq!(r3.seq, 3);
    drop(sink);
    let summary = verify_file(&path, &receipts).unwrap();
    assert_eq!(summary.head, Some(r3));
}

/// Opening a log that fails verification, including one with a partial trailing record, is refused.
#[test]
fn open_refuses_broken_log() {
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 2);
    let bytes = fs::read(&path).unwrap();
    fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert!(matches!(
        FileAuditSink::open(&path),
        Err(AuditError::Unavailable(_))
    ));
}

/// A second sink cannot open a log another sink holds, so two writers never fork the chain.
#[test]
fn open_refuses_concurrent_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let _first = FileAuditSink::open(&path).unwrap();
    assert!(matches!(
        FileAuditSink::open(&path),
        Err(AuditError::Unavailable(_))
    ));
}

/// A new log is created with mode 0600 and an existing group- or world-accessible log is refused.
#[cfg(unix)]
#[test]
fn log_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (path, _) = write_log(dir.path(), 1);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        FileAuditSink::open(&path),
        Err(AuditError::Unavailable(_))
    ));
}

/// Concurrent appends from many threads are serialized into one gap-free chain.
#[test]
fn concurrent_appends_are_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = Arc::new(FileAuditSink::open(&path).unwrap());
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let sink = Arc::clone(&sink);
            std::thread::spawn(move || {
                (0..25)
                    .map(|i| append(&*sink, &decision_record(t * 100 + i)).unwrap().seq)
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seqs: Vec<u64> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=200).collect::<Vec<_>>());
    assert_eq!(verify_file(&path, &[]).unwrap().records, 200);
}

/// Storage that records durable appends and can be switched to fail.
#[derive(Debug, Clone, Default)]
struct FlakyStorage {
    lines: Arc<Mutex<Vec<Vec<u8>>>>,
    fail: Arc<Mutex<bool>>,
}

impl AuditStorage for FlakyStorage {
    fn append_durable(&mut self, line: &[u8]) -> io::Result<()> {
        if *self.fail.lock().unwrap() {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        self.lines.lock().unwrap().push(line.to_vec());
        Ok(())
    }
}

/// Each append hands storage exactly one complete newline-terminated record before resolving.
#[test]
fn append_writes_one_complete_line_per_record() {
    let storage = FlakyStorage::default();
    let sink = ChainedAuditSink::with_storage(storage.clone(), None);
    append(&sink, &decision_record(1)).unwrap();
    append(&sink, &outcome_record(1)).unwrap();
    let written = storage.lines.lock().unwrap().concat();
    let summary = verify_log(Cursor::new(written), &[]).unwrap();
    assert_eq!(summary.records, 2);
}

/// A full disk fails the append (so the pipeline denies) and the sink stays failed afterwards.
#[test]
fn disk_full_fails_closed() {
    let storage = FlakyStorage::default();
    let sink = ChainedAuditSink::with_storage(storage.clone(), None);
    let r1 = append(&sink, &decision_record(1)).unwrap();
    *storage.fail.lock().unwrap() = true;
    assert!(matches!(
        append(&sink, &decision_record(2)),
        Err(AuditError::Io(e)) if e.kind() == io::ErrorKind::StorageFull
    ));
    *storage.fail.lock().unwrap() = false;
    assert!(matches!(
        append(&sink, &decision_record(3)),
        Err(AuditError::Unavailable(_))
    ));
    assert_eq!(sink.head(), Some(r1));
    assert_eq!(storage.lines.lock().unwrap().len(), 1);
}

/// A real ENOSPC from the kernel (`/dev/full`) fails the append.
#[cfg(target_os = "linux")]
#[test]
fn dev_full_fails_append() {
    let file = fs::OpenOptions::new()
        .append(true)
        .open("/dev/full")
        .unwrap();
    let sink = ChainedAuditSink::with_storage(file, None);
    assert!(matches!(
        append(&sink, &decision_record(1)),
        Err(AuditError::Io(e)) if e.kind() == io::ErrorKind::StorageFull
    ));
    assert!(sink.head().is_none());
}

/// A record carrying a plaintext session handle is refused, nothing reaches the log, and the sink keeps working.
#[test]
fn plaintext_handle_canary_is_refused() {
    let canary = format!("gsh_{}", "a1".repeat(32));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = FileAuditSink::open(&path).unwrap();
    let mut r = decision_record(1);
    r.untrusted.client_request_id = Some(format!("req {canary}"));
    assert!(matches!(append(&sink, &r), Err(AuditError::Unavailable(_))));
    let mut r = decision_record(2);
    r.args_logged
        .insert("note".into(), serde_json::json!(canary));
    assert!(matches!(append(&sink, &r), Err(AuditError::Unavailable(_))));
    let ok = append(&sink, &decision_record(3)).unwrap();
    assert_eq!(ok.seq, 1);
    assert!(!fs::read_to_string(&path).unwrap().contains(&canary));
}

/// Every record shape the sink writes validates against the published JSON Schema, and mutated lines do not.
#[test]
fn published_schema_validates_records() {
    let schema: serde_json::Value = serde_json::from_str(SCHEMA).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let sink = FileAuditSink::open(&path).unwrap();
    let mut minimal = AuditRecord::new(
        RecordKind::Decision,
        DecisionId::new("dec-min").unwrap(),
        Timestamp::from_unix_millis(1),
    );
    minimal.untrusted.requested_tool = Some("unknown_tool".into());
    minimal.reason = Some(DenyReason::UnknownAction);
    for r in [
        minimal,
        decision_record(1),
        outcome_record(1),
        denied_record(2),
    ] {
        append(&sink, &r).unwrap();
    }
    for line in lines(&path) {
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(errors.is_empty(), "{line}: {errors:?}");

        let mutations: [fn(&mut serde_json::Map<String, serde_json::Value>); 4] = [
            |o| {
                o.remove("decision_id");
            },
            |o| {
                o.insert("extra".into(), serde_json::json!(1));
            },
            |o| {
                o.insert("record".into(), serde_json::json!("maybe"));
            },
            |o| {
                o.insert("hash".into(), serde_json::json!("XYZ"));
            },
        ];
        for mutate in mutations {
            let mut bad = v.clone();
            mutate(bad.as_object_mut().unwrap());
            assert!(!validator.is_valid(&bad), "accepted {bad}");
        }
    }
}
