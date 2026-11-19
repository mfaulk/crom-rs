//! Pipeline tests (#351): each MILESTONE_18 §10.2 step's failure mode, with
//! counters proving the resolver, broker, and executor did not run.

use std::collections::BTreeMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crom_guard::*;
use futures::executor::block_on;
use futures::FutureExt;
use serde_json::{json, Value};

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");
const SECRET: &str = "sk-pipeline-CANARY-91d2";
const START_MS: u64 = 1_000_000;

// ---------------------------------------------------------------- fixtures

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

impl TestClock {
    fn set_secs_after_start(&self, s: u64) {
        self.0.store(START_MS + s * 1000, Ordering::SeqCst);
    }
}

type Hook = Arc<dyn Fn(&AuditRecord) + Send + Sync>;

#[derive(Default)]
struct MemorySink {
    records: Mutex<Vec<AuditRecord>>,
    fail: AtomicBool,
    hook: Mutex<Option<Hook>>,
}

impl AuditSink for MemorySink {
    fn append<'a>(
        &'a self,
        record: &'a AuditRecord,
    ) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(AuditError::Io(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "disk full",
                )));
            }
            let seq = {
                let mut r = self.records.lock().unwrap();
                r.push(record.clone());
                r.len() as u64
            };
            // Cloned out, so a hook may start another call on this sink.
            let hook = self.hook.lock().unwrap().clone();
            if let Some(h) = hook {
                h(record);
            }
            Ok(AuditReceipt {
                seq,
                head_hash: format!("{seq:064x}"),
            })
        })
    }
}

/// Storage that accepts `ok` lines, then fails like a full disk.
struct FlakyStorage {
    ok: usize,
}

impl AuditStorage for FlakyStorage {
    fn append_durable(&mut self, _line: &[u8]) -> io::Result<()> {
        if self.ok == 0 {
            return Err(io::Error::new(io::ErrorKind::StorageFull, "disk full"));
        }
        self.ok -= 1;
        Ok(())
    }
}

#[derive(Default)]
struct Probe {
    contexts: AtomicUsize,
    resolved: AtomicUsize,
    leased: AtomicUsize,
    executed: AtomicUsize,
    exec_decision_ids: Mutex<Vec<String>>,
}

struct TestBroker {
    probe: Arc<Probe>,
    fail: bool,
}

impl CredentialBroker for TestBroker {
    fn lease<'a>(
        &'a self,
        _action: &'a ActionId,
    ) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async move {
            self.probe.leased.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(BrokerError::Unavailable);
            }
            Ok(CredentialLease::new("test-cred", SECRET))
        })
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Resolve {
    Data,
    Unavailable,
    Panic,
    Hang,
}

#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Ok,
    Reserved,
    Panic,
}

#[derive(Clone, PartialEq)]
enum Exec {
    Ok,
    Precondition,
    Failed(String),
    Panic,
    /// Returns success, then panics when its future is dropped.
    DropPanic,
}

/// A future whose `Drop` panics.
struct DropBomb<'a>(BoxFuture<'a, Result<ExecutionOutput, ExecuteError>>);

impl std::future::Future for DropBomb<'_> {
    type Output = Result<ExecutionOutput, ExecuteError>;
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

impl Drop for DropBomb<'_> {
    fn drop(&mut self) {
        panic!("executor future drop bug");
    }
}

/// Refunds of this amount hang in `execute`, so a test can cancel mid-execute.
const HANG_AMOUNT: i64 = 7;

fn uid(ty: &str, id: &str) -> EntityUid {
    EntityUid::new(format!("Support::{ty}"), id).unwrap()
}

/// Application data: id → (tenant, resource).
fn data() -> BTreeMap<String, (String, ResolvedResource)> {
    let customer = |id: &str, t: &str| {
        (
            t.to_owned(),
            ResolvedResource::new(uid("Customer", id), ResourceVersion::new("1"))
                .with_attr("tenant", AttrValue::Entity(uid("Tenant", t)))
                .with_parent(uid("Tenant", t)),
        )
    };
    let invoice = |id: &str, t: &str, c: &str| {
        (
            t.to_owned(),
            ResolvedResource::new(uid("Invoice", id), ResourceVersion::new("1"))
                .with_attr("tenant", AttrValue::Entity(uid("Tenant", t)))
                .with_attr("customer", AttrValue::Entity(uid("Customer", c)))
                .with_attr("amount_cents", AttrValue::Long(20_000))
                .with_attr("refunded_cents", AttrValue::Long(0))
                .with_parent(uid("Customer", c)),
        )
    };
    [
        ("acme-cus-001", customer("acme-cus-001", "acme")),
        ("acme-cus-002", customer("acme-cus-002", "acme")),
        ("globex-cus-001", customer("globex-cus-001", "globex")),
        (
            "acme-inv-001",
            invoice("acme-inv-001", "acme", "acme-cus-001"),
        ),
        (
            "acme-inv-002",
            invoice("acme-inv-002", "acme", "acme-cus-002"),
        ),
        (
            "globex-inv-001",
            invoice("globex-inv-001", "globex", "globex-cus-001"),
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect()
}

struct TestResolver {
    id_field: &'static str,
    mode: Resolve,
    probe: Arc<Probe>,
}

impl ResourceResolver for TestResolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        Box::pin(async move {
            self.probe.resolved.fetch_add(1, Ordering::SeqCst);
            match self.mode {
                Resolve::Unavailable => return Err(ResolveError::Unavailable),
                Resolve::Panic => panic!("resolver bug"),
                Resolve::Hang => futures::future::pending::<()>().await,
                Resolve::Data => {}
            }
            let id = args.as_json()[self.id_field].as_str().unwrap_or_default();
            match data().remove(id) {
                Some((t, r)) if t == tenant.as_str() => Ok(r),
                _ => Err(ResolveError::NotFound),
            }
        })
    }
}

struct TestOp {
    action: ActionId,
    schema: Value,
    logged: &'static [&'static str],
    resolver: Box<dyn ResourceResolver>,
    ctx: Ctx,
    exec: Exec,
    probe: Arc<Probe>,
}

