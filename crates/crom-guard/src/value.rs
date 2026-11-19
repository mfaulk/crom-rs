use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// A Cedar entity reference, e.g. `Support::Invoice::"acme-inv-001"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityUid {
    /// Namespaced type name, e.g. `Support::Invoice`.
    #[serde(rename = "type")]
    entity_type: String,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntityUidError {
    #[error("entity type {0:?} is not a `::`-separated list of identifiers")]
    InvalidType(String),
    #[error("entity id is empty")]
    EmptyId,
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
