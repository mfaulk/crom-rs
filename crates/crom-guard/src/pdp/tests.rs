use std::collections::BTreeMap;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};

use super::*;
use crate::action::{ApprovalContext, AuthorizationContext, ValidatedArgs};
use crate::context::ExecutionContext;
use crate::decision::{ActionDigest, ApprovalRequirement, DenyReason};
use crate::grant::{Constraint, Constraints, Delegator, Grant, GrantIssue, GrantSpec};
use crate::ids::{ActionId, AgentId, AppId, ApprovalId, RunId, TenantId, UserId};
use crate::resource::{ResolvedResource, ResourceVersion};
use crate::time::Timestamp;
use crate::traits::PolicyDecisionPoint;
use crate::value::{AttrValue, Attrs, EntityUid};

const SCHEMA: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../../examples/guard-support/policies/support.cedar");
const ALL_ACTIONS: &[&str] = &[
    "customer.read",
    "invoice.list",
    "refund.issue",
    "customer.export",
];

fn pdp() -> &'static CedarPdp {
    static PDP: OnceLock<CedarPdp> = OnceLock::new();
    PDP.get_or_init(|| CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap())
}

fn pdp_with(extra: &str) -> CedarPdp {
    CedarPdp::from_sources("Support", SCHEMA, &format!("{POLICIES}\n{extra}\n")).unwrap()
}

fn load_with(extra: &str) -> Result<CedarPdp, PdpLoadError> {
    CedarPdp::from_sources("Support", SCHEMA, &format!("{POLICIES}\n{extra}\n"))
}

fn uid(ty: &str, id: &str) -> EntityUid {
    EntityUid::new(format!("Support::{ty}"), id).unwrap()
}

fn ent(ty: &str, id: &str) -> AttrValue {
    AttrValue::Entity(uid(ty, id))
}

fn customer(id: &str, tenant: &str) -> ResolvedResource {
    ResolvedResource::new(uid("Customer", id), ResourceVersion::new("1"))
        .with_attr("tenant", ent("Tenant", tenant))
        .with_parent(uid("Tenant", tenant))
}

fn invoice(id: &str, tenant: &str, cust: &str, amount: i64, refunded: i64) -> ResolvedResource {
    ResolvedResource::new(uid("Invoice", id), ResourceVersion::new("1"))
        .with_attr("tenant", ent("Tenant", tenant))
        .with_attr("customer", ent("Customer", cust))
        .with_attr("amount_cents", AttrValue::Long(amount))
        .with_attr("refunded_cents", AttrValue::Long(refunded))
        .with_parent(uid("Customer", cust))
}

/// i1: c1, 200.00, nothing refunded.
fn i1() -> ResolvedResource {
    invoice("i1", "t1", "c1", 20_000, 0)
}

fn user(id: &str, role: &str, limit: i64) -> Delegator {
    Delegator::new(
        UserId::new(id).unwrap(),
        role,
        [("refund_limit_cents".to_owned(), AttrValue::Long(limit))].into(),
    )
    .unwrap()
}

fn alice() -> Delegator {
    user("alice", "agent", 5_000)
}

#[derive(Clone)]
struct G {
    id: &'static str,
    actions: &'static [&'static str],
    customers: &'static [&'static str],
    max_refund_cents: i64,
}

const G_FULL: G = G {
    id: "g1",
    actions: ALL_ACTIONS,
    customers: &["c1"],
    max_refund_cents: 10_000,
};
const G_NARROW: G = G {
    id: "g2",
    actions: &["customer.read", "refund.issue"],
    customers: &["c1"],
    max_refund_cents: 2_000,
};
const G_READ_ONLY: G = G {
    id: "g1",
    actions: &["customer.read"],
    customers: &["c1"],
    max_refund_cents: 0,
};

/// Builds a structurally valid chain, leaf first.
fn chain(gs: &[G]) -> ExecutionContext {
    let grants = gs
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let constraints = Constraints::new(BTreeMap::from([
                (
                    "customers".to_owned(),
                    Constraint::Set(g.customers.iter().map(|c| uid("Customer", c)).collect()),
                ),
                (
                    "max_refund_cents".to_owned(),
                    Constraint::AtMost(g.max_refund_cents),
                ),
            ]))
            .unwrap();
            grant(g, gs.get(i + 1).map(|p| p.id), i, constraints)
        })
        .collect();
    ExecutionContext::from_chain(grants).unwrap()
}