impl ProtectedOperation for TestOp {
    fn action(&self) -> &ActionId {
        &self.action
    }
    fn description(&self) -> &str {
        "test operation"
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn logged_fields(&self) -> &[&'static str] {
        self.logged
    }
    fn resolver(&self) -> &dyn ResourceResolver {
        self.resolver.as_ref()
    }
    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        self.probe.contexts.fetch_add(1, Ordering::SeqCst);
        match self.ctx {
            Ctx::Panic => panic!("context bug"),
            Ctx::Reserved => {
                AuthorizationContext::new([("grant".to_owned(), AttrValue::Bool(true))].into())
                    .map_err(|_| DenyReason::InvalidArguments)
            }
            Ctx::Ok => match args.as_json().get("amount_cents").and_then(Value::as_i64) {
                Some(a) => AuthorizationContext::new(
                    [("amount_cents".to_owned(), AttrValue::Long(a))].into(),
                )
                .map_err(|_| DenyReason::InvalidArguments),
                None if self.action.as_str() == "refund.issue" => Err(DenyReason::InvalidArguments),
                None => Ok(AuthorizationContext::default()),
            },
        }
    }
    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        credential: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        let fut = Box::pin(async move {
            self.probe.executed.fetch_add(1, Ordering::SeqCst);
            self.probe
                .exec_decision_ids
                .lock()
                .unwrap()
                .push(action.decision_id().as_str().to_owned());
            assert!(credential.with_secret(|s| s == SECRET));
            if action.args().as_json().get("amount_cents") == Some(&json!(HANG_AMOUNT)) {
                futures::future::pending::<()>().await;
            }
            match &self.exec {
                Exec::Ok | Exec::DropPanic => Ok(ExecutionOutput {
                    content: json!({"resource": action.resource().id()}),
                    downstream_ref: Some("ref-1".into()),
                }),
                Exec::Precondition => Err(ExecuteError::Precondition),
                Exec::Failed(d) => Err(ExecuteError::Failed(d.clone())),
                Exec::Panic => panic!("executor bug"),
            }
        });
        if self.exec == Exec::DropPanic {
            return Box::pin(DropBomb(fut));
        }
        fut
    }
}

fn id_schema() -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": 128, "pattern": "^[A-Za-z0-9._:@-]+$"})
}

fn refund_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "invoice_id": id_schema(),
            "amount_cents": {"type": "integer", "minimum": 1}
        },
        "required": ["invoice_id", "amount_cents"],
        "additionalProperties": false
    })
}

fn customer_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"customer_id": id_schema()},
        "required": ["customer_id"],
        "additionalProperties": false
    })
}

struct Opts {
    resolve: Resolve,
    timeout_resolver: bool,
    ctx: Ctx,
    exec: Exec,
    broker_fails: bool,
    actions: &'static [&'static str],
    sink: Option<Arc<dyn AuditSink>>,
    guard_timer: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            resolve: Resolve::Data,
            timeout_resolver: false,
            ctx: Ctx::Ok,
            exec: Exec::Ok,
            broker_fails: false,
            actions: &["customer.read", "refund.issue", "customer.export"],
            sink: None,
            guard_timer: false,
        }
    }
}

struct Fixture {
    guard: Arc<Guard>,
    store: Arc<MemoryGrantStore>,
    sink: Arc<MemorySink>,
    clock: Arc<TestClock>,
    probe: Arc<Probe>,
    handle: String,
    grant_id: GrantId,
}

fn app() -> AppId {
    AppId::new("support-app").unwrap()
}

fn registry(o: &Opts, probe: &Arc<Probe>) -> OperationRegistry {
    let mut reg = OperationRegistry::new();
    for (action, schema, id_field, logged) in [
        (
            "refund.issue",
            refund_schema(),
            "invoice_id",
            &["invoice_id", "amount_cents"][..],
        ),
        (
            "customer.read",
            customer_schema(),
            "customer_id",
            &["customer_id"][..],
        ),
        (
            "customer.export",
            customer_schema(),
            "customer_id",
            &["customer_id"][..],
        ),
    ] {
        let inner = TestResolver {
            id_field,
            mode: o.resolve,
            probe: probe.clone(),
        };
        let resolver: Box<dyn ResourceResolver> = if o.timeout_resolver {
            Box::new(TimeoutResolver::new(inner, || Box::pin(async {})))
        } else {
            Box::new(inner)
        };
        reg.register(Arc::new(TestOp {
            action: ActionId::new(action).unwrap(),
            schema,
            logged,
            resolver,
            ctx: o.ctx,
            exec: o.exec.clone(),
            probe: probe.clone(),
        }))
        .unwrap();
    }
    reg
}

fn spec(actions: &[&str]) -> GrantSpec {
    GrantSpec {
        tenant: TenantId::new("acme").unwrap(),
        delegator: Delegator::new(
            UserId::new("acme-agent-operator").unwrap(),
            "agent-operator",
            [("refund_limit_cents".to_owned(), AttrValue::Long(5_000))].into(),
        )
        .unwrap(),
        run_id: RunId::new("run-1").unwrap(),
        actions: actions.iter().map(|a| ActionId::new(*a).unwrap()).collect(),
        constraints: Constraints::new(BTreeMap::from([
            (
                "customers".to_owned(),
                Constraint::Set([uid("Customer", "acme-cus-001")].into()),
            ),
            ("max_refund_cents".to_owned(), Constraint::AtMost(10_000)),
        ]))
        .unwrap(),
        ttl_seconds: 600,
        max_depth: 0,
    }
}

fn pdp(policies: &str) -> Arc<CedarPdp> {
    Arc::new(CedarPdp::from_sources("Support", SCHEMA, policies).unwrap())
}

fn fixture(o: Opts) -> Fixture {
    traces();
    let probe = Arc::new(Probe::default());
    let reg = registry(&o, &probe);
    let store = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(
        reg.action_ids(),
    )));
    let clock = Arc::new(TestClock(AtomicU64::new(START_MS)));
    let issued = block_on(store.issue(&app(), spec(o.actions), clock.now())).unwrap();
    let sink = Arc::new(MemorySink::default());
    let audit: Arc<dyn AuditSink> = o.sink.unwrap_or_else(|| sink.clone());
    let mut guard = Guard::new(
        reg,
        store.clone(),
        pdp(POLICIES),
        Arc::new(TestBroker {
            probe: probe.clone(),
            fail: o.broker_fails,
        }),
        audit,
        clock.clone(),
    );
    if o.guard_timer {
        guard = guard.with_resolve_timer(|| Box::pin(async {}));
    }
    Fixture {
        guard: Arc::new(guard),
        store,
        sink,
        clock,
        probe,
        handle: issued.handle.expose_secret().to_owned(),
        grant_id: issued.grant.grant_id().clone(),
    }
}

