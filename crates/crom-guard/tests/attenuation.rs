//! Attenuation (#353, MILESTONE_18 §8.3): `guard.delegate` produces only
//! narrower children, every widening attempt is denied before anything is
//! written, and a child's authority implies its parent's.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crom_guard::*;
use futures::executor::block_on;
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use serde_json::{json, Value};

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");
const START_MS: u64 = 1_000_000;
const ROOT_TTL: u64 = 600;
const CUSTOMERS: [&str; 3] = ["acme-c1", "acme-c2", "acme-c3"];

// ---------------------------------------------------------------- fixtures

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

#[derive(Default)]
struct MemorySink(Mutex<Vec<AuditRecord>>);

impl AuditSink for MemorySink {
    fn append<'a>(
        &'a self,
        record: &'a AuditRecord,
    ) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
        Box::pin(async move {
            let mut r = self.0.lock().unwrap();
            r.push(record.clone());
            Ok(AuditReceipt {
                seq: r.len() as u64,
                head_hash: String::new(),
            })
        })
    }
}

impl MemorySink {
    fn records(&self) -> Vec<AuditRecord> {
        self.0.lock().unwrap().clone()
    }
}

struct Broker;

impl CredentialBroker for Broker {
    fn lease<'a>(&'a self, _: &'a ActionId) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async { Ok(CredentialLease::new("test", "test-secret")) })
    }
}

fn uid(ty: &str, id: &str) -> EntityUid {
    EntityUid::new(format!("Support::{ty}"), id).unwrap()
}

/// Customers `acme-c1..3` and `globex-c9`; invoice `<customer>-inv` of
/// 200.00, nothing refunded. Static, so a call never changes later ones.
struct Resolver(&'static str);

impl ResourceResolver for Resolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        Box::pin(async move {
            let id = args.as_json()[self.0].as_str().unwrap_or_default();
            let (cust, is_invoice) = match id.strip_suffix("-inv") {
                Some(c) => (c, true),
                None => (id, false),
            };
            let Some(t) = cust.split('-').next() else {
                return Err(ResolveError::NotFound);
            };
            if t != tenant.as_str() || !(CUSTOMERS.contains(&cust) || cust == "globex-c9") {
                return Err(ResolveError::NotFound);
            }
            let tenant_attr = AttrValue::Entity(uid("Tenant", t));
            Ok(if is_invoice {
                ResolvedResource::new(uid("Invoice", id), ResourceVersion::new("1"))
                    .with_attr("tenant", tenant_attr)
                    .with_attr("customer", AttrValue::Entity(uid("Customer", cust)))
                    .with_attr("amount_cents", AttrValue::Long(20_000))
                    .with_attr("refunded_cents", AttrValue::Long(0))
                    .with_parent(uid("Customer", cust))
            } else {
                ResolvedResource::new(uid("Customer", id), ResourceVersion::new("1"))
                    .with_attr("tenant", tenant_attr)
                    .with_parent(uid("Tenant", t))
            })
        })
    }
}

struct Op {
    action: ActionId,
    schema: Value,
    resolver: Resolver,
    executed: Arc<AtomicUsize>,
}

impl ProtectedOperation for Op {
    fn action(&self) -> &ActionId {
        &self.action
    }
    fn description(&self) -> &str {
        "test operation"
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn resolver(&self) -> &dyn ResourceResolver {
        &self.resolver
    }
    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        let mut fields = Attrs::new();
        if let Some(a) = args.as_json().get("amount_cents").and_then(Value::as_i64) {
            fields.insert("amount_cents".into(), AttrValue::Long(a));
        }
        AuthorizationContext::new(fields).map_err(|_| DenyReason::InvalidArguments)
    }
    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        _: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        Box::pin(async move {
            self.executed.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionOutput {
                content: json!({"resource": action.resource().id()}),
                downstream_ref: None,
            })
        })
    }
}