fn grant(g: &G, parent: Option<&str>, depth: usize, constraints: Constraints) -> Grant {
    Grant::issue(
        GrantSpec {
            tenant: TenantId::new("t1").unwrap(),
            delegator: alice(),
            run_id: RunId::new("run-1").unwrap(),
            actions: g
                .actions
                .iter()
                .map(|a| ActionId::new(*a).unwrap())
                .collect(),
            constraints,
            ttl_seconds: 600,
            max_depth: u8::try_from(depth).unwrap(),
        },
        GrantIssue {
            grant_id: GrantId::new(g.id).unwrap(),
            issuer: AppId::new("support-app").unwrap(),
            agent: AgentId::new("a1").unwrap(),
            not_before: Timestamp::from_unix_millis(1_000),
            expires_at: Timestamp::from_unix_millis(601_000),
            parent: parent.map(|p| GrantId::new(p).unwrap()),
        },
    )
}

#[derive(Debug, Clone, PartialEq)]
enum Expect {
    Allow(&'static [&'static str]),
    Deny(DenyReason, &'static [&'static str]),
    Approval(&'static str),
}

#[derive(Clone)]
struct Case {
    name: &'static str,
    action: &'static str,
    resource: ResolvedResource,
    amount: Option<i64>,
    /// Approver and approved amount.
    approval: Option<(Delegator, i64)>,
    chain: Vec<G>,
    expect: Expect,
}

/// Request parts owned by a test, from which an `AuthorizationRequest` borrows.
struct Parts {
    ctx: ExecutionContext,
    action: ActionId,
    resource: ResolvedResource,
    args: ValidatedArgs,
    context: AuthorizationContext,
    approval: Option<ApprovalContext>,
}

impl Parts {
    fn new(c: &Case) -> Self {
        Self::build(c, chain(&c.chain))
    }

    fn build(c: &Case, ctx: ExecutionContext) -> Self {
        let mut args = serde_json::json!({"resource_id": c.resource.uid().id()});
        let mut fields = Attrs::new();
        if let Some(a) = c.amount {
            args["amount_cents"] = a.into();
            fields.insert("amount_cents".into(), AttrValue::Long(a));
        }
        Self {
            ctx,
            action: ActionId::new(c.action).unwrap(),
            resource: c.resource.clone(),
            args: ValidatedArgs::new(args),
            context: AuthorizationContext::new(fields).unwrap(),
            approval: c.approval.clone().map(|(approver, amount)| {
                ApprovalContext::new(
                    ApprovalId::new("ap1").unwrap(),
                    approver,
                    [("amount_cents".to_owned(), AttrValue::Long(amount))].into(),
                )
            }),
        }
    }

    fn request(&self) -> AuthorizationRequest<'_> {
        AuthorizationRequest::new(
            &self.ctx,
            &self.action,
            &self.resource,
            &self.args,
            &self.context,
            self.approval.as_ref(),
        )
    }

    fn digest(&self) -> ActionDigest {
        ActionDigest::compute(
            self.ctx.principal().grant_id(),
            &self.action,
            self.resource.uid(),
            &self.args,
        )
    }
}

fn ids(v: &[&str]) -> Vec<PolicyId> {
    v.iter().map(|s| PolicyId::new(*s).unwrap()).collect()
}

fn expected(c: &Case, parts: &Parts) -> Decision {
    match &c.expect {
        Expect::Allow(p) => Decision::Allow {
            determining_policies: ids(p),
        },
        Expect::Deny(r, p) => Decision::Deny {
            reason: r.clone(),
            determining_policies: ids(p),
        },
        Expect::Approval(role) => Decision::ApprovalRequired {
            requirement: ApprovalRequirement {
                role: (*role).into(),
                action_digest: parts.digest(),
            },
        },
    }
}

fn check(p: &CedarPdp, c: &Case) {
    let parts = Parts::new(c);
    let got = p.authorize(&parts.request());
    assert_eq!(got, expected(c, &parts), "case {:?}", c.name);
}

