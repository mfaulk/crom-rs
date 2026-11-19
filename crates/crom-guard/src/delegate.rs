//! `guard.delegate` (#353, MILESTONE_18 §8.3): the only agent-reachable
//! write. Authorized by the narrowing rules in `check`, before the decision
//! record, and enforced again by `GrantStore::delegate` under its lock. Not
//! evaluated by Cedar: see `docs/guard/grants.md` ("Why not Cedar").
//!
//! The returned child handle is a secret. It goes only into the
//! agent-visible `content`; the audit outcome carries the child grant id.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::action::ValidatedArgs;
use crate::context::ExecutionContext;
use crate::decision::DenyReason;
use crate::grant::{Constraint, Constraints, DelegationSpec, Grant};
use crate::ids::{ActionId, MAX_ID_LEN};
use crate::registry::BuiltinOperation;
use crate::store::check_narrowing;
use crate::time::Timestamp;
use crate::traits::{BoxFuture, ExecutionOutput, GrantStore, GrantStoreError};
use crate::value::EntityUid;

/// The tool name of Guard's delegate operation. Grants must list it in
/// `actions` to call it.
pub const DELEGATE_ACTION: &str = "guard.delegate";

/// Bounds on agent input, so one call cannot build an outsized grant.
const MAX_ACTIONS: u64 = 64;
const MAX_CONSTRAINTS: u64 = 32;
const MAX_SET_MEMBERS: u64 = 1024;

pub(crate) struct Delegate {
    action: ActionId,
    schema: Value,
}

impl Delegate {
    pub(crate) fn new() -> Self {
        let id = |max: usize| json!({"type": "string", "minLength": 1, "maxLength": max});
        let entity = json!({
            "type": "object",
            "properties": {"type": id(256), "id": id(256)},
            "required": ["type", "id"],
            "additionalProperties": false
        });
        let constraint = json!({
            "type": "object",
            "description": "Exactly one of `set` or `at_most`. Replaces the parent's constraint of the same name; parent constraints not listed are inherited unchanged.",
            "properties": {
                "name": {"type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$", "maxLength": 64},
                "set": {"type": "array", "items": entity, "maxItems": MAX_SET_MEMBERS},
                "at_most": {"type": "integer"}
            },
            "required": ["name"],
            "additionalProperties": false
        });
        Self {
            action: ActionId::new(DELEGATE_ACTION).expect("valid action id"),
            schema: json!({
                "type": "object",
                "properties": {
                    "actions": {
                        "type": "array",
                        "items": id(MAX_ID_LEN),
                        "minItems": 1,
                        "maxItems": MAX_ACTIONS,
                        "uniqueItems": true
                    },
                    "constraints": {"type": "array", "items": constraint, "maxItems": MAX_CONSTRAINTS},
                    "ttl_seconds": {"type": "integer", "minimum": 1},
                    "max_depth": {"type": "integer", "minimum": 0, "maximum": 255}
                },
                "required": ["actions", "ttl_seconds"],
                "additionalProperties": false
            }),
        }
    }
}

/// Builds the child spec from validated arguments. Listed constraints
/// replace the parent's of the same name; the rest are inherited. Malformed
/// input is `InvalidArguments`; whether the result narrows is `check`'s job.
fn parse(parent: &Grant, args: &ValidatedArgs) -> Result<DelegationSpec, DenyReason> {
    const BAD: DenyReason = DenyReason::InvalidArguments;
    let obj = args.as_json().as_object().ok_or(BAD)?;
    let actions = obj
        .get("actions")
        .and_then(Value::as_array)
        .ok_or(BAD)?
        .iter()
        .map(|a| a.as_str().and_then(|s| ActionId::new(s).ok()).ok_or(BAD))
        .collect::<Result<_, _>>()?;
    let mut constraints: BTreeMap<String, Constraint> = parent.constraints().clone().into();
    let mut named = std::collections::BTreeSet::new();
    for c in obj
        .get("constraints")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let name = c.get("name").and_then(Value::as_str).ok_or(BAD)?;
        if !named.insert(name) {
            return Err(BAD);
        }
        let value = match (c.get("set"), c.get("at_most")) {
            (Some(set), None) => Constraint::Set(
                serde_json::from_value::<Vec<EntityUid>>(set.clone())
                    .map_err(|_| BAD)?
                    .into_iter()
                    .collect(),
            ),
            (None, Some(n)) => Constraint::AtMost(n.as_i64().ok_or(BAD)?),
            _ => return Err(BAD),
        };
        constraints.insert(name.to_owned(), value);
    }
    let max_depth = match obj.get("max_depth") {
        None => 0,
        Some(d) => d.as_u64().and_then(|d| u8::try_from(d).ok()).ok_or(BAD)?,
    };
    Ok(DelegationSpec {
        actions,
        constraints: Constraints::new(constraints).map_err(|_| BAD)?,
        ttl_seconds: obj.get("ttl_seconds").and_then(Value::as_u64).ok_or(BAD)?,
        max_depth,
    })
}

