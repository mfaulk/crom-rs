use std::fmt;

use crate::context::{ExecutionContext, TrustedPrincipal};
use crate::decision::Decision;
use crate::grant::Delegator;
use crate::ids::{ActionId, ApprovalId, DecisionId};
use crate::resource::{ResolvedResource, ResourceVersion};
use crate::value::{Attrs, EntityUid};

/// Operation arguments that passed the operation's JSON Schema
/// (`additionalProperties: false`). Only the pipeline builds this (#351),
/// so resolvers, context builders, and executors never see unvalidated input:
///
/// ```compile_fail
/// fn forge(v: serde_json::Value) -> crom_guard::ValidatedArgs { crom_guard::ValidatedArgs(v) }
/// ```
#[derive(Clone, PartialEq)]
pub struct ValidatedArgs(serde_json::Value);

impl ValidatedArgs {
    #[allow(dead_code)] // used by the pipeline (#351)
    pub(crate) fn new(v: serde_json::Value) -> Self {
        Self(v)
    }

    pub fn as_json(&self) -> &serde_json::Value {
        &self.0
    }
}

impl fmt::Debug for ValidatedArgs {
    /// Argument values are not printed: only allowlisted fields may reach
    /// logs (MILESTONE_18 §12.1).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ValidatedArgs(..)")
    }
}

/// Context keys Guard fills itself; operations cannot supply them.
pub const RESERVED_CONTEXT_KEYS: [&str; 2] = ["grant", "approval"];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("context key {0:?} is reserved for Guard")]
pub struct ReservedContextKey(pub String);

/// Request-specific Cedar context fields, built by the operation from
/// [`ValidatedArgs`] (e.g. `amount_cents`). Guard adds `grant` and
/// `approval` itself, so an operation cannot supply them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorizationContext(Attrs);

impl AuthorizationContext {
    pub fn new(fields: Attrs) -> Result<Self, ReservedContextKey> {
        if let Some(k) = fields
            .keys()
            .find(|k| RESERVED_CONTEXT_KEYS.contains(&k.as_str()))
        {
            return Err(ReservedContextKey(k.clone()));
        }
        Ok(Self(fields))
    }

    pub fn fields(&self) -> &Attrs {
        &self.0
    }
}

/// A stored approval that matched the request (digest, grant, expiry,
/// unused). Becomes `context.approval`. Only the approval store builds it
/// (#354); the approver comes from the trusted application's admin call.
///
/// ```compile_fail
/// let a = crom_guard::ApprovalContext {
///     approval_id: "ap1".parse().unwrap(),
///     approver: todo!(),
///     fields: Default::default(),
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalContext {
    approval_id: ApprovalId,
    approver: Delegator,
    /// Approved values, e.g. `amount_cents`.
    fields: Attrs,
}

impl ApprovalContext {
    #[allow(dead_code)] // used by the approval store (#354)
    pub(crate) fn new(approval_id: ApprovalId, approver: Delegator, fields: Attrs) -> Self {
        Self {
            approval_id,
            approver,
            fields,
        }
    }
    pub fn approval_id(&self) -> &ApprovalId {
        &self.approval_id
    }
    pub fn approver(&self) -> &Delegator {
        &self.approver
    }
    pub fn fields(&self) -> &Attrs {
        &self.fields
    }
}

/// Input to the policy decision point. Built only by the pipeline, after
/// authentication, normalization, the Rust action check, and resolution.
///
/// ```compile_fail
/// fn forge<'a>(
///     ctx: &'a crom_guard::ExecutionContext,
///     action: &'a crom_guard::ActionId,
///     resource: &'a crom_guard::ResolvedResource,
///     args: &'a crom_guard::ValidatedArgs,
///     context: &'a crom_guard::AuthorizationContext,
/// ) -> crom_guard::AuthorizationRequest<'a> {
///     crom_guard::AuthorizationRequest { ctx, action, resource, args, context, approval: None }
/// }
/// ```
#[derive(Debug)]
pub struct AuthorizationRequest<'a> {
    ctx: &'a ExecutionContext,
    action: &'a ActionId,
    resource: &'a ResolvedResource,
    args: &'a ValidatedArgs,
    context: &'a AuthorizationContext,
    approval: Option<&'a ApprovalContext>,
}

impl<'a> AuthorizationRequest<'a> {
    #[allow(dead_code)] // used by the pipeline (#351)
    pub(crate) fn new(
        ctx: &'a ExecutionContext,
        action: &'a ActionId,
        resource: &'a ResolvedResource,
        args: &'a ValidatedArgs,
        context: &'a AuthorizationContext,
        approval: Option<&'a ApprovalContext>,
    ) -> Self {
        Self {
            ctx,
            action,
            resource,
            args,
            context,
            approval,
        }
    }
    /// The validated arguments; part of the approval action digest.
    pub fn args(&self) -> &ValidatedArgs {
        self.args
    }
    /// The full chain; the PDP evaluates every grant (MILESTONE_18 §8.3).
    pub fn execution_context(&self) -> &ExecutionContext {
        self.ctx
    }
    pub fn action(&self) -> &ActionId {
        self.action
    }
    pub fn resource(&self) -> &ResolvedResource {
        self.resource
    }
    pub fn context(&self) -> &AuthorizationContext {
        self.context
    }
    pub fn approval(&self) -> Option<&ApprovalContext> {
        self.approval
    }
}