fn refund(amount: i64) -> Value {
    json!({"invoice_id": "acme-inv-001", "amount_cents": amount})
}

impl Fixture {
    fn call(&self, tool: &str, args: Value) -> CallOutcome {
        self.call_with(tool, args, CallMeta::default())
    }

    fn call_with(&self, tool: &str, args: Value, meta: CallMeta) -> CallOutcome {
        block_on(self.guard.call(&self.handle, tool, args, meta))
    }

    fn records(&self) -> Vec<AuditRecord> {
        self.sink.records.lock().unwrap().clone()
    }

    /// The call's records, checking exactly one decision and at most one
    /// outcome record, decision first.
    fn records_for(&self, out: &CallOutcome) -> (AuditRecord, Option<AuditRecord>) {
        let rs: Vec<_> = self
            .records()
            .into_iter()
            .filter(|r| r.decision_id == out.decision_id)
            .collect();
        assert!(!rs.is_empty() && rs.len() <= 2, "{rs:?}");
        assert_eq!(rs[0].record, RecordKind::Decision);
        let outcome = rs.get(1).cloned();
        if let Some(o) = &outcome {
            assert_eq!(o.record, RecordKind::Outcome);
        }
        (rs[0].clone(), outcome)
    }

    fn executed(&self) -> usize {
        self.probe.executed.load(Ordering::SeqCst)
    }
    fn resolved(&self) -> usize {
        self.probe.resolved.load(Ordering::SeqCst)
    }
    fn leased(&self) -> usize {
        self.probe.leased.load(Ordering::SeqCst)
    }

    /// Asserts a pre-execution deny with `code`: one decision record, no
    /// outcome record, nothing leased or executed.
    fn assert_denied(&self, out: &CallOutcome, code: &str) -> AuditRecord {
        self.assert_denied_as(out, code, code)
    }

    /// Like [`assert_denied`](Self::assert_denied), for denials the agent
    /// sees as `agent_code` but the audit record keeps as `recorded`.
    fn assert_denied_as(&self, out: &CallOutcome, agent_code: &str, code: &str) -> AuditRecord {
        assert_eq!(
            out.result,
            CallResult::Denied {
                code: leak(agent_code)
            }
        );
        assert_eq!(self.executed(), 0);
        assert_eq!(self.leased(), 0);
        let (d, o) = self.records_for(out);
        assert!(o.is_none());
        assert_eq!(d.decision, Some(DecisionKind::Deny));
        assert_eq!(d.outcome, Some(Outcome::Blocked));
        assert_eq!(d.reason.as_ref().map(DenyReason::code), Some(code));
        let pos = self
            .records()
            .iter()
            .position(|r| r.decision_id == out.decision_id)
            .unwrap();
        assert_eq!(out.audit_head.as_ref().map(|h| h.seq), Some(pos as u64 + 1));
        d
    }

    fn revoke_on_allow(&self) {
        let store = self.store.clone();
        let grant = self.grant_id.clone();
        let clock = self.clock.clone();
        *self.sink.hook.lock().unwrap() = Some(Arc::new(move |r| {
            if r.record == RecordKind::Decision && r.decision == Some(DecisionKind::Allow) {
                store
                    .revoke(&app(), &grant, clock.now())
                    .now_or_never()
                    .unwrap()
                    .unwrap();
            }
        }));
    }
}

fn leak(s: &str) -> &'static str {
    Box::leak(s.to_owned().into_boxed_str())
}

// ------------------------------------------------------------------ tests

/// An allowed refund executes exactly once with the call's decision id, after a durable decision record and before one outcome record.
#[test]
fn allowed_call_executes_once_between_decision_and_outcome() {
    let f = fixture(Opts::default());
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(
        out.result,
        CallResult::Executed {
            content: json!({"resource": "acme-inv-001"})
        }
    );
    assert_eq!(f.executed(), 1);
    assert_eq!(
        *f.probe.exec_decision_ids.lock().unwrap(),
        [out.decision_id.as_str()]
    );
    let (d, o) = f.records_for(&out);
    let o = o.expect("outcome record");
    assert_eq!(d.decision, Some(DecisionKind::Allow));
    assert_eq!(d.enforcement, Some(Enforcement::GuardPolicy));
    assert_eq!(d.outcome, None);
    assert_eq!(
        d.determining_policies,
        [PolicyId::new("refund-within-limits").unwrap()]
    );
    assert!(d.policy_set_hash.is_some() && d.schema_hash.is_some());
    assert_eq!(d.resource, Some(uid("Invoice", "acme-inv-001")));
    assert_eq!(d.identity.as_ref().unwrap().grant_id, f.grant_id);
    assert_eq!(o.outcome, Some(Outcome::Executed));
    assert_eq!(o.downstream_ref.as_deref(), Some("ref-1"));
    assert_eq!(out.audit_head.as_ref().unwrap().seq, 2);
}

/// Missing, malformed, and unknown session handles are `no_grant` with no identity recorded.
#[test]
fn bad_handles_are_no_grant() {
    let f = fixture(Opts::default());
    for bearer in [
        String::new(),
        "Bearer x".to_owned(),
        format!("gsh_{}", "0".repeat(64)),
        f.handle.to_uppercase(),
    ] {
        let out =
            block_on(
                f.guard
                    .call(&bearer, "refund.issue", refund(1_000), CallMeta::default()),
            );
        let d = f.assert_denied(&out, "no_grant");
        assert_eq!(d.identity, None);
        assert_eq!(d.enforcement, Some(Enforcement::GuardSession));
    }
    assert_eq!(f.resolved(), 0);
}