fn op(action: &str, id_field: &'static str, amount: bool, executed: &Arc<AtomicUsize>) -> Op {
    let id =
        json!({"type": "string", "minLength": 1, "maxLength": 64, "pattern": "^[A-Za-z0-9-]+$"});
    let mut props = json!({ id_field: id });
    let mut required = vec![id_field];
    if amount {
        props["amount_cents"] = json!({"type": "integer", "minimum": 1});
        required.push("amount_cents");
    }
    Op {
        action: ActionId::new(action).unwrap(),
        schema: json!({"type": "object", "properties": props, "required": required,
                       "additionalProperties": false}),
        resolver: Resolver(id_field),
        executed: executed.clone(),
    }
}

fn pdp() -> Arc<CedarPdp> {
    static PDP: std::sync::OnceLock<Arc<CedarPdp>> = std::sync::OnceLock::new();
    PDP.get_or_init(|| Arc::new(CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap()))
        .clone()
}

struct Fx {
    guard: Guard,
    store: Arc<MemoryGrantStore>,
    sink: Arc<MemorySink>,
    clock: Arc<TestClock>,
    executed: Arc<AtomicUsize>,
}

fn app() -> AppId {
    AppId::new("support-app").unwrap()
}

fn fixture() -> Fx {
    let executed = Arc::new(AtomicUsize::new(0));
    let mut reg = OperationRegistry::new();
    reg.register(Arc::new(op(
        "customer.read",
        "customer_id",
        false,
        &executed,
    )))
    .unwrap();
    reg.register(Arc::new(op("refund.issue", "invoice_id", true, &executed)))
        .unwrap();
    reg.register(Arc::new(op(
        "customer.export",
        "customer_id",
        false,
        &executed,
    )))
    .unwrap();
    reg.register_delegate().unwrap();
    let mut config = GrantStoreConfig::new(reg.action_ids());
    config.max_grants = 1_000_000;
    let store = Arc::new(MemoryGrantStore::new(config));
    let sink = Arc::new(MemorySink::default());
    let clock = Arc::new(TestClock(AtomicU64::new(START_MS)));
    let guard = Guard::new(
        reg,
        store.clone(),
        pdp(),
        Arc::new(Broker),
        sink.clone(),
        clock.clone(),
    );
    Fx {
        guard,
        store,
        sink,
        clock,
        executed,
    }
}

fn actions(a: &[&str]) -> BTreeSet<ActionId> {
    a.iter().map(|a| ActionId::new(*a).unwrap()).collect()
}

fn customers(c: &[&str]) -> Constraint {
    Constraint::Set(c.iter().map(|c| uid("Customer", c)).collect())
}

/// Root grant: read and refund on c1 and c2, refunds up to 100.00
/// (delegator limit 50.00), may delegate two levels.
fn root_spec() -> GrantSpec {
    GrantSpec {
        tenant: TenantId::new("acme").unwrap(),
        delegator: Delegator::new(
            UserId::new("acme-operator").unwrap(),
            "agent-operator",
            [("refund_limit_cents".to_owned(), AttrValue::Long(5_000))].into(),
        )
        .unwrap(),
        run_id: RunId::new("run-1").unwrap(),
        actions: actions(&["customer.read", "refund.issue", "guard.delegate"]),
        constraints: Constraints::new(BTreeMap::from([
            ("customers".to_owned(), customers(&["acme-c1", "acme-c2"])),
            ("max_refund_cents".to_owned(), Constraint::AtMost(10_000)),
        ]))
        .unwrap(),
        ttl_seconds: ROOT_TTL,
        max_depth: 2,
    }
}

impl Fx {
    fn issue(&self, spec: GrantSpec) -> (String, GrantId) {
        let issued = block_on(self.store.issue(&app(), spec, self.clock.now())).unwrap();
        (
            issued.handle.expose_secret().to_owned(),
            issued.grant.grant_id().clone(),
        )
    }

    fn call(&self, handle: &str, tool: &str, args: Value) -> CallOutcome {
        block_on(self.guard.call(handle, tool, args, CallMeta::default()))
    }

    fn delegate(&self, handle: &str, args: Value) -> CallOutcome {
        self.call(handle, "guard.delegate", args)
    }

