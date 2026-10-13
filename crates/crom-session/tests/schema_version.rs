//! Integration tests for `SessionRecord` schema versioning (issue #16).
//!
//! These exercise the on-disk contract directly: a committed v0 fixture
//! (JSONL without a `schema_version` field) must still load and replay, a
//! record from a newer build must fail loudly, and a freshly written record
//! must round-trip at `CURRENT_SCHEMA_VERSION`.

use crom_session::{
    detect_state, load_records_resilient, replay_conversation, FsSessionStore, SessionError,
    SessionPayload, SessionRecord, SessionStore, CURRENT_SCHEMA_VERSION,
};

/// Minimal probe mirroring the production pre-parse schema-version gate: read
/// ONLY `schema_version`, ignoring every other field, so a future-version line
/// is detected even when the full `SessionRecord` shape would fail to parse.
#[derive(serde::Deserialize)]
struct SchemaVersionProbe {
    #[serde(default = "zero")]
    schema_version: u32,
}
fn zero() -> u32 {
    0
}

/// Parse a JSONL string into validated records exactly as the load paths do:
/// run the pre-parse version gate first (a future version is a hard error
/// regardless of payload parseability), then deserialize and validate/migrate.
fn load_jsonl(text: &str) -> Result<Vec<SessionRecord>, SessionError> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(probe) = serde_json::from_str::<SchemaVersionProbe>(line) {
            if probe.schema_version > CURRENT_SCHEMA_VERSION {
                return Err(SessionError::UnsupportedSchemaVersion {
                    found: probe.schema_version,
                    supported: CURRENT_SCHEMA_VERSION,
                });
            }
        }
        let rec: SessionRecord = serde_json::from_str(line)?;
        out.push(rec.validate_schema_version()?);
    }
    Ok(out)
}

#[test]
// A committed pre-versioning JSONL fixture (no `schema_version` field) loads
// as v0-on-disk, is migrated up to the current version, and replays.
fn v0_fixture_loads_and_replays() {
    let fixture = include_str!("fixtures/v0_session.jsonl");
    let records = load_jsonl(fixture).expect("v0 fixture must load");
    assert_eq!(records.len(), 2, "fixture has two records");

    // After load, every record is migrated up to the current version.
    for r in &records {
        assert_eq!(
            r.schema_version, CURRENT_SCHEMA_VERSION,
            "v0 records must be migrated up to the current version on load"
        );
    }

    // The first record is the user prompt; payload survives the migration.
    match &records[0].payload {
        SessionPayload::UserPrompt { text } => {
            assert_eq!(text, "hello from a pre-versioning session");
        }
        other => panic!("expected user prompt, got {other:?}"),
    }

    // Replay reconstructs the user + assistant turns.
    let conv = replay_conversation(&records);
    assert_eq!(
        conv.messages.len(),
        2,
        "replay yields user + assistant turn"
    );
}

#[test]
// A record whose schema_version is ABOVE the current build's version must
// fail loudly with a clear error rather than loading with dropped fields.
fn over_version_record_errors_clearly() {
    let future = CURRENT_SCHEMA_VERSION + 1;
    let line = format!(
        "{{\"schema_version\":{future},\"timestamp\":\"2026-01-01T00:00:00Z\",\
         \"cwd\":\"/tmp\",\"session_id\":\"00000000000000000000000000000000\",\
         \"payload\":{{\"kind\":\"user_prompt\",\"text\":\"from the future\"}}}}"
    );

    let err = load_jsonl(&line).expect_err("a newer-than-current record must error");
    match err {
        SessionError::UnsupportedSchemaVersion { found, supported } => {
            assert_eq!(found, future);
            assert_eq!(supported, CURRENT_SCHEMA_VERSION);
        }
        other => panic!("expected UnsupportedSchemaVersion, got {other:?}"),
    }
    // The error message names the version mismatch.
    let msg = load_jsonl(&line).unwrap_err().to_string();
    assert!(
        msg.contains("newer than this build supports"),
        "error message should be descriptive, got: {msg}"
    );
}

#[test]
// A freshly constructed record is stamped with CURRENT_SCHEMA_VERSION and
// survives a JSONL serialize -> deserialize -> validate round-trip.
fn current_record_round_trips() {
    let rec = SessionRecord::new(
        "2026-01-01T00:00:00Z".to_string(),
        std::path::PathBuf::from("/tmp"),
        "00000000000000000000000000000000".to_string(),
        SessionPayload::UserPrompt {
            text: "hi".to_string(),
        },
    );
    assert_eq!(rec.schema_version, CURRENT_SCHEMA_VERSION);

    let line = serde_json::to_string(&rec).unwrap();
    let loaded = load_jsonl(&line).expect("current record round-trips");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].schema_version, CURRENT_SCHEMA_VERSION);
}