/// Validity is judged on the injected clock: the same handle is denied before `not_before` and at expiry, whatever the wall clock says.
#[test]
fn now_comes_from_the_injected_clock() {
    let f = fixture(Opts::default());
    f.clock.0.store(START_MS - 1, Ordering::SeqCst);
    f.assert_denied(
        &f.call("refund.issue", refund(1_000)),
        "grant_not_yet_valid",
    );
    f.clock.set_secs_after_start(600);
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "grant_expired");
    f.clock.set_secs_after_start(599);
    assert!(!f.call("refund.issue", refund(1_000)).is_error());
}

/// A revoked grant is `grant_revoked` before any resolution.
#[test]
fn revoked_grant_denied() {
    let f = fixture(Opts::default());
    block_on(f.store.revoke(&app(), &f.grant_id, f.clock.now())).unwrap();
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "grant_revoked");
    assert_eq!(f.resolved(), 0);
}

/// Tool names match exactly: case variants, whitespace, near-misses, and Guard-reserved names are recorded as `unknown_action` before resolution, while the agent sees `action_not_granted`.
#[test]
fn unknown_tool_names_denied_before_resolution() {
    let f = fixture(Opts::default());
    for tool in [
        "Refund.issue",
        "refund.Issue",
        " refund.issue",
        "refund.issue ",
        "refund_issue",
        "refund.issue\u{200b}",
        "guard.delegate",
        "",
    ] {
        let d = f.assert_denied_as(
            &f.call(tool, refund(1_000)),
            "action_not_granted",
            "unknown_action",
        );
        assert_eq!(d.untrusted.requested_tool.as_deref(), Some(tool));
        assert_eq!(d.action, None);
    }
    assert_eq!(f.resolved(), 0);
    assert_eq!(f.probe.contexts.load(Ordering::SeqCst), 0);
}

/// An unknown tool and a registered but ungranted tool give the agent identical results; only the audit record tells them apart.
#[test]
fn unknown_and_ungranted_tools_look_identical() {
    let f = fixture(Opts {
        actions: &["customer.read"],
        ..Opts::default()
    });
    let unknown = f.call("payroll.run", refund(1_000));
    let ungranted = f.call("refund.issue", refund(1_000));
    assert_eq!(unknown.result, ungranted.result);
    assert_eq!(unknown.code(), ungranted.code());
    f.assert_denied_as(&unknown, "action_not_granted", "unknown_action");
    f.assert_denied_as(&ungranted, "action_not_granted", "action_not_granted");
}

/// Invalid arguments to an ungranted tool are `action_not_granted`, so an agent cannot probe the schemas of operations it does not hold.
#[test]
fn ungranted_tool_arguments_are_not_validated() {
    let f = fixture(Opts {
        actions: &["customer.read"],
        ..Opts::default()
    });
    for args in [
        json!({"invoice_id": "acme-inv-001", "amount_cents": 100, "tenant_id": "globex"}),
        json!({"invoice_id": 7}),
        json!(null),
    ] {
        let d = f.assert_denied(&f.call("refund.issue", args), "action_not_granted");
        assert_eq!(d.args_digest, None);
    }
    assert_eq!(f.resolved(), 0);
}

/// Extra fields (a forged `tenant_id`), missing or ill-typed fields, non-objects, and oversized input are `invalid_arguments` before resolution.
#[test]
fn invalid_arguments_denied_before_resolution() {
    let f = fixture(Opts::default());
    let big = "a".repeat(MAX_ARGUMENT_BYTES);
    for args in [
        json!({"invoice_id": "acme-inv-001", "amount_cents": 100, "tenant_id": "globex"}),
        json!({"invoice_id": "acme-inv-001"}),
        json!({"invoice_id": 7, "amount_cents": 100}),
        json!({"invoice_id": "../globex-inv-001", "amount_cents": 100}),
        json!({"invoice_id": "acme-inv-001", "amount_cents": "100"}),
        json!({"invoice_id": "acme-inv-001", "amount_cents": 0}),
        json!([1, 2]),
        json!(null),
        json!({"invoice_id": "acme-inv-001", "amount_cents": 100, "x": big}),
    ] {
        let d = f.assert_denied(&f.call("refund.issue", args), "invalid_arguments");
        assert_eq!(d.enforcement, Some(Enforcement::GuardValidation));
        assert_eq!(d.args_digest, None);
    }
    assert_eq!(f.resolved(), 0);
    assert_eq!(f.probe.contexts.load(Ordering::SeqCst), 0);
}

/// An action the grant does not list is `action_not_granted` before the operation's context or resolver runs.
#[test]
fn action_not_in_grant_denied_before_resolution() {
    let f = fixture(Opts {
        actions: &["customer.read"],
        ..Opts::default()
    });
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "action_not_granted");
    assert_eq!(f.resolved(), 0);
    assert_eq!(f.probe.contexts.load(Ordering::SeqCst), 0);
}

/// A context that tries to set a Guard-reserved key is denied before resolution.
#[test]
fn reserved_context_key_denied_before_resolution() {
    let f = fixture(Opts {
        ctx: Ctx::Reserved,
        ..Opts::default()
    });
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "invalid_arguments");
    assert_eq!(f.resolved(), 0);
}

/// A missing id and another tenant's id give byte-identical agent-visible results.
#[test]
fn not_found_and_other_tenant_are_indistinguishable() {
    let f = fixture(Opts::default());
    let other = f.call(
        "refund.issue",
        json!({"invoice_id": "globex-inv-001", "amount_cents": 100}),
    );
    let missing = f.call(
        "refund.issue",
        json!({"invoice_id": "acme-inv-999", "amount_cents": 100}),
    );
    f.assert_denied(&other, "resource_not_found");
    f.assert_denied(&missing, "resource_not_found");
    assert_eq!(other.result, missing.result);
    let strip = |o: &CallOutcome| {
        let mut v = serde_json::to_value(o).unwrap();
        v.as_object_mut().unwrap().remove("decision_id");
        v.as_object_mut().unwrap().remove("audit_head");
        v
    };
    assert_eq!(strip(&other), strip(&missing));
}