    fn read(&self, handle: &str, customer: &str) -> CallOutcome {
        self.call(handle, "customer.read", json!({"customer_id": customer}))
    }

    fn refund(&self, handle: &str, customer: &str, amount: i64) -> CallOutcome {
        self.call(
            handle,
            "refund.issue",
            json!({"invoice_id": format!("{customer}-inv"), "amount_cents": amount}),
        )
    }
}

fn content(o: &CallOutcome) -> &Value {
    match &o.result {
        CallResult::Executed { content } => content,
        other => panic!("not executed: {other:?}"),
    }
}

fn child_handle(o: &CallOutcome) -> String {
    content(o)["handle"].as_str().unwrap().to_owned()
}

fn set_arg(name: &str, ids: &[&str]) -> Value {
    let set: Vec<Value> = ids
        .iter()
        .map(|i| json!({"type": "Support::Customer", "id": i}))
        .collect();
    json!({"name": name, "set": set})
}

fn read_only_on_c1() -> Value {
    json!({
        "actions": ["customer.read"],
        "constraints": [set_arg("customers", &["acme-c1"])],
        "ttl_seconds": 60
    })
}

// ------------------------------------------------------------------- tests

/// A narrower delegation returns a working child handle whose authority is the requested subset, and records allow then executed with the child grant id.
#[test]
fn narrower_delegation_yields_a_working_child() {
    let fx = fixture();
    let (root, root_id) = fx.issue(root_spec());
    let out = fx.delegate(&root, read_only_on_c1());
    let c = content(&out);
    let child = child_handle(&out);
    assert!(child.starts_with(HANDLE_PREFIX));
    assert_ne!(child, root);
    assert_eq!(c["actions"], json!(["customer.read"]));
    assert_eq!(c["max_depth"], 0);
    assert_eq!(c["expires_at"], json!(START_MS + 60_000));

    let recs = fx.sink.records();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].decision, Some(DecisionKind::Allow));
    assert_eq!(recs[0].action.as_ref().unwrap().as_str(), "guard.delegate");
    assert_eq!(recs[1].outcome, Some(Outcome::Executed));
    let child_id = c["grant_id"].as_str().unwrap();
    assert_eq!(recs[1].downstream_ref.as_deref(), Some(child_id));

    assert!(!fx.read(&child, "acme-c1").is_error());
    assert_eq!(fx.read(&child, "acme-c2").code(), Some("policy_deny"));
    assert_eq!(
        fx.refund(&child, "acme-c1", 100).code(),
        Some("action_not_granted")
    );
    // Identity is copied from the parent; only grant and agent differ.
    let id = fx.sink.records().last().unwrap().identity.clone().unwrap();
    let root_identity = recs[0].identity.clone().unwrap();
    assert_eq!(
        id.grant_chain,
        vec![GrantId::new(child_id).unwrap(), root_id]
    );
    assert_eq!(
        (&id.tenant, &id.delegator, &id.run_id, &id.issuer),
        (
            &root_identity.tenant,
            &root_identity.delegator,
            &root_identity.run_id,
            &root_identity.issuer
        )
    );
    assert_ne!(id.agent, root_identity.agent);
}

/// Omitted constraints are inherited unchanged, so a child that names none has exactly its parent's constraints.
#[test]
fn omitted_constraints_are_inherited() {
    let fx = fixture();
    let (root, _) = fx.issue(root_spec());
    let out = fx.delegate(
        &root,
        json!({"actions": ["customer.read", "refund.issue"], "ttl_seconds": 60}),
    );
    let c = content(&out);
    assert_eq!(
        c["constraints"],
        serde_json::to_value(root_spec().constraints).unwrap()
    );
    let child = child_handle(&out);
    assert!(!fx.refund(&child, "acme-c2", 1_000).is_error());
    assert_eq!(fx.read(&child, "acme-c3").code(), Some("policy_deny"));
}