fn read(name: &'static str, action: &'static str, res: ResolvedResource, expect: Expect) -> Case {
    Case {
        name,
        action,
        resource: res,
        amount: None,
        approval: None,
        chain: vec![G_FULL],
        expect,
    }
}

fn refund(
    name: &'static str,
    res: ResolvedResource,
    amount: i64,
    approval: Option<(Delegator, i64)>,
    chain: Vec<G>,
    expect: Expect,
) -> Case {
    Case {
        name,
        action: "refund.issue",
        resource: res,
        amount: Some(amount),
        approval,
        chain,
        expect,
    }
}

fn sup() -> Option<(Delegator, i64)> {
    Some((user("sup", "supervisor", 100_000), 8_000))
}

/// The ADR 0003 §3.5 decision table (rows 1–20, in order) plus further
/// §14.2 rows, boundaries, and chain cases.
fn table() -> Vec<Case> {
    use DenyReason::*;
    use Expect::*;
    let i2 = || invoice("i2", "t1", "c2", 20_000, 0);
    let i3 = || invoice("i3", "t1", "c1", 10_000, 9_000);
    let i9 = || invoice("i9", "t2", "c1", 20_000, 0);
    let full = || vec![G_FULL];
    let narrow_chain = || vec![G_NARROW, G_FULL];
    vec![
        // ADR 0003 §3.5 rows 1–20.
        read(
            "01 read delegated customer",
            "customer.read",
            customer("c1", "t1"),
            Allow(&["read-delegated-customers"]),
        ),
        read(
            "02 read undelegated same-tenant customer",
            "customer.read",
            customer("c2", "t1"),
            Deny(PolicyDeny, &[]),
        ),
        read(
            "03 read other-tenant customer",
            "customer.read",
            customer("c9", "t2"),
            Deny(PolicyDeny, &["tenant-wall"]),
        ),
        refund(
            "04 refund on a read-only grant",
            i1(),
            1_000,
            None,
            vec![G_READ_ONLY],
            Deny(ActionNotGranted, &[]),
        ),
        refund(
            "05 refund 30.00 within all limits",
            i1(),
            3_000,
            None,
            full(),
            Allow(&["refund-within-limits"]),
        ),
        refund(
            "06 refund 80.00 over delegator limit",
            i1(),
            8_000,
            None,
            full(),
            Approval("supervisor"),
        ),
        refund(
            "07 refund 80.00 with matching supervisor approval",
            i1(),
            8_000,
            sup(),
            full(),
            Allow(&["refund-with-supervisor-approval"]),
        ),
        refund(
            "08 refund 80.00 with approval for 70.00",
            i1(),
            8_000,
            Some((user("sup", "supervisor", 100_000), 7_000)),
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "09 refund 80.00 approved by a non-supervisor",
            i1(),
            8_000,
            Some((user("bob", "agent", 5_000), 8_000)),
            full(),
            Deny(PolicyDeny, &[]),
        ),
        // Row 10 (another tenant's supervisor) needs an approver tenant the
        // public API cannot express; see `other_tenant_approver_denied`.
        refund(
            "11 refund 20.00 over remaining balance 10.00",
            i3(),
            2_000,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "12 over balance with a supervisor approval",
            i3(),
            2_000,
            Some((user("sup", "supervisor", 100_000), 2_000)),
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "13 refund on undelegated customer's invoice",
            i2(),
            1_000,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "14 refund 10.00 on another tenant's invoice",
            i9(),
            1_000,
            None,
            full(),
            Deny(PolicyDeny, &["tenant-wall"]),
        ),
        refund(
            "15 refund 80.00 on another tenant's invoice",
            i9(),
            8_000,
            None,
            full(),
            Deny(PolicyDeny, &["tenant-wall"]),
        ),
        refund(
            "16 negative refund",
            i1(),
            -500,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "17 negative refund with matching approval",
            i1(),
            -500,
            Some((user("sup", "supervisor", 100_000), -500)),
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "18 chain: child max 20.00, refund 30.00",
            i1(),
            3_000,
            None,
            narrow_chain(),
            Approval("supervisor"),
        ),
        refund(
            "19 chain: child max 20.00, refund 15.00",
            i1(),
            1_500,
            None,
            narrow_chain(),
            Allow(&["refund-within-limits"]),
        ),
        read(
            "20 customer.export",
            "customer.export",
            customer("c1", "t1"),
            Deny(PolicyDeny, &["no-bulk-export"]),
        ),
        // §14.2 rows and boundaries.
        read(
            "21 list invoices of a delegated customer",
            "invoice.list",
            customer("c1", "t1"),
            Allow(&["read-delegated-customers"]),
        ),
        read(
            "22 list invoices of another tenant's customer",
            "invoice.list",
            customer("c9", "t2"),
            Deny(PolicyDeny, &["tenant-wall"]),
        ),
        read(
            "23 export another tenant's customer: both forbids",
            "customer.export",
            customer("c9", "t2"),
            Deny(PolicyDeny, &["no-bulk-export", "tenant-wall"]),
        ),
        refund(
            "24 refund exactly at the delegator limit",
            i1(),
            5_000,
            None,
            full(),
            Allow(&["refund-within-limits"]),
        ),
        refund(
            "25 refund one cent over the delegator limit",
            i1(),
            5_001,
            None,
            full(),
            Approval("supervisor"),
        ),
        refund(
            "26 refund over the grant max needs approval",
            i1(),
            12_000,
            None,
            full(),
            Approval("supervisor"),
        ),
        refund(
            "27 refund over the grant max with approval",
            i1(),
            12_000,
            Some((user("sup", "supervisor", 100_000), 12_000)),
            full(),
            Allow(&["refund-with-supervisor-approval"]),
        ),
        refund(
            "28 zero refund",
            i1(),
            0,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "29 refund exactly the remaining balance",
            i3(),
            1_000,
            None,
            full(),
            Allow(&["refund-within-limits"]),
        ),
        refund(
            "30 refund one cent over the remaining balance",
            i3(),
            1_001,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "31 approval for more than requested",
            i1(),
            8_000,
            Some((user("sup", "supervisor", 100_000), 9_000)),
            full(),
            Deny(PolicyDeny, &[]),
        ),
        refund(
            "32 approval present on a request that needs none",
            i1(),
            3_000,
            sup().map(|(u, _)| (u, 3_000)),
            full(),
            Allow(&["refund-with-supervisor-approval", "refund-within-limits"]),
        ),
        refund(
            "33 other-tenant invoice with supervisor approval",
            i9(),
            8_000,
            sup(),
            full(),
            Deny(PolicyDeny, &["tenant-wall"]),
        ),
        refund(
            "34 refund over balance needs no approval prompt",
            invoice("i4", "t1", "c1", 5_000, 0),
            8_000,
            None,
            full(),
            Deny(PolicyDeny, &[]),
        ),
        // Chains.
        refund(
            "35 chain: ancestor lacks the customer",
            i1(),
            1_000,
            None,
            vec![
                G {
                    id: "g2",
                    customers: &["c1"],
                    ..G_NARROW
                },
                G {
                    customers: &["c2"],
                    ..G_FULL
                },
            ],
            Deny(
                ChainDeny {
                    grant_id: GrantId::new("g1").unwrap(),
                },
                &[],
            ),
        ),
        read(
            "36 chain: read where ancestor lacks the customer",
            "customer.read",
            customer("c1", "t1"),
            Expect::Deny(
                ChainDeny {
                    grant_id: GrantId::new("g1").unwrap(),
                },
                &[],
            ),
        )
        .with_chain(vec![
            G_NARROW,
            G {
                customers: &["c2"],
                ..G_FULL
            },
        ]),
        read(
            "37 chain: cross-tenant read denied at the leaf",
            "customer.read",
            customer("c9", "t2"),
            Deny(PolicyDeny, &["tenant-wall"]),
        )
        .with_chain(vec![G_NARROW, G_FULL]),
        read(
            "38 chain of three, all allow",
            "customer.read",
            customer("c1", "t1"),
            Allow(&["read-delegated-customers"]),
        )
        .with_chain(vec![
            G {
                id: "g3",
                ..G_NARROW
            },
            G {
                id: "g2",
                ..G_NARROW
            },
            G_FULL,
        ]),
        refund(
            "39 chain: action missing from the ancestor",
            i1(),
            1_000,
            None,
            vec![
                G_NARROW,
                G {
                    actions: &["customer.read"],
                    ..G_FULL
                },
            ],
            Deny(ActionNotGranted, &[]),
        ),
        refund(
            "40 chain: over child max with approval",
            i1(),
            3_000,
            sup().map(|(u, _)| (u, 3_000)),
            narrow_chain(),
            Allow(&["refund-with-supervisor-approval", "refund-within-limits"]),
        ),
        refund(
            "41 chain: ancestor lacks customer, approval cannot help",
            i1(),
            8_000,
            None,
            vec![
                G_NARROW,
                G {
                    customers: &["c2"],
                    ..G_FULL
                },
            ],
            Deny(PolicyDeny, &[]),
        ),
        // Inputs the schema rejects.
        read(
            "42 action not declared in the schema",
            "customer.delete",
            customer("c1", "t1"),
            Deny(UnknownAction, &[]),
        )
        .with_actions(&["customer.delete"]),
        refund(
            "43 refund action on a customer resource",
            customer("c1", "t1"),
            1_000,
            None,
            full(),
            Deny(PolicyError, &[]),
        ),
        read(
            "44 refund context missing amount_cents",
            "refund.issue",
            i1(),
            Deny(PolicyError, &[]),
        ),
        refund(
            "45 invoice missing a required attribute",
            ResolvedResource::new(uid("Invoice", "i5"), ResourceVersion::new("1"))
                .with_attr("tenant", ent("Tenant", "t1")),
            1_000,
            None,
            full(),
            Deny(PolicyError, &[]),
        ),
        refund(
            "46 resource uses the reserved placeholder id",
            invoice(APPROVAL_PLACEHOLDER_ID, "t1", "c1", 20_000, 0),
            1_000,
            None,
            full(),
            Deny(PolicyError, &[]),
        ),
    ]
}