/// Proof that one specific request was allowed. Executors accept only this
/// type, so no other decision can execute. Built only by the pipeline from
/// a `Decision::Allow` (MILESTONE_18 §10.2 step 5); not `Clone`, so each
/// value drives at most one execution. No public constructor, no
/// `Deserialize`:
///
/// ```compile_fail
/// let a: crom_guard::AuthorizedAction = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// let a = crom_guard::AuthorizedAction {
///     decision_id: "d".parse().unwrap(),
///     action: "refund.issue".parse().unwrap(),
///     resource: todo!(),
///     resource_version: todo!(),
///     args: todo!(),
///     principal: todo!(),
///     approval_id: None,
/// };
/// ```
///
/// ```compile_fail
/// fn dup(a: &crom_guard::AuthorizedAction) -> crom_guard::AuthorizedAction { a.clone() }
/// ```
pub struct AuthorizedAction {
    decision_id: DecisionId,
    action: ActionId,
    resource: EntityUid,
    resource_version: ResourceVersion,
    args: ValidatedArgs,
    principal: TrustedPrincipal,
    approval_id: Option<ApprovalId>,
}

impl AuthorizedAction {
    /// Binds exactly what `request` described, and only for an `Allow`
    /// decision; any other decision yields `None`.
    #[allow(dead_code)] // used by the pipeline (#351)
    pub(crate) fn bind(
        decision_id: DecisionId,
        decision: &Decision,
        request: &AuthorizationRequest<'_>,
    ) -> Option<Self> {
        if !decision.is_allow() {
            return None;
        }
        Some(Self {
            decision_id,
            action: request.action().clone(),
            resource: request.resource().uid().clone(),
            resource_version: request.resource().version().clone(),
            args: request.args().clone(),
            principal: request.execution_context().principal().clone(),
            approval_id: request.approval().map(|a| a.approval_id().clone()),
        })
    }

    /// Send downstream as `X-Guard-Decision-Id`.
    pub fn decision_id(&self) -> &DecisionId {
        &self.decision_id
    }
    pub fn action(&self) -> &ActionId {
        &self.action
    }
    /// The resolved resource; the executor must target exactly this uid.
    pub fn resource(&self) -> &EntityUid {
        &self.resource
    }
    /// The executor must condition the downstream call on this version.
    pub fn resource_version(&self) -> &ResourceVersion {
        &self.resource_version
    }
    pub fn args(&self) -> &ValidatedArgs {
        &self.args
    }
    pub fn principal(&self) -> &TrustedPrincipal {
        &self.principal
    }
    pub fn approval_id(&self) -> Option<&ApprovalId> {
        self.approval_id.as_ref()
    }
}

impl fmt::Debug for AuthorizedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthorizedAction")
            .field("decision_id", &self.decision_id)
            .field("action", &self.action)
            .field("resource", &self.resource)
            .field("grant_id", self.principal.grant_id())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::AttrValue;

    /// Operations cannot supply the Guard-owned `grant` or `approval` context keys.
    #[test]
    fn reserved_context_keys_rejected() {
        for k in RESERVED_CONTEXT_KEYS {
            let fields: Attrs = [(k.to_owned(), AttrValue::Bool(true))].into();
            assert_eq!(
                AuthorizationContext::new(fields),
                Err(ReservedContextKey(k.to_owned()))
            );
        }
        let ok: Attrs = [("amount_cents".to_owned(), AttrValue::Long(100))].into();
        assert!(AuthorizationContext::new(ok).is_ok());
    }

    /// `bind` yields an action only for `Allow`, and copies the action, resource, version, args, and principal from the request.
    #[test]
    fn bind_only_from_allow_and_from_the_request() {
        use crate::decision::{ActionDigest, ApprovalRequirement, DenyReason};
        use crate::grant::tests::{grant, spec};
        use crate::resource::ResourceVersion;
        use crate::value::EntityUid;

        let ctx = ExecutionContext::from_chain(vec![grant("g1", None, spec())]).unwrap();
        let action = ActionId::new("refund.issue").unwrap();
        let resource = ResolvedResource::new(
            EntityUid::new("Support::Invoice", "acme-inv-001").unwrap(),
            ResourceVersion::new("3"),
        );
        let args = ValidatedArgs::new(
            serde_json::json!({"invoice_id": "acme-inv-001", "amount_cents": 5}),
        );
        let context = AuthorizationContext::default();
        let req = AuthorizationRequest::new(&ctx, &action, &resource, &args, &context, None);
        let id = || DecisionId::new("d1").unwrap();

        assert!(
            AuthorizedAction::bind(id(), &Decision::deny(DenyReason::PolicyDeny), &req).is_none()
        );
        let approval = Decision::ApprovalRequired {
            requirement: ApprovalRequirement {
                role: "supervisor".into(),
                action_digest: ActionDigest::from_bytes([0; 32]),
            },
        };
        assert!(AuthorizedAction::bind(id(), &approval, &req).is_none());

        let allow = Decision::Allow {
            determining_policies: vec![],
        };
        let a = AuthorizedAction::bind(id(), &allow, &req).unwrap();
        assert_eq!(a.action(), &action);
        assert_eq!(a.resource(), resource.uid());
        assert_eq!(a.resource_version().as_str(), "3");
        assert_eq!(a.args(), &args);
        assert_eq!(a.principal(), ctx.principal());
        assert_eq!(a.approval_id(), None);
    }

    /// Debug output of validated arguments never includes their values.
    #[test]
    fn validated_args_debug_hides_values() {
        let a = ValidatedArgs::new(serde_json::json!({"note": "secret-memo"}));
        assert!(!format!("{a:?}").contains("secret-memo"));
    }
}