/// Each widening attempt (actions, customers, amount, expiry, depth, constraint kind) is a `policy_deny` decision and creates no grant.
#[test]
fn widening_attempts_are_denied() {
    let fx = fixture();
    let (root, _) = fx.issue(root_spec());
    let cases = [
        (
            "broader actions",
            json!({"actions": ["customer.read", "customer.export"], "ttl_seconds": 60}),
        ),
        (
            "unknown action",
            json!({"actions": ["customer.read", "admin.everything"], "ttl_seconds": 60}),
        ),
        (
            "broader customers",
            json!({"actions": ["customer.read"], "ttl_seconds": 60,
                   "constraints": [set_arg("customers", &["acme-c1", "acme-c3"])]}),
        ),
        (
            "higher amount",
            json!({"actions": ["refund.issue"], "ttl_seconds": 60,
                   "constraints": [{"name": "max_refund_cents", "at_most": 10_001}]}),
        ),
        (
            "later expiry",
            json!({"actions": ["customer.read"], "ttl_seconds": ROOT_TTL + 1}),
        ),
        (
            "huge expiry",
            json!({"actions": ["customer.read"], "ttl_seconds": u64::MAX}),
        ),
        (
            "equal depth",
            json!({"actions": ["customer.read"], "ttl_seconds": 60, "max_depth": 2}),
        ),
        (
            "greater depth",
            json!({"actions": ["customer.read"], "ttl_seconds": 60, "max_depth": 255}),
        ),
        (
            "constraint kind change",
            json!({"actions": ["customer.read"], "ttl_seconds": 60,
                   "constraints": [{"name": "customers", "at_most": 1}]}),
        ),
    ];
    for (name, args) in cases {
        let before = fx.sink.records().len();
        let out = fx.delegate(&root, args);
        assert_eq!(out.code(), Some("policy_deny"), "{name}: {out:?}");
        let recs = fx.sink.records();
        assert_eq!(recs.len(), before + 1, "{name}: only a decision record");
        let rec = recs.last().unwrap();
        assert_eq!(rec.decision, Some(DecisionKind::Deny), "{name}");
        assert_eq!(rec.reason, Some(DenyReason::PolicyDeny), "{name}");
        assert_eq!(rec.outcome, Some(Outcome::Blocked), "{name}");
    }
    // The root can still delegate its 16 children: none were spent above.
    for _ in 0..16 {
        assert!(!fx.delegate(&root, read_only_on_c1()).is_error());
    }
}

/// A grant whose actions lack `guard.delegate`, or whose depth is 0, cannot delegate at all.
#[test]
fn delegate_requires_the_action_and_depth() {
    let fx = fixture();
    let mut no_action = root_spec();
    no_action
        .actions
        .remove(&ActionId::new("guard.delegate").unwrap());
    let (h, _) = fx.issue(no_action);
    assert_eq!(
        fx.delegate(&h, read_only_on_c1()).code(),
        Some("action_not_granted")
    );
    let mut no_depth = root_spec();
    no_depth.max_depth = 0;
    let (h, _) = fx.issue(no_depth);
    assert_eq!(
        fx.delegate(&h, read_only_on_c1()).code(),
        Some("policy_deny")
    );
}

/// A child may delegate further only if it holds `guard.delegate` and depth remains; a depth-0 grandchild cannot.
#[test]
fn multi_hop_delegation_is_bounded_by_depth() {
    let fx = fixture();
    let (root, root_id) = fx.issue(root_spec());
    let child = child_handle(&fx.delegate(
        &root,
        json!({"actions": ["customer.read", "guard.delegate"], "ttl_seconds": 120, "max_depth": 1}),
    ));
    let grandchild_out = fx.delegate(
        &child,
        json!({"actions": ["customer.read", "guard.delegate"], "ttl_seconds": 60,
               "constraints": [set_arg("customers", &["acme-c2"])]}),
    );
    let grandchild = child_handle(&grandchild_out);
    assert!(!fx.read(&grandchild, "acme-c2").is_error());
    assert_eq!(fx.read(&grandchild, "acme-c1").code(), Some("policy_deny"));
    assert_eq!(
        fx.delegate(
            &grandchild,
            json!({"actions": ["customer.read"], "ttl_seconds": 1})
        )
        .code(),
        Some("policy_deny")
    );
    // A child cannot outlive its parent: 121 s exceeds the child's 120 s.
    assert_eq!(
        fx.delegate(
            &child,
            json!({"actions": ["customer.read"], "ttl_seconds": 121})
        )
        .code(),
        Some("policy_deny")
    );

    // Revoking the root disables every descendant, and delegation from them.
    block_on(fx.store.revoke(&app(), &root_id, fx.clock.now())).unwrap();
    assert_eq!(
        fx.read(&grandchild, "acme-c2").code(),
        Some("grant_revoked")
    );
    assert_eq!(fx.read(&child, "acme-c1").code(), Some("grant_revoked"));
    assert_eq!(
        fx.delegate(&child, read_only_on_c1()).code(),
        Some("grant_revoked")
    );
}