impl Case {
    fn with_chain(mut self, chain: Vec<G>) -> Self {
        self.chain = chain;
        self
    }

    fn with_actions(mut self, actions: &'static [&'static str]) -> Self {
        for g in &mut self.chain {
            g.actions = actions;
        }
        self
    }
}

/// Every table case yields exactly the expected decision and determining policies.
#[test]
fn decision_table() {
    let cases = table();
    assert!(cases.len() >= 40, "{} cases", cases.len());
    for c in &cases {
        check(pdp(), c);
    }
}

/// Row 35 reports a chain deny with the leaf's evaluation recorded as allowed.
#[test]
fn chain_deny_records_leaf_allow() {
    let c = &table()[33];
    assert!(c.name.starts_with("35"), "{}", c.name);
    let parts = Parts::new(c);
    let e = pdp().evaluate_chain(&parts.request());
    assert_eq!(e.grants.len(), 2);
    assert!(e.grants[0].allowed);
    assert_eq!(e.grants[0].grant_id.as_str(), "g2");
    assert!(!e.grants[1].allowed);
}

/// ADR 0003 §3.5 row 10: a supervisor of another tenant cannot approve.
#[test]
fn other_tenant_approver_denied() {
    let c = refund("10", i1(), 8_000, sup(), vec![G_FULL], Expect::Allow(&[]));
    let parts = Parts::new(&c);
    let req = parts.request();
    let t2 = TenantId::new("t2").unwrap();
    let mut inputs = Inputs::from_request(&req);
    inputs.approval.as_mut().unwrap().tenant = &t2;
    assert_eq!(
        pdp().evaluate(&inputs).decision,
        Decision::deny(DenyReason::PolicyDeny)
    );
    assert!(pdp().authorize(&req).is_allow(), "control: same tenant");
}

