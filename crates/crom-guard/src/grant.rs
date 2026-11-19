use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::decision::DenyReason;
use crate::ids::{ActionId, AgentId, AppId, GrantId, RunId, TenantId, UserId};
use crate::time::Timestamp;
use crate::value::{AttrValue, Attrs, EntityUid};

/// Grant record schema version written by this crate.
pub const GRANT_SCHEMA_VERSION: u32 = 1;

/// The delegating user, as asserted by the trusted application. Becomes the
/// Cedar `User` entity referenced by `principal.delegator`; `tenant` is the
/// grant's tenant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delegator {
    pub id: UserId,
    pub role: String,
    /// Further Cedar attributes, e.g. `refund_limit_cents`.
    #[serde(default)]
    pub attrs: Attrs,
}

/// One grant constraint. Narrowing is defined per kind: a child [`Set`]
/// must be a subset, a child [`AtMost`] must be less than or equal.
///
/// [`Set`]: Constraint::Set
/// [`AtMost`]: Constraint::AtMost
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    Set(BTreeSet<EntityUid>),
    AtMost(i64),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConstraintError {
    #[error("constraint name {0:?} is not an identifier")]
    InvalidName(String),
    #[error("constraint {name:?} is missing from the child grant")]
    Missing { name: String },
    #[error("constraint {name:?} changes kind")]
    KindMismatch { name: String },
    #[error("constraint {name:?} is wider than the parent's")]
    Wider { name: String },
}

/// Typed grant constraints, keyed by name. They become the `context.grant`
/// record in Cedar (ADR 0003 §4.2), so names must be Cedar identifiers.
/// Which names an action needs is fixed by the operator's schema, which
/// strict validation enforces per request.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<String, Constraint>",
    into = "BTreeMap<String, Constraint>"
)]
pub struct Constraints(BTreeMap<String, Constraint>);

impl Constraints {
    pub fn new(map: BTreeMap<String, Constraint>) -> Result<Self, ConstraintError> {
        for name in map.keys() {
            let mut c = name.chars();
            let ok = matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
                && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if !ok {
                return Err(ConstraintError::InvalidName(name.clone()));
            }
        }
        Ok(Self(map))
    }

    pub fn get(&self, name: &str) -> Option<&Constraint> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Constraint)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// `Ok` when `self` (a child) is narrower than or equal to `parent`:
    /// every parent constraint is present with the same kind and is not
    /// widened. A child may add constraints the parent lacks.
    ///
    /// This is the early check for `delegate` (#353). Enforcement does not
    /// rely on it: every grant in a chain is evaluated independently.
    pub fn narrows(&self, parent: &Constraints) -> Result<(), ConstraintError> {
        for (name, p) in &parent.0 {
            let c = self
                .0
                .get(name)
                .ok_or_else(|| ConstraintError::Missing { name: name.clone() })?;
            let ok = match (c, p) {
                (Constraint::Set(c), Constraint::Set(p)) => c.is_subset(p),
                (Constraint::AtMost(c), Constraint::AtMost(p)) => c <= p,
                _ => {
                    return Err(ConstraintError::KindMismatch { name: name.clone() });
                }
            };
            if !ok {
                return Err(ConstraintError::Wider { name: name.clone() });
            }
        }
        Ok(())
    }

    /// The `context.grant` record: sets become sets of entities, limits
    /// become `Long`s.
    pub fn to_context_record(&self) -> Attrs {
        self.0
            .iter()
            .map(|(k, v)| {
                let value = match v {
                    Constraint::Set(s) => {
                        AttrValue::Set(s.iter().cloned().map(AttrValue::Entity).collect())
                    }
                    Constraint::AtMost(n) => AttrValue::Long(*n),
                };
                (k.clone(), value)
            })
            .collect()
    }
}

impl TryFrom<BTreeMap<String, Constraint>> for Constraints {
    type Error = ConstraintError;
    fn try_from(map: BTreeMap<String, Constraint>) -> Result<Self, Self::Error> {
        Self::new(map)
    }
}

impl From<Constraints> for BTreeMap<String, Constraint> {
    fn from(c: Constraints) -> Self {
        c.0
    }
}

/// What the trusted application submits to issue a root grant. Guard
/// assigns the grant id, agent id, issuer, and validity window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantSpec {
    pub tenant: TenantId,
    pub delegator: Delegator,
    pub run_id: RunId,
    pub actions: BTreeSet<ActionId>,
    #[serde(default)]
    pub constraints: Constraints,
    /// Lifetime from issuance. The store caps it at its configured maximum.
    pub ttl_seconds: u64,
    /// Remaining delegation depth. 0 (the default) forbids `delegate`.
    #[serde(default)]
    pub max_depth: u8,
}

