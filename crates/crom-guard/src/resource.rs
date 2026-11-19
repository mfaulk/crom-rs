use std::collections::BTreeSet;

use serde::Serialize;

use crate::value::{AttrValue, Attrs, EntityUid};

/// Opaque resource version used as the executor's precondition, e.g. the
/// business API's `ETag`. A change between authorize and execute fails the
/// downstream call (MILESTONE_18 §10.2 step 6).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ResourceVersion(String);

impl ResourceVersion {
    pub fn new(v: impl Into<String>) -> Self {
        Self(v.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A resource as the trusted resolver found it in application data. The
/// resolver is trusted code (ADR 0002 §2.5), so this type is publicly
/// constructible; it must be built from application data, never from
/// argument values other than the identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedResource {
    uid: EntityUid,
    attrs: Attrs,
    /// Cedar parents, for `resource in ...` policies.
    parents: BTreeSet<EntityUid>,
    version: ResourceVersion,
}

impl ResolvedResource {
    pub fn new(uid: EntityUid, version: ResourceVersion) -> Self {
        Self {
            uid,
            attrs: Attrs::new(),
            parents: BTreeSet::new(),
            version,
        }
    }

    pub fn with_attr(mut self, name: impl Into<String>, value: AttrValue) -> Self {
        self.attrs.insert(name.into(), value);
        self
    }

    pub fn with_parent(mut self, parent: EntityUid) -> Self {
        self.parents.insert(parent);
        self
    }

    pub fn uid(&self) -> &EntityUid {
        &self.uid
    }
    pub fn attrs(&self) -> &Attrs {
        &self.attrs
    }
    pub fn parents(&self) -> &BTreeSet<EntityUid> {
        &self.parents
    }
    pub fn version(&self) -> &ResourceVersion {
        &self.version
    }
}
