use crate::action::{AuthorizedAction, ValidatedArgs};
use crate::ids::{DecisionId, TenantId};
use crate::traits::ExecuteError;

/// The downstream call an executor may make for one [`AuthorizedAction`]
/// (MILESTONE_18 §10.2 step 6, #350). The version-condition contract:
///
/// 1. **Exact resource.** Target [`resource_id`](Self::resource_id) of the
///    resolved type, never an identifier re-read from the arguments.
/// 2. **Tenant-scoped.** Address it under [`tenant`](Self::tenant), the
///    principal's tenant from the grant record, so the downstream API's own
///    tenant check applies.
/// 3. **Version-conditioned.** Send [`if_match`](Self::if_match) (a strong
///    entity-tag of the resolved version) as `If-Match`, so the downstream
///    performs the effect only if the resource is unchanged since it was
///    authorized. Any change that could alter the decision (owner,
///    amounts, tenant) must change the version.
/// 4. **Correlated.** Send [`decision_id`](Self::decision_id) as
///    `X-Guard-Decision-Id`.
/// 5. **Fail closed.** Map the response with
///    [`check_status`](Self::check_status): `412` and `404` mean the bound
///    resource changed or left the tenant, reported as
///    [`ExecuteError::Precondition`]; the downstream must not have
///    performed the effect in either case.
///
/// The credential comes from the `CredentialLease` passed to
/// `ProtectedOperation::execute`.
#[derive(Debug)]
pub struct ConditionalCall<'a> {
    action: &'a AuthorizedAction,
    if_match: String,
}

impl<'a> ConditionalCall<'a> {
    /// Fails if the bound resource is not of `entity_type` (an operation
    /// wired to the wrong resolver) or its version cannot be sent as an
    /// entity-tag; both are reported as [`ExecuteError::Failed`] before any
    /// downstream call.
    pub fn new(action: &'a AuthorizedAction, entity_type: &str) -> Result<Self, ExecuteError> {
        let uid = action.resource();
        if uid.entity_type() != entity_type {
            return Err(ExecuteError::Failed(format!(
                "operation expects a {entity_type} resource, bound to {}",
                uid.entity_type()
            )));
        }
        let v = action.resource_version().as_str();
        // RFC 9110 etagc: %x21 / %x23-7E. Rejects quotes, whitespace, and
        // control characters, so the header cannot be split or widened.
        if v.is_empty() || !v.bytes().all(|b| b == 0x21 || (0x23..=0x7e).contains(&b)) {
            return Err(ExecuteError::Failed(
                "resource version is not a valid entity-tag".into(),
            ));
        }
        Ok(Self {
            action,
            if_match: format!("\"{v}\""),
        })
    }

    pub fn tenant(&self) -> &TenantId {
        self.action.principal().tenant()
    }

    pub fn resource_id(&self) -> &str {
        self.action.resource().id()
    }

    /// The `If-Match` header value, e.g. `"3"`.
    pub fn if_match(&self) -> &str {
        &self.if_match
    }

    pub fn decision_id(&self) -> &DecisionId {
        self.action.decision_id()
    }

    /// Non-identifier fields (e.g. `amount_cents`) come from here.
    pub fn args(&self) -> &ValidatedArgs {
        self.action.args()
    }