/// The audit reason for a store refusal after the `allow` record. Nothing
/// was created in any case. A parent that is no longer live (a revoke that
/// raced this call) keeps its own reason; a per-grant, per-root, or tenant
/// limit is `PolicyDeny`; a full or failing store is `EngineUnavailable`.
async fn classify(
    ctx: &ExecutionContext,
    store: &dyn GrantStore,
    e: &GrantStoreError,
    now: Timestamp,
) -> DenyReason {
    if let Err(reason) = store.recheck(ctx, now).await {
        return reason;
    }
    match e {
        GrantStoreError::InvalidSpec(_) => DenyReason::PolicyDeny,
        GrantStoreError::UnknownGrant | GrantStoreError::Unavailable => {
            DenyReason::EngineUnavailable
        }
    }
}

impl BuiltinOperation for Delegate {
    fn action(&self) -> &ActionId {
        &self.action
    }

    fn description(&self) -> &str {
        "Create a session handle for a subagent with a subset of this session's authority: \
         fewer or equal actions, narrower or equal constraints, an expiry no later than this \
         grant's, and a lower max_depth. Any widening is denied."
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn logged_fields(&self) -> &[&'static str] {
        &["actions", "constraints", "ttl_seconds", "max_depth"]
    }

    fn check(
        &self,
        ctx: &ExecutionContext,
        args: &ValidatedArgs,
        now: Timestamp,
    ) -> Result<(), DenyReason> {
        let spec = parse(ctx.leaf(), args)?;
        check_narrowing(ctx.leaf(), &spec, now)
            .map(|_| ())
            .map_err(|_| DenyReason::PolicyDeny)
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ExecutionContext,
        store: &'a dyn GrantStore,
        args: &'a ValidatedArgs,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ExecutionOutput, DenyReason>> {
        Box::pin(async move {
            let spec = parse(ctx.leaf(), args)?;
            // The store re-reads the parent and applies the same rules under
            // its write lock, so `check` racing a revoke cannot widen.
            let issued = match store.delegate(ctx, spec, now).await {
                Ok(i) => i,
                Err(e) => return Err(classify(ctx, store, &e, now).await),
            };
            let g = &issued.grant;
            Ok(ExecutionOutput {
                content: json!({
                    "handle": issued.handle.expose_secret(),
                    "grant_id": g.grant_id(),
                    "actions": g.actions(),
                    "constraints": g.constraints(),
                    "expires_at": g.expires_at(),
                    "max_depth": g.max_depth(),
                }),
                downstream_ref: Some(g.grant_id().as_str().to_owned()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use futures::executor::block_on;

    use super::*;
    use crate::grant::tests::spec;
    use crate::grant::GrantSpec;
    use crate::ids::AppId;
    use crate::store::{GrantStoreConfig, MemoryGrantStore};

    /// A revoke between `check` and the store write is reported as `GrantRevoked` (blocked), and creates no child.
    #[test]
    fn revoke_racing_run_is_blocked_as_revoked() {
        let op = Delegate::new();
        let mut actions = spec().actions;
        actions.insert(op.action.clone());
        let store = MemoryGrantStore::new(GrantStoreConfig::new(actions.clone()));
        let app = AppId::new("support-app").unwrap();
        let now = Timestamp::from_unix_millis(5_000);
        let parent = block_on(store.issue(&app, GrantSpec { actions, ..spec() }, now)).unwrap();
        let ctx = block_on(store.authenticate(&parent.handle, now)).unwrap();
        let args = ValidatedArgs::new(json!({"actions": ["customer.read"], "ttl_seconds": 60}));
        assert_eq!(op.check(&ctx, &args, now), Ok(()));
        block_on(store.revoke(&app, parent.grant.grant_id(), now)).unwrap();
        let out = block_on(op.run(&ctx, &store, &args, now));
        assert_eq!(out.err(), Some(DenyReason::GrantRevoked));
    }
}
