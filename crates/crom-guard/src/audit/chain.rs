//! `guard-audit/v1` line encoding and chain verification. The byte-level
//! rules are specified in `docs/guard/audit.md`; keep the two in sync.

use std::collections::HashMap;
use std::io::{self, BufRead};
use std::path::Path;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{AuditReceipt, AuditRecord, AUDIT_FORMAT};

/// `prev_hash` of the record with `seq` 1.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

const HASH_MEMBER: &[u8] = b",\"hash\":\"";
/// `,"hash":"` + 64 hex digits + `"}`.
const HASH_SUFFIX_LEN: usize = HASH_MEMBER.len() + 64 + 2;

/// Encodes one record as a newline-terminated line and returns it with its
/// hash.
///
/// The hash input is the line's JSON object without its final `hash`
/// member: `{"format":..,"seq":..,"prev_hash":..,<record members>}`, compact
/// serde_json output. Verifiers hash the stored bytes and never
/// re-serialize, so they need no JSON canonicalization.
pub(super) fn encode_line(
    seq: u64,
    prev_hash: &str,
    record: &AuditRecord,
) -> Result<(Vec<u8>, String), serde_json::Error> {
    let body = serde_json::to_vec(record)?;
    debug_assert!(body.len() > 2 && body[0] == b'{' && body[body.len() - 1] == b'}');
    let mut line =
        format!("{{\"format\":\"{AUDIT_FORMAT}\",\"seq\":{seq},\"prev_hash\":\"{prev_hash}\",")
            .into_bytes();
    line.reserve(body.len() + HASH_SUFFIX_LEN);
    line.extend_from_slice(&body[1..]);
    let hash = hex::encode(Sha256::digest(&line));
    line.pop();
    line.extend_from_slice(HASH_MEMBER);
    line.extend_from_slice(hash.as_bytes());
    line.extend_from_slice(b"\"}\n");
    Ok((line, hash))
}

/// A verified record and its chain fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub seq: u64,
    pub prev_hash: String,
    pub hash: String,
    pub record: AuditRecord,
}

/// Result of a successful verification. `head` is `None` for an empty log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSummary {
    pub records: u64,
    pub head: Option<AuditReceipt>,
}