/// Fail-closed: an erroring forbid denies with PolicyError even though Cedar's own decision is Allow.
#[test]
fn erroring_forbid_denies_even_when_cedar_allows() {
    let p = pdp_with(
        r#"@id("overflow-forbid")
forbid (principal, action == Support::Action::"refund.issue", resource)
when { context.amount_cents * 4611686018427387904 > 0 };"#,
    );
    let c = refund("", i1(), 3_000, None, vec![G_FULL], Expect::Allow(&[]));
    let parts = Parts::new(&c);
    let e = p.evaluate_chain(&parts.request());
    assert_eq!(
        e.decision,
        Decision::Deny {
            reason: DenyReason::PolicyError,
            determining_policies: ids(&["overflow-forbid"]),
        }
    );
    assert_eq!(e.grants[0].erroring_policies, ids(&["overflow-forbid"]));
    assert!(
        e.errors.iter().any(|m| m.contains("overflow")),
        "{:?}",
        e.errors
    );
}

/// Fail-closed: an erroring permit also denies with PolicyError, even when another permit allows.
#[test]
fn erroring_permit_denies() {
    let p = pdp_with(
        r#"@id("overflow-permit")
permit (principal, action == Support::Action::"refund.issue", resource)
when { context.amount_cents * 4611686018427387904 > 0 };"#,
    );
    let c = refund("", i1(), 3_000, None, vec![G_FULL], Expect::Allow(&[]));
    let parts = Parts::new(&c);
    assert_eq!(
        p.authorize(&parts.request()),
        Decision::Deny {
            reason: DenyReason::PolicyError,
            determining_policies: ids(&["overflow-permit"]),
        }
    );
}