/// A stored grant (MILESTONE_18 §8.2). Immutable after issuance except for
/// revocation, so fields are private.
///
/// A `Grant` value is data, not authority: deserializing one grants
/// nothing. Authority exists only as an [`ExecutionContext`](crate::ExecutionContext),
/// which only the grant store builds from an authenticated session handle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    grant_id: GrantId,
    schema_version: u32,
    issuer: AppId,
    tenant: TenantId,
    delegator: Delegator,
    agent: AgentId,
    run_id: RunId,
    actions: BTreeSet<ActionId>,
    constraints: Constraints,
    not_before: Timestamp,
    expires_at: Timestamp,
    parent: Option<GrantId>,
    max_depth: u8,
    revoked_at: Option<Timestamp>,
}

/// Guard-assigned values for a new grant; the store generates the ids.
#[allow(dead_code)] // used by the grant store (#348) and `delegate` (#353)
pub(crate) struct GrantIssue {
    pub grant_id: GrantId,
    pub issuer: AppId,
    pub agent: AgentId,
    pub not_before: Timestamp,
    pub expires_at: Timestamp,
    pub parent: Option<GrantId>,
}

impl Grant {
    #[allow(dead_code)] // used by the grant store (#348) and `delegate` (#353)
    pub(crate) fn issue(spec: GrantSpec, issue: GrantIssue) -> Self {
        Self {
            grant_id: issue.grant_id,
            schema_version: GRANT_SCHEMA_VERSION,
            issuer: issue.issuer,
            tenant: spec.tenant,
            delegator: spec.delegator,
            agent: issue.agent,
            run_id: spec.run_id,
            actions: spec.actions,
            constraints: spec.constraints,
            not_before: issue.not_before,
            expires_at: issue.expires_at,
            parent: issue.parent,
            max_depth: spec.max_depth,
            revoked_at: None,
        }
    }

    #[allow(dead_code)] // used by the grant store (#348)
    pub(crate) fn revoke(&mut self, at: Timestamp) {
        self.revoked_at.get_or_insert(at);
    }

    /// Checks this grant alone (not its ancestors) against `now`.
    pub fn check_live(&self, now: Timestamp) -> Result<(), DenyReason> {
        // Any recorded revocation denies, whatever its timestamp (fail closed).
        if self.revoked_at.is_some() {
            return Err(DenyReason::GrantRevoked);
        }
        if now < self.not_before {
            return Err(DenyReason::GrantNotYetValid);
        }
        if now >= self.expires_at {
            return Err(DenyReason::GrantExpired);
        }
        Ok(())
    }