/// The first defect found. `line` is 1-based.
#[derive(Debug, thiserror::Error)]
#[error("audit log line {line}: {kind}")]
pub struct VerifyError {
    pub line: u64,
    pub kind: VerifyErrorKind,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyErrorKind {
    #[error("read failed: {0}")]
    Io(#[from] io::Error),
    /// The last line has no terminating newline: a torn write or a
    /// byte-level truncation.
    #[error("partial trailing record (no terminating newline)")]
    PartialTrailingRecord,
    #[error("malformed record: {0}")]
    Malformed(String),
    #[error("stored hash does not match the record contents")]
    HashMismatch,
    #[error("unsupported format {0:?}")]
    WrongFormat(String),
    /// A record was deleted, inserted, or reordered.
    #[error("expected seq {expected}, found {found}")]
    SeqMismatch { expected: u64, found: u64 },
    /// The record does not link to its predecessor's hash.
    #[error("prev_hash does not match the previous record's hash")]
    PrevHashMismatch,
    #[error("record does not match the guard-audit/v1 schema: {0}")]
    Schema(String),
    /// An anchor's seq is present with a different hash: the chain was
    /// rewritten.
    #[error("anchor at seq {seq} does not match the log")]
    AnchorMismatch { seq: u64 },
    /// An anchor's seq is past the end of the log: records were truncated.
    #[error("anchor at seq {seq} is past the end of the log")]
    AnchorMissing { seq: u64 },
}

/// Verifies a `guard-audit/v1` log. See [`verify_log_with`].
pub fn verify_log<R: BufRead>(
    reader: R,
    anchors: &[AuditReceipt],
) -> Result<ChainSummary, VerifyError> {
    verify_log_with(reader, anchors, |_| {})
}

/// Opens and verifies the log at `path`. See [`verify_log_with`].
pub fn verify_file(
    path: impl AsRef<Path>,
    anchors: &[AuditReceipt],
) -> Result<ChainSummary, VerifyError> {
    let file = std::fs::File::open(path).map_err(|e| VerifyError {
        line: 0,
        kind: e.into(),
    })?;
    verify_log(io::BufReader::new(file), anchors)
}

/// Verifies every line's hash, format, schema, `seq` (1, 2, 3, ...) and
/// `prev_hash` link, calling `visit` for each record in order.
///
/// Each line is `{"format":"guard-audit/v1","seq":N,"prev_hash":"<hex>",
/// <AuditRecord members>,"hash":"<hex>"}\n`, and `hash` is lowercase hex
/// SHA-256 over the line's bytes with the final `,"hash":"<64 hex>"`
/// member removed (the closing `}` kept). The stored bytes are hashed
/// as-is, so an independent verifier needs no JSON canonicalization.
///
/// Without anchors this detects in-place edits, deletion, insertion,
/// reordering, and torn or byte-truncated trailing records. Dropping whole
/// trailing records, or rewriting the entire chain, is detectable only
/// against receipts a client kept: each anchor's `seq` must be present
/// with the same hash.
pub fn verify_log_with<R: BufRead>(
    mut reader: R,
    anchors: &[AuditReceipt],
    mut visit: impl FnMut(AuditEntry),
) -> Result<ChainSummary, VerifyError> {
    let anchors: HashMap<u64, &str> = anchors
        .iter()
        .map(|a| (a.seq, a.head_hash.as_str()))
        .collect();
    let mut head: Option<AuditReceipt> = None;
    let mut buf = Vec::new();
    loop {
        let line_no = head.as_ref().map_or(1, |h| h.seq + 1);
        let fail = |kind| VerifyError {
            line: line_no,
            kind,
        };
        buf.clear();
        if reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| fail(e.into()))?
            == 0
        {
            break;
        }
        if buf.pop() != Some(b'\n') {
            return Err(fail(VerifyErrorKind::PartialTrailingRecord));
        }
        let prev = head.as_ref().map_or(GENESIS_HASH, |h| h.head_hash.as_str());
        let entry = check_line(&buf, line_no, prev).map_err(fail)?;
        if let Some(expected) = anchors.get(&entry.seq) {
            if *expected != entry.hash {
                return Err(fail(VerifyErrorKind::AnchorMismatch { seq: entry.seq }));
            }
        }
        head = Some(AuditReceipt {
            seq: entry.seq,
            head_hash: entry.hash.clone(),
        });
        visit(entry);
    }
    let records = head.as_ref().map_or(0, |h| h.seq);
    if let Some(&seq) = anchors.keys().filter(|&&s| s > records).min() {
        return Err(VerifyError {
            line: records + 1,
            kind: VerifyErrorKind::AnchorMissing { seq },
        });
    }
    Ok(ChainSummary { records, head })
}

fn check_line(line: &[u8], seq: u64, prev: &str) -> Result<AuditEntry, VerifyErrorKind> {
    use VerifyErrorKind::*;
    let malformed = |m: &str| Malformed(m.to_owned());

    let split = line
        .len()
        .checked_sub(HASH_SUFFIX_LEN)
        .ok_or_else(|| malformed("too short"))?;
    let (body, suffix) = line.split_at(split);
    let stored = &suffix[HASH_MEMBER.len()..HASH_MEMBER.len() + 64];
    if !suffix.starts_with(HASH_MEMBER)
        || !suffix.ends_with(b"\"}")
        || !stored
            .iter()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(malformed("line does not end with a hash member"));
    }
    let mut hasher = Sha256::new();
    hasher.update(body);
    hasher.update(b"}");
    let hash = hex::encode(hasher.finalize());
    if hash.as_bytes() != stored {
        return Err(HashMismatch);
    }

    let mut obj: Map<String, Value> =
        serde_json::from_slice(line).map_err(|e| Malformed(e.to_string()))?;
    let mut take_str = |key: &str| match obj.remove(key) {
        Some(Value::String(s)) => Ok(s),
        _ => Err(Malformed(format!("missing or non-string {key:?}"))),
    };
    let format = take_str("format")?;
    let prev_hash = take_str("prev_hash")?;
    let member_hash = take_str("hash")?;
    if format != AUDIT_FORMAT {
        return Err(WrongFormat(format));
    }
    let found = obj
        .remove("seq")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| malformed("missing or non-integer \"seq\""))?;
    if found != seq {
        return Err(SeqMismatch {
            expected: seq,
            found,
        });
    }
    if prev_hash != prev {
        return Err(PrevHashMismatch);
    }
    if member_hash != hash {
        return Err(malformed("duplicate \"hash\" member"));
    }

    let rest = Value::Object(obj);
    let record: AuditRecord =
        serde_json::from_value(rest.clone()).map_err(|e| Schema(e.to_string()))?;
    // Deserialization ignores unknown members; a round trip exposes them.
    if serde_json::to_value(&record).ok().as_ref() != Some(&rest) {
        return Err(Schema("unknown or extra members".to_owned()));
    }
    Ok(AuditEntry {
        seq,
        prev_hash,
        hash,
        record,
    })
}