/// Fail-closed: a forbid that errors only in the approval probe yields PolicyError, not ApprovalRequired.
#[test]
fn erroring_probe_denies() {
    let p = pdp_with(
        r#"@id("probe-error-forbid")
forbid (principal, action == Support::Action::"refund.issue", resource)
when { context has approval && context.approval.amount_cents * 4611686018427387904 > 0 };"#,
    );
    let c = refund("", i1(), 8_000, None, vec![G_FULL], Expect::Allow(&[]));
    let parts = Parts::new(&c);
    assert_eq!(
        p.authorize(&parts.request()),
        Decision::Deny {
            reason: DenyReason::PolicyError,
            determining_policies: ids(&["probe-error-forbid"]),
        }
    );
}

/// Fail-closed: a forbid that dereferences an entity missing from the store errors, so the request is denied.
#[test]
fn missing_entity_denies() {
    let p = pdp_with(
        r#"@id("customer-tenant-forbid")
forbid (principal, action == Support::Action::"refund.issue", resource)
when { resource.customer.tenant != principal.tenant };"#,
    );
    let c = refund("", i1(), 3_000, None, vec![G_FULL], Expect::Allow(&[]));
    let parts = Parts::new(&c);
    let e = p.evaluate_chain(&parts.request());
    assert_eq!(
        e.decision,
        Decision::Deny {
            reason: DenyReason::PolicyError,
            determining_policies: ids(&["customer-tenant-forbid"]),
        }
    );
    assert!(
        e.errors.iter().any(|m| m.contains("does not exist")),
        "{:?}",
        e.errors
    );
}

/// Fail-closed: context values of the wrong type or undeclared context keys fail schema validation and deny.
#[test]
fn schema_invalid_context_denies() {
    let c = refund("", i1(), 3_000, None, vec![G_FULL], Expect::Allow(&[]));
    for fields in [
        Attrs::from([("amount_cents".to_owned(), AttrValue::String("3000".into()))]),
        Attrs::from([
            ("amount_cents".to_owned(), AttrValue::Long(3_000)),
            ("extra".to_owned(), AttrValue::Bool(true)),
        ]),
    ] {
        let mut parts = Parts::new(&c);
        parts.context = AuthorizationContext::new(fields).unwrap();
        assert_eq!(
            pdp().authorize(&parts.request()),
            Decision::deny(DenyReason::PolicyError)
        );
    }
}

/// Fail-closed: grant constraints that do not match the schema's GrantCtx deny.
#[test]
fn schema_invalid_grant_record_denies() {
    let c = read(
        "",
        "customer.read",
        customer("c1", "t1"),
        Expect::Allow(&[]),
    );
    let only_customers = Constraints::new(BTreeMap::from([(
        "customers".to_owned(),
        Constraint::Set([uid("Customer", "c1")].into()),
    )]))
    .unwrap();
    let ctx = ExecutionContext::from_chain(vec![grant(&G_FULL, None, 0, only_customers)]).unwrap();
    let parts = Parts::build(&c, ctx);
    assert_eq!(
        pdp().authorize(&parts.request()),
        Decision::deny(DenyReason::PolicyError)
    );
}

/// An approver who is also the delegator, with different attributes, is a conflicting entity and denies.
#[test]
fn conflicting_approver_entity_denies() {
    let c = refund(
        "",
        i1(),
        8_000,
        Some((user("alice", "supervisor", 100_000), 8_000)),
        vec![G_FULL],
        Expect::Allow(&[]),
    );
    let parts = Parts::new(&c);
    assert_eq!(
        pdp().authorize(&parts.request()),
        Decision::deny(DenyReason::PolicyError)
    );
}