/// An unavailable resolver, a resolver cut off by `TimeoutResolver`, and one cut off by the Guard-level timer are all `engine_unavailable`.
#[test]
fn resolver_unavailable_or_timeout_denied() {
    for o in [
        Opts {
            resolve: Resolve::Unavailable,
            ..Opts::default()
        },
        Opts {
            resolve: Resolve::Hang,
            timeout_resolver: true,
            ..Opts::default()
        },
        Opts {
            resolve: Resolve::Hang,
            guard_timer: true,
            ..Opts::default()
        },
    ] {
        let f = fixture(o);
        f.assert_denied(&f.call("refund.issue", refund(1_000)), "engine_unavailable");
    }
}

/// A panicking resolver or context builder is a deny, never an execution.
#[test]
fn panicking_resolver_or_context_denied() {
    let f = fixture(Opts {
        resolve: Resolve::Panic,
        ..Opts::default()
    });
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "engine_unavailable");
    let f = fixture(Opts {
        ctx: Ctx::Panic,
        ..Opts::default()
    });
    f.assert_denied(&f.call("refund.issue", refund(1_000)), "engine_unavailable");
    assert_eq!(f.resolved(), 0);
}

/// A refund on a customer outside the grant is `policy_deny`, and the full reason stays in the audit record.
#[test]
fn policy_deny_does_not_execute() {
    let f = fixture(Opts::default());
    let out = f.call(
        "refund.issue",
        json!({"invoice_id": "acme-inv-002", "amount_cents": 100}),
    );
    let d = f.assert_denied(&out, "policy_deny");
    assert_eq!(d.enforcement, Some(Enforcement::GuardPolicy));
    assert_eq!(d.resource, Some(uid("Invoice", "acme-inv-002")));
}

/// `customer.export` is denied by the `no-bulk-export` forbid.
#[test]
fn export_denied_by_policy() {
    let f = fixture(Opts::default());
    let d = f.assert_denied(
        &f.call("customer.export", json!({"customer_id": "acme-cus-001"})),
        "policy_deny",
    );
    assert_eq!(
        d.determining_policies,
        [PolicyId::new("no-bulk-export").unwrap()]
    );
}

/// `ApprovalRequired` does not execute; the agent gets the role and the digest of exactly this request.
#[test]
fn approval_required_does_not_execute() {
    let f = fixture(Opts::default());
    let args = refund(6_000);
    let out = f.call("refund.issue", args.clone());
    let CallResult::ApprovalRequired {
        role,
        action_digest,
    } = &out.result
    else {
        panic!("{out:?}");
    };
    assert_eq!(role, "supervisor");
    assert_eq!(out.code(), Some("approval_required"));
    let (d, o) = f.records_for(&out);
    assert!(o.is_none());
    assert_eq!(d.decision, Some(DecisionKind::ApprovalRequired));
    assert_eq!(d.enforcement, Some(Enforcement::GuardApproval));
    assert_eq!(d.outcome, Some(Outcome::Blocked));
    assert_eq!(action_digest.to_hex().len(), 64);
    assert_eq!(f.executed(), 0);
    assert_eq!(f.leased(), 0);
}

/// Without an approval store every presented approval, malformed or well-formed, is `approval_invalid`.
#[test]
fn presented_approval_without_store_denied() {
    let f = fixture(Opts::default());
    for id in ["ap-1", "not an id", "__guard_x"] {
        let meta = CallMeta {
            approval_id: Some(id.into()),
            ..CallMeta::default()
        };
        let d = f.assert_denied(
            &f.call_with("refund.issue", refund(6_000), meta),
            "approval_invalid",
        );
        assert_eq!(d.enforcement, Some(Enforcement::GuardApproval));
    }
}

/// A sink that cannot write the decision record (disk full) denies with a generic `engine_unavailable`, leases nothing, and executes nothing.
#[test]
fn audit_decision_failure_denies() {
    let f = fixture(Opts::default());
    f.sink.fail.store(true, Ordering::SeqCst);
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(
        out.result,
        CallResult::Denied {
            code: "engine_unavailable"
        }
    );
    assert_eq!(out.audit_head, None);
    assert_eq!(f.executed(), 0);
    assert_eq!(f.leased(), 0);
    assert!(f.records().is_empty());
}

/// With the real hash-chained sink over storage that fails like a full disk, the call is denied, nothing executes, and later calls stay denied.
#[test]
fn chained_sink_storage_failure_denies() {
    let sink = Arc::new(ChainedAuditSink::with_storage(FlakyStorage { ok: 0 }, None));
    let f = fixture(Opts {
        sink: Some(sink),
        ..Opts::default()
    });
    for _ in 0..2 {
        let out = f.call("refund.issue", refund(1_000));
        assert_eq!(out.code(), Some("engine_unavailable"));
    }
    assert_eq!(f.executed(), 0);
    assert_eq!(f.leased(), 0);
}

/// If the outcome record cannot be written after an effect, Guard fails closed and denies every later call without executing.
#[test]
fn outcome_write_failure_fails_closed() {
    let sink = Arc::new(ChainedAuditSink::with_storage(FlakyStorage { ok: 1 }, None));
    let f = fixture(Opts {
        sink: Some(sink),
        ..Opts::default()
    });
    let first = f.call("refund.issue", refund(1_000));
    assert!(!first.is_error());
    assert_eq!(f.executed(), 1);
    assert!(f.guard.is_failed_closed());
    let next = f.call("refund.issue", refund(1_000));
    assert_eq!(next.code(), Some("engine_unavailable"));
    assert_eq!(f.executed(), 1);
    assert_eq!(f.leased(), 1);
}

/// Revoking the grant after authorization but before execution blocks the call: the recheck denies and nothing executes.
#[test]
fn revoke_between_authorize_and_execute_has_no_effect() {
    let f = fixture(Opts::default());
    f.revoke_on_allow();
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(
        out.result,
        CallResult::Denied {
            code: "grant_revoked"
        }
    );
    assert_eq!(f.executed(), 0);
    let (d, o) = f.records_for(&out);
    assert_eq!(d.decision, Some(DecisionKind::Allow));
    let o = o.expect("outcome record");
    assert_eq!(o.outcome, Some(Outcome::Blocked));
    assert_eq!(o.reason, Some(DenyReason::GrantRevoked));
    assert_eq!(o.enforcement, Some(Enforcement::GuardSession));
}