/// A line simulating a FUTURE breaking change: a `schema_version` above the
/// current build AND a payload that cannot be deserialized as today's
/// `SessionRecord` (an unknown `kind` tag). The whole point of versioning is
/// to guard exactly this case — `from_str::<SessionRecord>` FAILS on such a
/// line, so without a pre-parse version gate the resilient loaders would
/// silently SKIP it as "corrupt" instead of erroring loudly.
fn future_unparseable_line() -> String {
    let future = CURRENT_SCHEMA_VERSION + 1;
    format!(
        "{{\"schema_version\":{future},\"timestamp\":\"2026-01-01T00:00:00Z\",\
         \"cwd\":\"/tmp\",\"session_id\":\"00000000000000000000000000000000\",\
         \"payload\":{{\"kind\":\"some_unknown_future_variant\",\
         \"brand_new_field\":42}}}}"
    )
}

#[test]
// KEY REGRESSION: a future-version line whose payload is unparseable as the
// current `SessionRecord` must NOT fall into the "skip corrupt line" path. It
// must hard-error with UnsupportedSchemaVersion. Confirms the pre-parse probe
// fires before the (failing) full deserialize.
fn future_version_unparseable_line_hard_errors_in_load_jsonl() {
    let line = future_unparseable_line();
    // Sanity: the payload really is unparseable as a current SessionRecord, so
    // the only thing standing between us and a silent skip is the pre-parse
    // gate.
    assert!(
        serde_json::from_str::<SessionRecord>(&line).is_err(),
        "fixture must be unparseable as a current SessionRecord"
    );
    let err = load_jsonl(&line).expect_err("future-version line must hard-error");
    assert!(
        matches!(err, SessionError::UnsupportedSchemaVersion { .. }),
        "expected UnsupportedSchemaVersion, got {err:?}"
    );
}

#[tokio::test]
// KEY REGRESSION via the recovery path: writing a future-version,
// unparseable-as-current line to a real session file and loading it with the
// resilient loader must surface UnsupportedSchemaVersion, NOT count it as a
// skipped/corrupt line.
async fn future_version_unparseable_line_hard_errors_in_resilient_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = FsSessionStore::new(dir.path());
    let id = store.create(dir.path()).await.unwrap();
    let path = FsSessionStore::sessions_dir_for(dir.path()).join(format!("{}.jsonl", id.as_str()));
    let mut body = future_unparseable_line();
    body.push('\n');
    std::fs::write(&path, body).unwrap();

    let err = load_records_resilient(dir.path(), &id)
        .await
        .expect_err("resilient load must hard-error on a future-version line");
    assert!(
        matches!(err, SessionError::UnsupportedSchemaVersion { .. }),
        "future-version line must NOT be silently skipped; got {err:?}"
    );
}

#[tokio::test]
// KEY REGRESSION via detect_state/tail_probe: recovery state detection must
// also reject a future-version session loudly rather than silently classifying
// it. The unparseable payload makes the full parse fail, so only the pre-parse
// gate in tail_probe can catch it.
async fn future_version_unparseable_line_hard_errors_in_detect_state() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = FsSessionStore::new(dir.path());
    let id = store.create(dir.path()).await.unwrap();
    let path = FsSessionStore::sessions_dir_for(dir.path()).join(format!("{}.jsonl", id.as_str()));
    let mut body = future_unparseable_line();
    body.push('\n');
    std::fs::write(&path, body).unwrap();

    let err = detect_state(dir.path(), 3600)
        .await
        .expect_err("detect_state must hard-error on a future-version session");
    assert!(
        matches!(err, SessionError::UnsupportedSchemaVersion { .. }),
        "detect_state must not silently classify a future-version session; got {err:?}"
    );
}

#[tokio::test]
// Guard the converse: a genuinely corrupt/truncated line at a KNOWN (current
// or absent) schema version must still be skipped by the resilient loader, not
// promoted to a hard error by the version gate.
async fn corrupt_known_version_line_still_skipped() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = FsSessionStore::new(dir.path());
    let id = store.create(dir.path()).await.unwrap();
    let path = FsSessionStore::sessions_dir_for(dir.path()).join(format!("{}.jsonl", id.as_str()));
    // One valid current-version record plus a truncated line (no schema_version
    // recoverable -> probe defaults to v0, gate passes, full parse fails ->
    // skipped).
    let good = SessionRecord::new(
        "2026-01-01T00:00:00Z".to_string(),
        std::path::PathBuf::from("/tmp"),
        id.to_string(),
        SessionPayload::UserPrompt {
            text: "ok".to_string(),
        },
    );
    let mut body = serde_json::to_string(&good).unwrap();
    body.push('\n');
    body.push_str("{\"timestamp\":\"truncated...");
    std::fs::write(&path, body).unwrap();

    let (recs, skipped) = load_records_resilient(dir.path(), &id)
        .await
        .expect("corrupt known-version line must not hard-error");
    assert_eq!(recs.len(), 1, "the valid record loads");
    assert_eq!(skipped, 1, "the truncated line is skipped, not errored");
}