/// ApprovalRequired is never an allow, and its digest is bound to the leaf grant.
#[test]
fn approval_required_binds_leaf_digest() {
    let c = &table()[16];
    assert!(c.name.starts_with("18"), "{}", c.name);
    let parts = Parts::new(c);
    let d = pdp().authorize(&parts.request());
    assert!(!d.is_allow());
    let Decision::ApprovalRequired { requirement } = d else {
        panic!("{d:?}")
    };
    let expected = ActionDigest::compute(
        &GrantId::new("g2").unwrap(),
        &parts.action,
        parts.resource.uid(),
        &parts.args,
    );
    assert_eq!(requirement.action_digest, expected);
}

/// The same inputs give the same decision and determining policies across 10k runs.
#[test]
fn deterministic_over_10k_runs() {
    let cases = table();
    let picks: Vec<&Case> = ["05", "06", "23", "32"]
        .iter()
        .map(|n| cases.iter().find(|c| c.name.starts_with(n)).unwrap())
        .collect();
    for c in picks {
        let parts = Parts::new(c);
        let first = pdp().authorize(&parts.request());
        assert_eq!(first, expected(c, &parts));
        for _ in 0..10_000 / 4 {
            assert_eq!(pdp().authorize(&parts.request()), first, "{}", c.name);
        }
    }
}

/// The PDP is usable as a shared trait object across threads.
#[test]
fn pdp_is_a_send_sync_trait_object() {
    fn assert_send_sync<T: Send + Sync>(_: &T) {}
    let p: &dyn PolicyDecisionPoint = pdp();
    assert_send_sync(&p);
}

fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The reference schema and policies load, and the hashes are hex SHA-256 of the loaded source text.
#[test]
fn loads_reference_set_and_hashes_sources() {
    let p = pdp();
    assert_eq!(p.policy_set_hash(), sha256_hex(POLICIES));
    assert_eq!(p.schema_hash(), sha256_hex(SCHEMA));
    assert_eq!(p.approval_roles, ["supervisor"]);
}

/// Loading from files reads the same sources; a missing file is refused.
#[test]
fn loads_from_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/guard-support/policies");
    let p = CedarPdp::from_files(
        "Support",
        &dir.join("support.cedarschema"),
        &dir.join("support.cedar"),
    )
    .unwrap();
    assert_eq!(p.policy_set_hash(), sha256_hex(POLICIES));
    let missing = CedarPdp::from_files("Support", &dir.join("nope"), &dir.join("support.cedar"));
    assert!(
        matches!(missing, Err(PdpLoadError::Io { .. })),
        "{missing:?}"
    );
}

/// Fail-closed at startup: unparseable schema or policies are refused.
#[test]
fn refuses_unparseable_sources() {
    let e = CedarPdp::from_sources("Support", "namespace Support { entity }", POLICIES);
    assert!(matches!(e, Err(PdpLoadError::SchemaParse(_))), "{e:?}");
    let e = load_with("permit (");
    assert!(matches!(e, Err(PdpLoadError::PolicyParse(_))), "{e:?}");
}

/// Fail-closed at startup: a policy that fails strict validation (misspelt attribute) is refused.
#[test]
fn refuses_strict_validation_failure() {
    let bad = POLICIES.replace(
        "principal.delegator.refund_limit_cents",
        "principal.delegator.refund_limt_cents",
    );
    assert_ne!(bad, POLICIES);
    let e = CedarPdp::from_sources("Support", SCHEMA, &bad);
    assert!(matches!(e, Err(PdpLoadError::Validation(_))), "{e:?}");
}

/// Fail-closed at startup: strict-validation warnings are treated as errors.
#[test]
fn refuses_validation_warnings() {
    let e = load_with(
        r#"@id("impossible")
permit (principal, action == Support::Action::"customer.read", resource)
when { false };"#,
    );
    assert!(
        matches!(&e, Err(PdpLoadError::Validation(v)) if v.iter().all(|m| m.starts_with("warning:"))),
        "{e:?}"
    );
}