/// Expiry between authorization and execution is caught by the recheck on a fresh clock reading.
#[test]
fn expiry_between_authorize_and_execute_has_no_effect() {
    let f = fixture(Opts::default());
    f.clock.set_secs_after_start(599);
    let clock = f.clock.clone();
    *f.sink.hook.lock().unwrap() = Some(Arc::new(move |r| {
        if r.decision == Some(DecisionKind::Allow) {
            clock.set_secs_after_start(600);
        }
    }));
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some("grant_expired"));
    assert_eq!(f.executed(), 0);
}

/// A panicking executor is a failed outcome, never an executed one.
#[test]
fn panicking_executor_is_failed() {
    let f = fixture(Opts {
        exec: Exec::Panic,
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(
        out.result,
        CallResult::Failed {
            code: EXECUTION_FAILED
        }
    );
    let (_, o) = f.records_for(&out);
    assert_eq!(o.unwrap().outcome, Some(Outcome::Failed));
}

/// A downstream precondition failure is `precondition_failed` with `executor_precondition` enforcement.
#[test]
fn precondition_failure_reported() {
    let f = fixture(Opts {
        exec: Exec::Precondition,
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some(PRECONDITION_FAILED));
    let (_, o) = f.records_for(&out);
    let o = o.unwrap();
    assert_eq!(o.outcome, Some(Outcome::Failed));
    assert_eq!(o.enforcement, Some(Enforcement::ExecutorPrecondition));
}

/// Executor error detail reaches neither the agent-visible outcome nor the audit records.
#[test]
fn executor_error_detail_not_exposed() {
    let detail = format!("upstream said {SECRET}");
    let f = fixture(Opts {
        exec: Exec::Failed(detail),
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some(EXECUTION_FAILED));
    let visible = format!("{out:?} {}", serde_json::to_string(&out).unwrap());
    assert!(!visible.contains(SECRET));
    let audit = serde_json::to_string(&f.records()).unwrap();
    assert!(!audit.contains(SECRET) && !audit.contains("upstream said"));
}

/// A broker failure after the decision record blocks execution with `engine_unavailable`.
#[test]
fn broker_failure_blocks_execution() {
    let f = fixture(Opts {
        broker_fails: true,
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some("engine_unavailable"));
    assert_eq!(f.executed(), 0);
    let (_, o) = f.records_for(&out);
    assert_eq!(o.unwrap().outcome, Some(Outcome::Blocked));
}

/// Audit keeps only allowlisted argument fields, a digest of the rest, and bounded untrusted correlation kept apart from trusted fields.
#[test]
fn audit_fields_are_allowlisted_and_bounded() {
    let f = fixture(Opts::default());
    let meta = CallMeta {
        approval_id: None,
        correlation: UntrustedCorrelation {
            tool_use_id: Some("t".repeat(10_000)),
            client_request_id: Some("req-1".into()),
            requested_tool: Some("forged".into()),
            client_info: Some(json!({"name": "x".repeat(10_000)})),
        },
    };
    let out = f.call_with(
        "customer.read",
        json!({"customer_id": "acme-cus-001"}),
        meta,
    );
    assert!(!out.is_error());
    let (d, _) = f.records_for(&out);
    assert_eq!(
        Value::Object(d.args_logged.clone()),
        json!({"customer_id": "acme-cus-001"})
    );
    assert_eq!(d.args_digest.as_deref().map(str::len), Some(64));
    assert_eq!(d.untrusted.tool_use_id.unwrap().len(), MAX_CORRELATION_LEN);
    assert_eq!(d.untrusted.client_request_id.as_deref(), Some("req-1"));
    assert_eq!(d.untrusted.requested_tool, None);
    assert_eq!(d.untrusted.client_info, None);
}

/// No audit record ever contains the session handle or the broker secret.
#[test]
fn audit_never_contains_handle_or_secret() {
    let f = fixture(Opts::default());
    f.call("refund.issue", refund(1_000));
    f.call("refund.issue", refund(6_000));
    f.call("nope", json!({}));
    let audit = serde_json::to_string(&f.records()).unwrap();
    assert!(!audit.contains(&f.handle));
    assert!(!audit.contains(SECRET));
}

/// `Guard::call` returns a `Send` future, so a multi-threaded server can spawn it.
#[test]
fn call_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let f = fixture(Opts::default());
    let fut = f.guard.call(&f.handle, "x", json!({}), CallMeta::default());
    assert_send(&fut);
}

/// Dropping the call future while `execute` is pending (no outcome record) fails Guard closed: later calls are denied without executing.
#[test]
fn cancel_mid_execute_fails_closed() {
    let f = fixture(Opts::default());
    let fut = f.guard.call(
        &f.handle,
        "refund.issue",
        refund(HANG_AMOUNT),
        CallMeta::default(),
    );
    assert!(fut.now_or_never().is_none(), "executor should be pending");
    assert_eq!(f.executed(), 1);
    assert!(f.guard.is_failed_closed());
    let recs = f.records();
    assert_eq!(recs.len(), 1, "decision record only: {recs:?}");
    let next = f.call("refund.issue", refund(1_000));
    assert_eq!(next.code(), Some("engine_unavailable"));
    assert_eq!(f.executed(), 1);
}

/// Dropping a call future before its decision record leaves Guard serving.
#[test]
fn cancel_before_decision_does_not_latch() {
    let f = fixture(Opts {
        resolve: Resolve::Hang,
        ..Opts::default()
    });
    let fut = f.guard.call(
        &f.handle,
        "refund.issue",
        refund(1_000),
        CallMeta::default(),
    );
    assert!(fut.now_or_never().is_none());
    assert!(!f.guard.is_failed_closed());
    assert!(f.records().is_empty());
}

/// An executor future whose `Drop` panics is a failed outcome with an outcome record, not an escaped panic.
#[test]
fn panicking_drop_of_executor_future_is_failed() {
    let f = fixture(Opts {
        exec: Exec::DropPanic,
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some(EXECUTION_FAILED));
    let (_, o) = f.records_for(&out);
    assert_eq!(o.unwrap().outcome, Some(Outcome::Failed));
    assert!(!f.guard.is_failed_closed());
}

/// A latch set by another call after this call's decision record stops this call before lease and execute.
#[test]
fn latch_rechecked_before_lease_and_execute() {
    let f = fixture(Opts::default());
    let guard = f.guard.clone();
    let handle = f.handle.clone();
    let fired = AtomicBool::new(false);
    *f.sink.hook.lock().unwrap() = Some(Arc::new(move |r| {
        if r.record == RecordKind::Decision
            && r.decision == Some(DecisionKind::Allow)
            && !fired.swap(true, Ordering::SeqCst)
        {
            // A concurrent call is cancelled mid-execute, latching Guard.
            let other = guard.call(
                &handle,
                "refund.issue",
                refund(HANG_AMOUNT),
                CallMeta::default(),
            );
            assert!(other.now_or_never().is_none());
        }
    }));
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some("engine_unavailable"));
    assert!(f.guard.is_failed_closed());
    assert_eq!(f.executed(), 1, "only the cancelled call's executor ran");
    assert_eq!(f.leased(), 1);
    assert!(!f
        .probe
        .exec_decision_ids
        .lock()
        .unwrap()
        .contains(&out.decision_id.as_str().to_owned()));
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Every trace event of this test binary, at `TRACE`. A global subscriber,
/// installed before any call, because a thread-local one races with
/// callsite-interest caching in parallel tests.
fn traces() -> &'static Capture {
    static CAP: std::sync::OnceLock<Capture> = std::sync::OnceLock::new();
    CAP.get_or_init(|| {
        let cap = Capture::default();
        let w = cap.clone();
        tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(move || w.clone())
                .finish(),
        )
        .expect("no other global subscriber in this test binary");
        cap
    })
}

/// Executor error text is never emitted to `tracing`, only its length and a digest prefix.
#[test]
fn executor_error_detail_not_traced() {
    let detail = format!("upstream said {SECRET}");
    let f = fixture(Opts {
        exec: Exec::Failed(detail),
        ..Opts::default()
    });
    let out = f.call("refund.issue", refund(1_000));
    assert_eq!(out.code(), Some(EXECUTION_FAILED));
    let all = String::from_utf8(traces().0.lock().unwrap().clone()).unwrap();
    let mine: Vec<&str> = all
        .lines()
        .filter(|l| l.contains(out.decision_id.as_str()))
        .collect();
    assert!(
        mine.iter()
            .any(|l| l.contains("protected operation failed") && l.contains("detail_len")),
        "{mine:?}"
    );
    assert!(!all.contains(SECRET) && !all.contains("upstream said"));
}

/// Prints in-process end-to-end latency of an allowed and a denied call (metric E5); run with `--release --ignored --nocapture`.
#[test]
#[ignore = "measurement"]
fn pipeline_latency() {
    use std::time::Instant;
    let f = fixture(Opts::default());
    let n = 5_000;
    for (name, args) in [
        ("allowed refund", refund(1)),
        (
            "policy deny",
            json!({"invoice_id": "acme-inv-002", "amount_cents": 1}),
        ),
    ] {
        f.sink.records.lock().unwrap().clear();
        let mut t: Vec<u128> = (0..n)
            .map(|_| {
                let s = Instant::now();
                std::hint::black_box(f.call("refund.issue", args.clone()));
                s.elapsed().as_nanos()
            })
            .collect();
        t.sort_unstable();
        let us = |q: usize| t[n * q / 100] as f64 / 1e3;
        println!(
            "{name}: p50={:.1}us p95={:.1}us p99={:.1}us (n={n}, in-memory sink)",
            us(50),
            us(95),
            us(99)
        );
    }
}

// --------------------------------------------------------------- registry

fn op_with(action: &str, schema: Value, logged: &'static [&'static str]) -> Arc<TestOp> {
    let probe = Arc::new(Probe::default());
    Arc::new(TestOp {
        action: ActionId::new(action).unwrap(),
        schema,
        logged,
        resolver: Box::new(TestResolver {
            id_field: "customer_id",
            mode: Resolve::Data,
            probe: probe.clone(),
        }),
        ctx: Ctx::Ok,
        exec: Exec::Ok,
        probe,
    })
}

/// Duplicate actions and the Guard-reserved `guard.` prefix are rejected at registration.
#[test]
fn registry_rejects_duplicates_and_reserved_names() {
    let mut reg = OperationRegistry::new();
    reg.register(op_with("customer.read", customer_schema(), &[]))
        .unwrap();
    assert!(matches!(
        reg.register(op_with("customer.read", customer_schema(), &[])),
        Err(RegistryError::Duplicate(_))
    ));
    assert!(matches!(
        reg.register(op_with("guard.delegate", customer_schema(), &[])),
        Err(RegistryError::Reserved(_))
    ));
    assert_eq!(reg.len(), 1);
    assert_eq!(
        reg.action_ids(),
        [ActionId::new("customer.read").unwrap()].into()
    );
    let tools: Vec<_> = reg.tools().map(|t| t.name.as_str().to_owned()).collect();
    assert_eq!(tools, ["customer.read"]);
}

/// Open, non-object, `$ref`-using, and uncompilable schemas, and logged fields outside the schema, are rejected at registration.
#[test]
fn registry_rejects_unsafe_schemas() {
    let bad = [
        json!({"type": "object", "properties": {"customer_id": {"type": "string"}}}),
        json!({"type": "object", "additionalProperties": true}),
        json!({"type": "string"}),
        json!({"type": "object", "additionalProperties": false,
               "properties": {"n": {"type": "object", "properties": {"x": {"type": "string"}}}}}),
        json!({"type": "object", "additionalProperties": false,
               "properties": {"n": {"$ref": "https://example.com/s.json"}}}),
        json!({"type": "object", "additionalProperties": false,
               "patternProperties": {"^x": {"type": "string"}}}),
        json!({"type": "object", "additionalProperties": false,
               "properties": {"n": {"type": "string", "pattern": "("}}}),
    ];
    for schema in bad {
        let mut reg = OperationRegistry::new();
        assert!(
            matches!(
                reg.register(op_with("customer.read", schema.clone(), &[])),
                Err(RegistryError::InvalidSchema { .. })
            ),
            "{schema}"
        );
    }
    let mut reg = OperationRegistry::new();
    assert!(matches!(
        reg.register(op_with("customer.read", customer_schema(), &["note"])),
        Err(RegistryError::UnknownLoggedField { .. })
    ));
}

fn closed(props: Value) -> Value {
    json!({"type": "object", "properties": props, "additionalProperties": false})
}

/// Schemas that would accept unvalidated structure are rejected: empty and boolean subschemas, type unions, untyped or open arrays, unwalked applicators, unknown keywords, and unknown formats.
#[test]
fn registry_rejects_schema_bypasses() {
    let s = json!({"type": "string"});
    let bad = [
        ("empty subschema", closed(json!({"x": {}}))),
        ("boolean subschema", closed(json!({"x": true}))),
        (
            "type union at root",
            json!({"type": ["object", "null"], "additionalProperties": false}),
        ),
        (
            "type union",
            closed(json!({"x": {"type": ["string", "object"]}})),
        ),
        ("untyped", closed(json!({"x": {"minLength": 1}}))),
        (
            "array without items",
            closed(json!({"x": {"type": "array"}})),
        ),
        (
            "items true",
            closed(json!({"x": {"type": "array", "items": true}})),
        ),
        (
            "items empty",
            closed(json!({"x": {"type": "array", "items": {}}})),
        ),
        (
            "items open object",
            closed(json!({"x": {"type": "array", "items": {"type": "object"}}})),
        ),
        (
            "additionalProperties schema",
            json!({"type": "object", "additionalProperties": {"type": "string"}}),
        ),
        (
            "contains",
            closed(json!({"x": {"type": "array", "items": s, "contains": {}}})),
        ),
        (
            "prefixItems",
            closed(json!({"x": {"type": "array", "items": s, "prefixItems": [{}]}})),
        ),
        (
            "unevaluatedItems",
            closed(json!({"x": {"type": "array", "items": s, "unevaluatedItems": {}}})),
        ),
        (
            "dependentSchemas",
            json!({"type": "object", "additionalProperties": false, "dependentSchemas": {"a": {}}}),
        ),
        (
            "propertyNames",
            json!({"type": "object", "additionalProperties": false, "propertyNames": {}}),
        ),
        (
            "oneOf",
            json!({"type": "object", "additionalProperties": false, "oneOf": [{}]}),
        ),
        (
            "anyOf",
            json!({"type": "object", "additionalProperties": false, "anyOf": [{}]}),
        ),
        (
            "allOf",
            json!({"type": "object", "additionalProperties": false, "allOf": [{}]}),
        ),
        (
            "not",
            json!({"type": "object", "additionalProperties": false, "not": {}}),
        ),
        (
            "if",
            json!({"type": "object", "additionalProperties": false, "if": {}, "then": {}}),
        ),
        (
            "$defs",
            json!({"type": "object", "additionalProperties": false, "$defs": {"a": {}}}),
        ),
        (
            "unknown keyword",
            closed(json!({"x": {"type": "string", "x-ui": 1}})),
        ),
        (
            "unknown format",
            closed(json!({"x": {"type": "string", "format": "no-such-format"}})),
        ),
        (
            "string keyword on integer",
            closed(json!({"x": {"type": "integer", "pattern": "^1$"}})),
        ),
        (
            "old draft",
            json!({"$schema": "http://json-schema.org/draft-07/schema#",
                   "type": "object", "additionalProperties": false}),
        ),
    ];
    for (name, schema) in bad {
        let mut reg = OperationRegistry::new();
        assert!(
            matches!(
                reg.register(op_with("customer.read", schema.clone(), &[])),
                Err(RegistryError::InvalidSchema { .. })
            ),
            "{name}: {schema}"
        );
    }
}

/// Typed scalars, arrays with typed items, nested closed objects, enums, and a 2020-12 `$schema` are accepted.
#[test]
fn registry_accepts_closed_typed_schemas() {
    let good = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "customer_id": id_schema(),
            "tags": {"type": "array", "items": {"type": "string", "enum": ["a", "b"]}, "maxItems": 4},
            "nested": closed(json!({"n": {"type": "number", "minimum": 0}})),
            "flag": {"type": "boolean", "description": "d"},
            "email": {"type": "string", "format": "email"}
        },
        "required": ["customer_id"],
        "additionalProperties": false
    });
    let mut reg = OperationRegistry::new();
    reg.register(op_with("customer.read", good, &["customer_id"]))
        .unwrap();
    for schema in [refund_schema(), customer_schema()] {
        let mut reg = OperationRegistry::new();
        reg.register(op_with("customer.read", schema, &[])).unwrap();
    }
}

