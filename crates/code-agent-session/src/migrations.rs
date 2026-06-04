//! Version-to-version migration framework for [`SessionRecord`].
//!
//! On-disk JSONL is versioned by `SessionRecord::schema_version` (see
//! [`crate::CURRENT_SCHEMA_VERSION`]). When the schema evolves, a migration
//! upgrades a record from its stored version up to the current version so the
//! rest of the codebase only ever handles current-shaped records.
//!
//! Today no field-level migrations exist: v0 (pre-versioning) and v1 records
//! are structurally identical, so [`migrate_record`] is an identity for known
//! versions. The structure — a single entry point that walks a record forward
//! one version at a time — is the deliverable; real migrations slot in as the
//! schema changes.

use crate::{SessionError, SessionRecord, CURRENT_SCHEMA_VERSION};

/// Migrate a record from `from_version` up to [`CURRENT_SCHEMA_VERSION`].
///
/// `from_version` is the record's stored `schema_version` (callers pass the
/// version as read from disk, before any in-memory rewrites). The returned
/// record carries `schema_version == CURRENT_SCHEMA_VERSION`.
///
/// Versions newer than the current build are rejected up front with
/// [`SessionError::UnsupportedSchemaVersion`]; this is normally caught earlier
/// by [`SessionRecord::validate_schema_version`], but is enforced here too so
/// the function is safe to call directly.
pub fn migrate_record(
    mut record: SessionRecord,
    from_version: u32,
) -> Result<SessionRecord, SessionError> {
    if from_version > CURRENT_SCHEMA_VERSION {
        return Err(SessionError::UnsupportedSchemaVersion {
            found: from_version,
            supported: CURRENT_SCHEMA_VERSION,
        });
    }

    let mut version = from_version;
    while version < CURRENT_SCHEMA_VERSION {
        record = migrate_one(record, version)?;
        version += 1;
    }

    // Stamp the record as current so downstream code sees a uniform version.
    record.schema_version = CURRENT_SCHEMA_VERSION;
    Ok(record)
}

/// Apply the single step that upgrades a record from `version` to
/// `version + 1`. New migrations are added as `match` arms here.
fn migrate_one(record: SessionRecord, version: u32) -> Result<SessionRecord, SessionError> {
    match version {
        // v0 (pre-versioning) -> v1: no structural change; the only difference
        // is the presence of the `schema_version` field itself, which is
        // already defaulted in on load.
        0 => Ok(record),
        // Unknown intermediate version: refuse rather than guess.
        other => Err(SessionError::Other(format!(
            "no migration registered from schema version {other} to {}",
            other + 1
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SessionPayload, CURRENT_SCHEMA_VERSION};
    use std::path::PathBuf;

    fn sample(version: u32) -> SessionRecord {
        SessionRecord {
            schema_version: version,
            timestamp: "1970-01-01T00:00:00Z".to_string(),
            cwd: PathBuf::from("/tmp"),
            session_id: "s".to_string(),
            payload: SessionPayload::UserPrompt {
                text: "hi".to_string(),
            },
        }
    }

    #[test]
    // Migrating a v0 record yields a current-version record unchanged in
    // payload.
    fn migrate_v0_is_identity_to_current() {
        let rec = migrate_record(sample(0), 0).unwrap();
        assert_eq!(rec.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(matches!(rec.payload, SessionPayload::UserPrompt { .. }));
    }

    #[test]
    // Migrating a current-version record is a no-op.
    fn migrate_current_is_identity() {
        let rec = migrate_record(sample(CURRENT_SCHEMA_VERSION), CURRENT_SCHEMA_VERSION).unwrap();
        assert_eq!(rec.schema_version, CURRENT_SCHEMA_VERSION);
    }

    #[test]
    // A future version is rejected rather than silently accepted.
    fn migrate_future_version_errors() {
        let err = migrate_record(
            sample(CURRENT_SCHEMA_VERSION + 5),
            CURRENT_SCHEMA_VERSION + 5,
        )
        .expect_err("future version must error");
        assert!(matches!(err, SessionError::UnsupportedSchemaVersion { .. }));
    }
}