/// When the parent expires, its children stop working with it.
#[test]
fn child_expires_with_its_parent() {
    let fx = fixture();
    let (root, _) = fx.issue(root_spec());
    let child = child_handle(&fx.delegate(
        &root,
        json!({"actions": ["customer.read"], "ttl_seconds": ROOT_TTL}),
    ));
    fx.clock
        .0
        .store(START_MS + ROOT_TTL * 1000, Ordering::SeqCst);
    assert_eq!(fx.read(&child, "acme-c1").code(), Some("grant_expired"));
}

/// Malformed delegation arguments are `invalid_arguments`: unknown fields, forged tenant, duplicate or ambiguous constraints, bad ids.
#[test]
fn malformed_arguments_are_invalid() {
    let fx = fixture();
    let (root, _) = fx.issue(root_spec());
    for args in [
        json!({"actions": ["customer.read"], "ttl_seconds": 60, "tenant": "globex"}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60, "run_id": "other"}),
        json!({"actions": [], "ttl_seconds": 60}),
        json!({"actions": ["customer.read"], "ttl_seconds": 0}),
        json!({"actions": ["customer.read"]}),
        json!({"actions": ["customer read"], "ttl_seconds": 60}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60, "max_depth": 256}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60,
               "constraints": [{"name": "customers", "at_most": 1, "set": []}]}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60,
               "constraints": [{"name": "customers"}]}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60,
               "constraints": [{"name": "max_refund_cents", "at_most": 1},
                               {"name": "max_refund_cents", "at_most": 2}]}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60,
               "constraints": [{"name": "bad-name", "at_most": 1}]}),
        json!({"actions": ["customer.read"], "ttl_seconds": 60,
               "constraints": [{"name": "customers", "set": [{"type": "Support Customer", "id": "x"}]}]}),
    ] {
        let out = fx.delegate(&root, args.clone());
        assert_eq!(out.code(), Some("invalid_arguments"), "{args}");
    }
}

/// The child handle appears only in the delegate result: never in audit records, other results, or `Debug` output (`tracing` is covered by the testkit canary).
#[test]
fn child_handle_appears_only_in_the_delegate_result() {
    let fx = fixture();
    let (root, _) = fx.issue(root_spec());
    let out = fx.delegate(
        &root,
        json!({"actions": ["customer.read", "refund.issue", "guard.delegate"],
                   "ttl_seconds": 60, "max_depth": 1}),
    );
    let child = child_handle(&out);
    let mut others = vec![
        fx.read(&child, "acme-c1"),
        fx.read(&child, "acme-c3"),
        fx.refund(&child, "acme-c1", 9_000),
        fx.delegate(&child, read_only_on_c1()),
        fx.delegate(
            &child,
            json!({"actions": ["customer.export"], "ttl_seconds": 1}),
        ),
        fx.delegate(
            &child,
            json!({"actions": ["customer.read"], "ttl_seconds": 1, "x": 1}),
        ),
    ];
    let grandchild = child_handle(&others[3]);
    others.push(fx.read(&grandchild, "acme-c1"));
    let codes: Vec<_> = others
        .iter()
        .map(|o| o.code().unwrap_or("executed"))
        .collect();
    assert_eq!(
        codes,
        [
            "executed",
            "policy_deny",
            "approval_required",
            "executed",
            "policy_deny",
            "invalid_arguments",
            "executed"
        ]
    );
    let mut haystacks = vec![
        serde_json::to_string(&fx.sink.records()).unwrap(),
        format!("{:?}", fx.sink.records()),
        format!("{:?}", fx.guard),
        format!("{:?}", fx.store),
    ];
    haystacks.extend(
        others
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 3)
            .map(|(_, o)| format!("{o:?}\n{}", serde_json::to_string(o).unwrap())),
    );
    for (i, h) in haystacks.iter().enumerate() {
        for secret in [&root, &child, &grandchild] {
            assert!(
                !h.contains(secret.as_str()),
                "handle leaked in haystack {i}"
            );
            // The random part alone must not leak either.
            assert!(!h.contains(&secret[HANDLE_PREFIX.len()..]), "haystack {i}");
        }
    }
}