/// Fail-closed at startup: missing, duplicate, or invalid @id annotations are refused.
#[test]
fn refuses_bad_policy_ids() {
    let export = r#"forbid (principal, action == Support::Action::"customer.export", resource);"#;
    let e = load_with(export);
    assert_eq!(e.unwrap_err(), PdpLoadError::MissingId { index: 5 });
    let e = load_with(&format!("@id(\"tenant-wall\")\n{export}"));
    assert_eq!(
        e.unwrap_err(),
        PdpLoadError::DuplicateId("tenant-wall".into())
    );
    for bad in ["bad id", "__guard_x", ""] {
        let e = load_with(&format!("@id(\"{bad}\")\n{export}"));
        assert_eq!(e.unwrap_err(), PdpLoadError::InvalidId(bad.into()));
    }
}

/// Fail-closed at startup: @approval on a forbid, with an empty role, or on a permit that does not test that role is refused.
#[test]
fn refuses_bad_approval_annotations() {
    let e = load_with(
        r#"@id("x")
@approval("supervisor")
forbid (principal, action == Support::Action::"customer.export", resource);"#,
    );
    assert!(matches!(e, Err(PdpLoadError::Approval { .. })), "{e:?}");
    let approval_permit = POLICIES
        .split("@id(\"refund-with-supervisor-approval\")")
        .nth(1)
        .unwrap()
        .split("@id(")
        .next()
        .unwrap()
        .replace("@approval(\"supervisor\")", "");
    for role in ["", "manager"] {
        let e = load_with(&format!(
            "@id(\"x\")\n@approval(\"{role}\"){approval_permit}"
        ));
        assert!(
            matches!(e, Err(PdpLoadError::Approval { .. })),
            "{role}: {e:?}"
        );
    }
    // Control: the same permit with the matching role loads.
    load_with(&format!(
        "@id(\"x\")\n@approval(\"supervisor\"){approval_permit}"
    ))
    .unwrap();
}

/// Templates, an oversized source, a wrong or malformed namespace are refused at startup.
#[test]
fn refuses_templates_size_and_namespace() {
    let e = load_with(
        r#"@id("t")
permit (principal == ?principal, action == Support::Action::"customer.read", resource);"#,
    );
    assert_eq!(e.unwrap_err(), PdpLoadError::Templates);
    let big = format!("{POLICIES}//{}", "x".repeat(MAX_POLICY_SOURCE_BYTES));
    let e = CedarPdp::from_sources("Support", SCHEMA, &big);
    assert!(matches!(e, Err(PdpLoadError::TooLarge { .. })), "{e:?}");
    let e = CedarPdp::from_sources("Billing", SCHEMA, POLICIES);
    assert!(
        matches!(e, Err(PdpLoadError::MissingEntityType(_))),
        "{e:?}"
    );
    let e = CedarPdp::from_sources("Support::", SCHEMA, POLICIES);
    assert!(matches!(e, Err(PdpLoadError::InvalidNamespace(_))), "{e:?}");
}

/// Latency of `authorize` on the support set; run with
/// `cargo test --release -p crom-guard pdp_latency -- --ignored --nocapture`.
#[test]
#[ignore = "benchmark"]
fn pdp_latency() {
    use std::time::Instant;
    let cases = table();
    let n = 20_000;
    for name in ["05", "06", "07", "03"] {
        let c = cases.iter().find(|c| c.name.starts_with(name)).unwrap();
        let parts = Parts::new(c);
        let req = parts.request();
        for _ in 0..n / 10 {
            std::hint::black_box(pdp().authorize(&req));
        }
        let mut t: Vec<u128> = (0..n)
            .map(|_| {
                let s = Instant::now();
                std::hint::black_box(pdp().authorize(&req));
                s.elapsed().as_nanos()
            })
            .collect();
        t.sort_unstable();
        let us = |q: usize| t[n * q / 100] as f64 / 1e3;
        println!(
            "{}: p50={:.1}us p95={:.1}us p99={:.1}us (n={n})",
            c.name,
            us(50),
            us(95),
            us(99)
        );
    }
    let t = Instant::now();
    for _ in 0..100 {
        std::hint::black_box(CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap());
    }
    println!("load: {:.2}ms avg", t.elapsed().as_secs_f64() * 10.0);
}