    /// Maps a downstream HTTP status: 2xx succeeds; `412 Precondition
    /// Failed` and `404 Not Found` (the resource changed or is no longer in
    /// the tenant) are [`ExecuteError::Precondition`]; anything else,
    /// including `428 Precondition Required`, is [`ExecuteError::Failed`].
    pub fn check_status(status: u16) -> Result<(), ExecuteError> {
        match status {
            200..=299 => Ok(()),
            404 | 412 => Err(ExecuteError::Precondition),
            s => Err(ExecuteError::Failed(format!("downstream status {s}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    use super::*;
    use crate::action::{AuthorizationContext, AuthorizationRequest};
    use crate::context::ExecutionContext;
    use crate::decision::{Decision, DenyReason};
    use crate::grant::tests::{grant, spec};
    use crate::ids::ActionId;
    use crate::resource::{ResolvedResource, ResourceVersion};
    use crate::traits::{BoxFuture, ResolveError, ResourceResolver};
    use crate::value::{AttrValue, EntityUid};

    /// In-memory application data: invoice id → (tenant, customer, version).
    #[derive(Default)]
    struct Db {
        rows: Mutex<BTreeMap<String, (String, String, u64)>>,
        effects: Mutex<Vec<String>>,
    }

    impl Db {
        fn with(id: &str, tenant: &str, customer: &str) -> Self {
            let db = Self::default();
            db.rows
                .lock()
                .unwrap()
                .insert(id.into(), (tenant.into(), customer.into(), 1));
            db
        }

        /// A downstream that honours `If-Match` and tenant-scoped paths.
        fn refund(&self, tenant: &str, id: &str, if_match: &str) -> u16 {
            match self.rows.lock().unwrap().get(id) {
                Some((t, _, _)) if t != tenant => 404,
                None => 404,
                Some((_, _, v)) if format!("\"{v}\"") != if_match => 412,
                Some(_) => {
                    self.effects.lock().unwrap().push(id.into());
                    201
                }
            }
        }

        fn reassign(&self, id: &str, customer: &str) {
            let mut rows = self.rows.lock().unwrap();
            let row = rows.get_mut(id).unwrap();
            row.1 = customer.into();
            row.2 += 1;
        }
    }

    impl ResourceResolver for Db {
        fn resolve<'a>(
            &'a self,
            tenant: &'a TenantId,
            args: &'a ValidatedArgs,
        ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
            let id = args.as_json()["invoice_id"].as_str().unwrap_or_default();
            let row = self.rows.lock().unwrap().get(id).cloned();
            let out = match row {
                Some((t, c, v)) if t == tenant.as_str() => Ok(ResolvedResource::new(
                    EntityUid::new("Support::Invoice", id).unwrap(),
                    ResourceVersion::new(v.to_string()),
                )
                .with_attr(
                    "customer",
                    AttrValue::Entity(EntityUid::new("Support::Customer", c).unwrap()),
                )),
                _ => Err(ResolveError::NotFound),
            };
            Box::pin(async move { out })
        }
    }

    fn execute(db: &Db, action: &AuthorizedAction) -> Result<(), ExecuteError> {
        let call = ConditionalCall::new(action, "Support::Invoice")?;
        ConditionalCall::check_status(db.refund(
            call.tenant().as_str(),
            call.resource_id(),
            call.if_match(),
        ))
    }

    /// Resolves with `db` and binds an `Allow` for the principal of the test grant (tenant `acme`).
    fn authorize(db: &Db, invoice_id: &str) -> Result<AuthorizedAction, DenyReason> {
        let ctx = ExecutionContext::from_chain(vec![grant("g1", None, spec())]).unwrap();
        let args = ValidatedArgs::new(serde_json::json!({ "invoice_id": invoice_id }));
        let resource = futures::executor::block_on(db.resolve(ctx.principal().tenant(), &args))?;
        let action = ActionId::new("refund.issue").unwrap();
        let context = AuthorizationContext::default();
        let req = AuthorizationRequest::new(&ctx, &action, &resource, &args, &context, None);
        let allow = Decision::Allow {
            determining_policies: vec![],
        };
        Ok(AuthorizedAction::bind(DecisionId::new("d1").unwrap(), &allow, &req).unwrap())
    }

    /// The call targets the bound uid in the principal's tenant, with a quoted `If-Match` and the decision id.
    #[test]
    fn call_targets_exactly_the_bound_resource() {
        let db = Db::with("acme-inv-001", "acme", "acme-cus-001");
        let a = authorize(&db, "acme-inv-001").unwrap();
        let call = ConditionalCall::new(&a, "Support::Invoice").unwrap();
        assert_eq!(call.tenant().as_str(), "acme");
        assert_eq!(call.resource_id(), "acme-inv-001");
        assert_eq!(call.if_match(), "\"1\"");
        assert_eq!(call.decision_id().as_str(), "d1");
        assert_eq!(execute(&db, &a), Ok(()));
        assert_eq!(*db.effects.lock().unwrap(), ["acme-inv-001"]);
    }

    /// An id that exists only in another tenant resolves to the same not-found denial as a missing id.
    #[test]
    fn other_tenant_id_is_indistinguishable_from_missing() {
        let db = Db::with("globex-inv-001", "globex", "globex-cus-001");
        let other = authorize(&db, "globex-inv-001").unwrap_err();
        let missing = authorize(&db, "nope").unwrap_err();
        assert_eq!(other, DenyReason::ResourceNotFound);
        assert_eq!(other.code(), missing.code());
    }

    /// An ownership change between authorize and execute fails the precondition and produces no effect.
    #[test]
    fn ownership_change_after_authorize_has_no_effect() {
        let db = Db::with("acme-inv-001", "acme", "acme-cus-001");
        let a = authorize(&db, "acme-inv-001").unwrap();
        db.reassign("acme-inv-001", "acme-cus-002");
        assert_eq!(execute(&db, &a), Err(ExecuteError::Precondition));
        assert!(db.effects.lock().unwrap().is_empty());
    }

    /// A resource moved to another tenant after authorize is a precondition failure with no effect.
    #[test]
    fn tenant_move_after_authorize_has_no_effect() {
        let db = Db::with("acme-inv-001", "acme", "acme-cus-001");
        let a = authorize(&db, "acme-inv-001").unwrap();
        db.rows.lock().unwrap().get_mut("acme-inv-001").unwrap().0 = "globex".into();
        assert_eq!(execute(&db, &a), Err(ExecuteError::Precondition));
        assert!(db.effects.lock().unwrap().is_empty());
    }

    /// A resource of the wrong type, or a version that is not a valid entity-tag, fails before any downstream call.
    #[test]
    fn wrong_type_or_unsendable_version_fails_closed() {
        let db = Db::with("acme-inv-001", "acme", "acme-cus-001");
        let a = authorize(&db, "acme-inv-001").unwrap();
        assert!(matches!(
            ConditionalCall::new(&a, "Support::Customer"),
            Err(ExecuteError::Failed(_))
        ));

        let ctx = ExecutionContext::from_chain(vec![grant("g1", None, spec())]).unwrap();
        let args = ValidatedArgs::new(serde_json::json!({}));
        let action = ActionId::new("refund.issue").unwrap();
        let context = AuthorizationContext::default();
        let allow = Decision::Allow {
            determining_policies: vec![],
        };
        for bad in ["", "1\" , \"2", "1\r\nX-Evil: 1", "a b", "é"] {
            let resource = ResolvedResource::new(
                EntityUid::new("Support::Invoice", "acme-inv-001").unwrap(),
                ResourceVersion::new(bad),
            );
            let req = AuthorizationRequest::new(&ctx, &action, &resource, &args, &context, None);
            let a = AuthorizedAction::bind(DecisionId::new("d").unwrap(), &allow, &req).unwrap();
            assert!(
                matches!(
                    ConditionalCall::new(&a, "Support::Invoice"),
                    Err(ExecuteError::Failed(_))
                ),
                "{bad:?}"
            );
        }
    }

    /// 2xx succeeds, 404 and 412 are precondition failures, and every other status is a plain failure.
    #[test]
    fn status_mapping() {
        for s in [200, 201, 204] {
            assert_eq!(ConditionalCall::check_status(s), Ok(()));
        }
        for s in [404, 412] {
            assert_eq!(
                ConditionalCall::check_status(s),
                Err(ExecuteError::Precondition)
            );
        }
        for s in [301, 400, 401, 403, 422, 428, 500, 503] {
            assert!(matches!(
                ConditionalCall::check_status(s),
                Err(ExecuteError::Failed(_))
            ));
        }
    }
}
