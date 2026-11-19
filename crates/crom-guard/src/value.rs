use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Maximum entity id length in bytes.
pub const MAX_ENTITY_ID_LEN: usize = 256;

/// A Cedar entity reference, e.g. `Support::Invoice` / `acme-inv-001`.
/// Validated on construction and on deserialization. Build Cedar UIDs from
/// [`entity_type`](Self::entity_type) and [`id`](Self::id); never parse the
/// `Display` output, which is for humans and logs only.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "RawEntityUid")]
pub struct EntityUid {
    /// Namespaced type name, e.g. `Support::Invoice`.
    #[serde(rename = "type")]
    entity_type: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntityUid {
    #[serde(rename = "type")]
    entity_type: String,
    id: String,
}

impl TryFrom<RawEntityUid> for EntityUid {
    type Error = EntityUidError;
    fn try_from(r: RawEntityUid) -> Result<Self, EntityUidError> {
        Self::new(r.entity_type, r.id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntityUidError {
    #[error("entity type {0:?} is not a `::`-separated list of identifiers")]
    InvalidType(String),
    #[error("entity id is empty")]
    EmptyId,
    #[error("entity id is longer than {MAX_ENTITY_ID_LEN} bytes")]
    IdTooLong,
    #[error("entity id contains a control character")]
    IdControlChar,
}

impl EntityUid {
    pub fn new(
        entity_type: impl Into<String>,
        id: impl Into<String>,
    ) -> Result<Self, EntityUidError> {
        let entity_type = entity_type.into();
        let id = id.into();
        let ident_ok = |s: &str| {
            let mut c = s.chars();
            matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
                && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        };
        if !entity_type.split("::").all(ident_ok) {
            return Err(EntityUidError::InvalidType(entity_type));
        }
        if id.is_empty() {
            return Err(EntityUidError::EmptyId);
        }
        if id.len() > MAX_ENTITY_ID_LEN {
            return Err(EntityUidError::IdTooLong);
        }
        if id.chars().any(char::is_control) {
            return Err(EntityUidError::IdControlChar);
        }
        Ok(Self { entity_type, id })
    }

    pub fn entity_type(&self) -> &str {
        &self.entity_type
    }

    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for EntityUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}::{:?}", self.entity_type, self.id)
    }
}

/// An attribute or context value, restricted to the Cedar types Guard uses.
/// Serialized externally tagged so the type is never inferred from JSON shape.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttrValue {
    Bool(bool),
    Long(i64),
    String(String),
    Entity(EntityUid),
    Set(BTreeSet<AttrValue>),
    Record(BTreeMap<String, AttrValue>),
}

/// Named attributes of an entity or fields of a context record.
pub type Attrs = BTreeMap<String, AttrValue>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialization applies the same validation as `EntityUid::new`, including for nested values.
    #[test]
    fn entity_uid_deserialize_validates() {
        let ok: EntityUid = serde_json::from_value(
            serde_json::json!({"type": "Support::Customer", "id": "acme-cus-001"}),
        )
        .unwrap();
        assert_eq!(
            ok,
            EntityUid::new("Support::Customer", "acme-cus-001").unwrap()
        );
        for bad in [
            serde_json::json!({"type": "Support::Customer::\"x\" || true", "id": "a"}),
            serde_json::json!({"type": "Support::Customer", "id": ""}),
            serde_json::json!({"type": "Support::Customer", "id": "a\nb"}),
            serde_json::json!({"type": "Support::Customer", "id": "a".repeat(MAX_ENTITY_ID_LEN + 1)}),
            serde_json::json!({"type": "", "id": "a"}),
            serde_json::json!({"type": "Support::", "id": "a"}),
            serde_json::json!({"type": "Support::Customer", "id": "a", "extra": 1}),
        ] {
            assert!(
                serde_json::from_value::<EntityUid>(bad.clone()).is_err(),
                "{bad}"
            );
            let nested = serde_json::json!({"entity": bad});
            assert!(serde_json::from_value::<AttrValue>(nested).is_err());
        }
    }
}