// ---------------------------------------------------------------- property

const ACTION_POOL: [&str; 4] = [
    "customer.read",
    "refund.issue",
    "customer.export",
    "guard.delegate",
];

#[derive(Debug, Clone)]
struct ParentGen {
    actions: Vec<bool>,
    customers: Vec<bool>,
    max_refund: i64,
    ttl: u64,
    depth: u8,
}

#[derive(Debug, Clone)]
struct ChildGen {
    actions: Vec<bool>,
    customers: Option<Vec<bool>>,
    max_refund: Option<i64>,
    extra: bool,
    ttl: u64,
    depth: u8,
}

#[derive(Debug, Clone)]
struct RequestGen {
    refund: bool,
    customer: usize,
    amount: i64,
}

fn parent_gen() -> impl Strategy<Value = ParentGen> {
    // Biased toward wide parents and narrow children, so that both accepted
    // and rejected delegations, and allowed child calls, are common.
    (
        proptest::collection::vec(proptest::bool::weighted(0.8), 3),
        proptest::collection::vec(proptest::bool::weighted(0.7), 3),
        0i64..=12_000,
        1u64..=ROOT_TTL,
        prop_oneof![1 => Just(0u8), 6 => 1u8..=3],
    )
        .prop_map(|(actions, customers, max_refund, ttl, depth)| ParentGen {
            actions,
            customers,
            max_refund,
            ttl,
            depth,
        })
}

fn child_gen() -> impl Strategy<Value = ChildGen> {
    (
        proptest::collection::vec(proptest::bool::weighted(0.3), 4),
        proptest::option::of(proptest::collection::vec(proptest::bool::weighted(0.3), 4)),
        proptest::option::of(0i64..=15_000),
        proptest::bool::weighted(0.05),
        prop_oneof![3 => 1u64..=30, 1 => 1u64..=ROOT_TTL + 100],
        prop_oneof![3 => Just(0u8), 1 => 0u8..=4],
    )
        .prop_map(
            |(actions, customers, max_refund, extra, ttl, depth)| ChildGen {
                actions,
                customers,
                max_refund,
                extra,
                ttl,
                depth,
            },
        )
}

fn request_gen() -> impl Strategy<Value = RequestGen> {
    (any::<bool>(), 0usize..4, 1i64..=12_000).prop_map(|(refund, customer, amount)| RequestGen {
        refund,
        customer,
        amount,
    })
}

/// `acme-c1..3` plus another tenant's customer.
const CUSTOMER_POOL: [&str; 4] = ["acme-c1", "acme-c2", "acme-c3", "globex-c9"];

fn pick<'a>(pool: &[&'a str], mask: &[bool]) -> Vec<&'a str> {
    pool.iter()
        .zip(mask)
        .filter(|(_, m)| **m)
        .map(|(p, _)| *p)
        .collect()
}