    pub fn grant_id(&self) -> &GrantId {
        &self.grant_id
    }
    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }
    pub fn issuer(&self) -> &AppId {
        &self.issuer
    }
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }
    pub fn delegator(&self) -> &Delegator {
        &self.delegator
    }
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
    pub fn actions(&self) -> &BTreeSet<ActionId> {
        &self.actions
    }
    pub fn constraints(&self) -> &Constraints {
        &self.constraints
    }
    pub fn not_before(&self) -> Timestamp {
        self.not_before
    }
    pub fn expires_at(&self) -> Timestamp {
        self.expires_at
    }
    pub fn parent(&self) -> Option<&GrantId> {
        self.parent.as_ref()
    }
    pub fn max_depth(&self) -> u8 {
        self.max_depth
    }
    pub fn revoked_at(&self) -> Option<Timestamp> {
        self.revoked_at
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn uid(id: &str) -> EntityUid {
        EntityUid::new("Support::Customer", id).unwrap()
    }

    fn set(ids: &[&str]) -> Constraint {
        Constraint::Set(ids.iter().map(|i| uid(i)).collect())
    }

    fn cons(pairs: Vec<(&str, Constraint)>) -> Constraints {
        Constraints::new(pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()).unwrap()
    }

    pub(crate) fn spec() -> GrantSpec {
        GrantSpec {
            tenant: TenantId::new("acme").unwrap(),
            delegator: Delegator {
                id: UserId::new("acme-agent-operator").unwrap(),
                role: "agent-operator".into(),
                attrs: [("refund_limit_cents".to_owned(), AttrValue::Long(5_000))].into(),
            },
            run_id: RunId::new("run-1").unwrap(),
            actions: [ActionId::new("customer.read").unwrap()].into(),
            constraints: cons(vec![
                ("customers", set(&["acme-cus-001", "acme-cus-002"])),
                ("max_refund_cents", Constraint::AtMost(10_000)),
            ]),
            ttl_seconds: 600,
            max_depth: 1,
        }
    }

    pub(crate) fn grant(id: &str, parent: Option<&str>, spec: GrantSpec) -> Grant {
        Grant::issue(
            spec,
            GrantIssue {
                grant_id: GrantId::new(id).unwrap(),
                issuer: AppId::new("support-app").unwrap(),
                agent: AgentId::new(format!("agent-{id}")).unwrap(),
                not_before: Timestamp::from_unix_millis(1_000),
                expires_at: Timestamp::from_unix_millis(1_000).saturating_add_secs(600),
                parent: parent.map(|p| GrantId::new(p).unwrap()),
            },
        )
    }

    /// The JSON a trusted app submits round-trips through `GrantSpec`, and unknown fields are rejected.
    #[test]
    fn grant_spec_json_round_trip_and_strict() {
        let json = serde_json::json!({
            "tenant": "acme",
            "delegator": {"id": "acme-agent-operator", "role": "agent-operator",
                          "attrs": {"refund_limit_cents": {"long": 5000}}},
            "run_id": "run-1",
            "actions": ["customer.read"],
            "constraints": {
                "customers": {"set": [
                    {"type": "Support::Customer", "id": "acme-cus-001"},
                    {"type": "Support::Customer", "id": "acme-cus-002"}]},
                "max_refund_cents": {"at_most": 10000}
            },
            "ttl_seconds": 600,
            "max_depth": 1
        });
        let parsed: GrantSpec = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(parsed, spec());
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);

        let mut extra = json.clone();
        extra["agent"] = "agent-x".into();
        assert!(serde_json::from_value::<GrantSpec>(extra).is_err());
        let mut bad_tenant = json.clone();
        bad_tenant["tenant"] = "acme corp".into();
        assert!(serde_json::from_value::<GrantSpec>(bad_tenant).is_err());
        let mut bad_constraint = json;
        bad_constraint["constraints"]["max-refund"] = serde_json::json!({"at_most": 1});
        assert!(serde_json::from_value::<GrantSpec>(bad_constraint).is_err());
    }

    /// A stored grant round-trips through serde unchanged and rejects unknown fields.
    #[test]
    fn grant_record_round_trip() {
        let g = grant("g1", None, spec());
        let v = serde_json::to_value(&g).unwrap();
        assert_eq!(v["schema_version"], 1);
        assert_eq!(serde_json::from_value::<Grant>(v.clone()).unwrap(), g);
        let mut extra = v;
        extra["admin"] = true.into();
        assert!(serde_json::from_value::<Grant>(extra).is_err());
    }

    /// Narrowing accepts subsets, lower limits, and added constraints; it rejects wider, missing, or re-kinded ones.
    #[test]
    fn constraint_narrowing() {
        let parent = spec().constraints;
        assert!(parent.narrows(&parent).is_ok());
        let narrower = cons(vec![
            ("customers", set(&["acme-cus-001"])),
            ("max_refund_cents", Constraint::AtMost(500)),
            ("extra", Constraint::AtMost(1)),
        ]);
        assert!(narrower.narrows(&parent).is_ok());

        let wider_set = cons(vec![
            ("customers", set(&["acme-cus-001", "acme-cus-009"])),
            ("max_refund_cents", Constraint::AtMost(500)),
        ]);
        assert!(matches!(
            wider_set.narrows(&parent),
            Err(ConstraintError::Wider { .. })
        ));
        let wider_limit = cons(vec![
            ("customers", set(&["acme-cus-001"])),
            ("max_refund_cents", Constraint::AtMost(10_001)),
        ]);
        assert!(matches!(
            wider_limit.narrows(&parent),
            Err(ConstraintError::Wider { .. })
        ));
        let missing = cons(vec![("customers", set(&["acme-cus-001"]))]);
        assert!(matches!(
            missing.narrows(&parent),
            Err(ConstraintError::Missing { .. })
        ));
        let rekinded = cons(vec![
            ("customers", Constraint::AtMost(0)),
            ("max_refund_cents", Constraint::AtMost(0)),
        ]);
        assert!(matches!(
            rekinded.narrows(&parent),
            Err(ConstraintError::KindMismatch { .. })
        ));
    }

    /// Constraints map to the Cedar `context.grant` record: sets of entities and longs.
    #[test]
    fn constraints_to_context_record() {
        let rec = spec().constraints.to_context_record();
        assert_eq!(rec["max_refund_cents"], AttrValue::Long(10_000));
        match &rec["customers"] {
            AttrValue::Set(s) => {
                assert_eq!(s.len(), 2);
                assert!(s.contains(&AttrValue::Entity(uid("acme-cus-001"))));
            }
            other => panic!("{other:?}"),
        }
    }

    /// A grant is live only in [not_before, expires_at) and never once revoked, whatever the revocation time.
    #[test]
    fn grant_liveness_window() {
        let mut g = grant("g1", None, spec());
        let t = Timestamp::from_unix_millis;
        assert_eq!(g.check_live(t(999)), Err(DenyReason::GrantNotYetValid));
        assert_eq!(g.check_live(t(1_000)), Ok(()));
        assert_eq!(g.check_live(t(600_999)), Ok(()));
        assert_eq!(g.check_live(t(601_000)), Err(DenyReason::GrantExpired));
        g.revoke(t(5_000));
        g.revoke(t(9_000)); // first revocation time is kept
        assert_eq!(g.revoked_at(), Some(t(5_000)));
        assert_eq!(g.check_live(t(4_999)), Err(DenyReason::GrantRevoked));
        assert_eq!(g.check_live(t(5_000)), Err(DenyReason::GrantRevoked));
    }
}