// --------------------------------------------------------------- proptest

mod props {
    use super::*;
    use proptest::prelude::*;

    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<f64>()
                .prop_map(|f| serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)),
            "[a-z0-9._:@/ -]{0,12}".prop_map(Value::from),
            Just(Value::from("acme-inv-001")),
        ];
        leaf.prop_recursive(4, 32, 6, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map(
                    prop_oneof![
                        Just("invoice_id".to_owned()),
                        Just("amount_cents".to_owned()),
                        Just("tenant_id".to_owned()),
                        "[a-z_]{1,8}"
                    ],
                    inner,
                    0..4
                )
                .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Arbitrary arguments and tool names never panic, and the executor runs only for the exact tool name with schema-valid arguments.
        #[test]
        fn malformed_input_never_executes(args in arb_json(), tool in prop_oneof![
            Just("refund.issue".to_owned()), "[a-zA-Z._ ]{0,16}"
        ]) {
            let f = fixture(Opts::default());
            let out = f.call(&tool, args.clone());
            let valid = jsonschema::draft202012::new(&refund_schema()).unwrap().is_valid(&args);
            if f.executed() > 0 {
                prop_assert_eq!(tool.as_str(), "refund.issue");
                prop_assert!(valid);
            } else {
                prop_assert!(out.is_error());
            }
            if tool != "refund.issue" {
                prop_assert_eq!(f.resolved(), 0);
            }
            if tool == "refund.issue" && !valid {
                prop_assert_eq!(out.code(), Some("invalid_arguments"));
                prop_assert_eq!(f.resolved(), 0);
            }
        }
    }
}