fn parent_spec(p: &ParentGen) -> GrantSpec {
    let mut s = root_spec();
    let mut acts = pick(&ACTION_POOL[..2], &p.actions[..2]);
    if acts.is_empty() {
        acts.push("customer.read");
    }
    if p.actions[2] || p.depth > 0 {
        acts.push("guard.delegate");
    }
    s.actions = actions(&acts);
    s.constraints = Constraints::new(BTreeMap::from([
        (
            "customers".to_owned(),
            customers(&pick(&CUSTOMERS, &p.customers)),
        ),
        (
            "max_refund_cents".to_owned(),
            Constraint::AtMost(p.max_refund),
        ),
    ]))
    .unwrap();
    s.ttl_seconds = p.ttl;
    s.max_depth = p.depth;
    s
}

fn child_args(c: &ChildGen) -> Value {
    let mut acts = pick(&ACTION_POOL, &c.actions);
    if acts.is_empty() {
        acts.push("customer.read");
    }
    let mut cons = Vec::new();
    if let Some(m) = &c.customers {
        cons.push(set_arg("customers", &pick(&CUSTOMER_POOL, m)));
    }
    if let Some(m) = c.max_refund {
        cons.push(json!({"name": "max_refund_cents", "at_most": m}));
    }
    if c.extra {
        cons.push(json!({"name": "extra_limit", "at_most": 1}));
    }
    json!({"actions": acts, "constraints": cons, "ttl_seconds": c.ttl, "max_depth": c.depth})
}

/// The independent oracle for "narrower or equal" (MILESTONE_18 §8.3).
fn narrows(p: &ParentGen, c: &ChildGen) -> bool {
    let spec = parent_spec(p);
    let mut acts = pick(&ACTION_POOL, &c.actions);
    if acts.is_empty() {
        acts.push("customer.read");
    }
    let parent_customers = pick(&CUSTOMERS, &p.customers);
    actions(&acts).is_subset(&spec.actions)
        && c.customers.as_ref().is_none_or(|m| {
            pick(&CUSTOMER_POOL, m)
                .iter()
                .all(|x| parent_customers.contains(x))
        })
        && c.max_refund.is_none_or(|m| m <= p.max_refund)
        && c.ttl <= p.ttl
        && p.depth >= 1
        && c.depth < p.depth
}

/// Over 10,000 random parents, delegations, and requests: a delegation succeeds exactly when it narrows, and whatever a child may execute its parent may execute.
#[test]
fn child_authority_implies_parent_authority() {
    let fx = fixture();
    let mut runner = TestRunner::new(Config {
        cases: 10_000,
        failure_persistence: None,
        ..Config::default()
    });
    let delegated = AtomicUsize::new(0);
    let child_executed = AtomicUsize::new(0);
    runner
        .run(&(parent_gen(), child_gen(), request_gen()), |(p, c, r)| {
            let (parent, _) = fx.issue(parent_spec(&p));
            let out = fx.delegate(&parent, child_args(&c));
            let expect = narrows(&p, &c);
            prop_assert_eq!(!out.is_error(), expect, "delegate: {:?}", out);
            if !expect {
                let held = p.actions[2] || p.depth > 0;
                let code = if held {
                    "policy_deny"
                } else {
                    "action_not_granted"
                };
                prop_assert_eq!(out.code(), Some(code));
                return Ok(());
            }
            delegated.fetch_add(1, Ordering::Relaxed);
            let child = child_handle(&out);
            let cust = CUSTOMER_POOL[r.customer];
            let request = |h: &str| {
                if r.refund {
                    fx.refund(h, cust, r.amount)
                } else {
                    fx.read(h, cust)
                }
            };
            if !request(&child).is_error() {
                child_executed.fetch_add(1, Ordering::Relaxed);
                let parent_out = request(&parent);
                prop_assert!(
                    !parent_out.is_error(),
                    "child allowed, parent {:?}",
                    parent_out
                );
            }
            Ok(())
        })
        .unwrap();
    // The property must not hold vacuously.
    // Typically about 2,000 and 300.
    assert!(delegated.load(Ordering::Relaxed) > 1_000);
    assert!(child_executed.load(Ordering::Relaxed) > 150);
    assert!(fx.executed.load(Ordering::Relaxed) >= 2 * child_executed.load(Ordering::Relaxed));
}
