//! Approval tests (#354, scenarios G8/G9): a stored approval authorizes at
//! most one execution of exactly the digested action.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use crom_guard::*;
use futures::executor::block_on;
use serde_json::{json, Value};

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");
const START_MS: u64 = 1_000_000;
/// Above the delegator's 5 000 limit, within the grant's 10 000: needs a supervisor.
const BIG: i64 = 7_000;

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
                head_hash: format!("{:064x}", r.len()),
            })
        })
    }
}

struct Broker;

impl CredentialBroker for Broker {
    fn lease<'a>(&'a self, _: &'a ActionId) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async { Ok(CredentialLease::new("test-cred", "sk-approvals-test")) })
    }
}

fn uid(ty: &str, id: &str) -> EntityUid {
    EntityUid::new(format!("Support::{ty}"), id).unwrap()
}

/// Invoices in tenant `acme`; `version` is the resource version the
/// resolver reports, so a test can change it between approval and retry.
/// Each resolution advances `clock` by `advance_ms`.
struct Resolver {
    version: Arc<AtomicU64>,
    clock: Arc<TestClock>,
    advance_ms: Arc<AtomicU64>,
}

impl ResourceResolver for Resolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        Box::pin(async move {
            let id = args.as_json()["invoice_id"].as_str().unwrap_or_default();
            let customer = match id {
                "acme-inv-001" => "acme-cus-001",
                "acme-inv-002" => "acme-cus-002",
                _ => return Err(ResolveError::NotFound),
            };
            if tenant.as_str() != "acme" {
                return Err(ResolveError::NotFound);
            }
            self.clock
                .0
                .fetch_add(self.advance_ms.load(Ordering::SeqCst), Ordering::SeqCst);
            let v = self.version.load(Ordering::SeqCst).to_string();
            Ok(
                ResolvedResource::new(uid("Invoice", id), ResourceVersion::new(v))
                    .with_attr("tenant", AttrValue::Entity(uid("Tenant", "acme")))
                    .with_attr("customer", AttrValue::Entity(uid("Customer", customer)))
                    .with_attr("amount_cents", AttrValue::Long(20_000))
                    .with_attr("refunded_cents", AttrValue::Long(0))
                    .with_parent(uid("Customer", customer)),
            )
        })
    }
}

struct Refund {
    action: ActionId,
    schema: Value,
    resolver: Resolver,
    executed: Arc<AtomicUsize>,
}

impl ProtectedOperation for Refund {
    fn action(&self) -> &ActionId {
        &self.action
    }
    fn description(&self) -> &str {
        "refund"
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn logged_fields(&self) -> &[&'static str] {
        &["invoice_id", "amount_cents"]
    }
    fn resolver(&self) -> &dyn ResourceResolver {
        &self.resolver
    }
    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        let a = args.as_json()["amount_cents"]
            .as_i64()
            .ok_or(DenyReason::InvalidArguments)?;
        AuthorizationContext::new([("amount_cents".to_owned(), AttrValue::Long(a))].into())
            .map_err(|_| DenyReason::InvalidArguments)
    }
    fn execute<'a>(
        &'a self,
        _: AuthorizedAction,
        _: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        Box::pin(async move {
            self.executed.fetch_add(1, Ordering::SeqCst);
            Ok(ExecutionOutput {
                content: json!({"ok": true}),
                downstream_ref: None,
            })
        })
    }
}

fn app() -> AppId {
    AppId::new("support-app").unwrap()
}

fn grant_spec() -> GrantSpec {
    GrantSpec {
        tenant: TenantId::new("acme").unwrap(),
        delegator: Delegator::new(
            UserId::new("acme-agent-operator").unwrap(),
            "agent-operator",
            [("refund_limit_cents".to_owned(), AttrValue::Long(5_000))].into(),
        )
        .unwrap(),
        run_id: RunId::new("run-1").unwrap(),
        actions: [ActionId::new("refund.issue").unwrap()].into(),
        constraints: Constraints::new(BTreeMap::from([
            (
                "customers".to_owned(),
                Constraint::Set(
                    [
                        uid("Customer", "acme-cus-001"),
                        uid("Customer", "acme-cus-002"),
                    ]
                    .into(),
                ),
            ),
            ("max_refund_cents".to_owned(), Constraint::AtMost(10_000)),
        ]))
        .unwrap(),
        ttl_seconds: 3_600,
        max_depth: 0,
    }
}

struct Fixture {
    guard: Arc<Guard>,
    grants: Arc<MemoryGrantStore>,
    approvals: Arc<MemoryApprovalStore>,
    sink: Arc<MemorySink>,
    clock: Arc<TestClock>,
    version: Arc<AtomicU64>,
    advance_ms: Arc<AtomicU64>,
    executed: Arc<AtomicUsize>,
    handle: String,
}

fn fixture() -> Fixture {
    fixture_with(ApprovalStoreConfig::default())
}

fn fixture_with(config: ApprovalStoreConfig) -> Fixture {
    traces();
    let version = Arc::new(AtomicU64::new(1));
    let advance_ms = Arc::new(AtomicU64::new(0));
    let clock = Arc::new(TestClock(AtomicU64::new(START_MS)));
    let executed = Arc::new(AtomicUsize::new(0));
    let mut reg = OperationRegistry::new();
    reg.register(Arc::new(Refund {
        action: ActionId::new("refund.issue").unwrap(),
        schema: json!({
            "type": "object",
            "properties": {
                "invoice_id": {"type": "string", "pattern": "^[A-Za-z0-9._:@-]{1,128}$"},
                "amount_cents": {"type": "integer", "minimum": 1}
            },
            "required": ["invoice_id", "amount_cents"],
            "additionalProperties": false
        }),
        resolver: Resolver {
            version: version.clone(),
            clock: clock.clone(),
            advance_ms: advance_ms.clone(),
        },
        executed: executed.clone(),
    }))
    .unwrap();
    let grants = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(
        reg.action_ids(),
    )));
    let issued = block_on(grants.issue(&app(), grant_spec(), clock.now())).unwrap();
    let approvals = Arc::new(MemoryApprovalStore::new(config));
    let sink = Arc::new(MemorySink::default());
    let guard = Guard::new(
        reg,
        grants.clone(),
        Arc::new(CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap()),
        Arc::new(Broker),
        sink.clone(),
        clock.clone(),
    )
    .with_approval_verifier(approvals.clone());
    Fixture {
        guard: Arc::new(guard),
        grants,
        approvals,
        sink,
        clock,
        version,
        advance_ms,
        executed,
        handle: issued.handle.expose_secret().to_owned(),
    }
}

fn refund(invoice: &str, amount: i64) -> Value {
    json!({"invoice_id": invoice, "amount_cents": amount})
}

fn user(id: &str, role: &str) -> Delegator {
    let attrs = [("refund_limit_cents".to_owned(), AttrValue::Long(100_000))].into();
    Delegator::new(UserId::new(id).unwrap(), role, attrs).unwrap()
}

fn supervisor() -> Delegator {
    user("acme-supervisor", "supervisor")
}

fn with_id(id: &ApprovalId) -> CallMeta {
    CallMeta {
        approval_id: Some(id.as_str().to_owned()),
        ..CallMeta::default()
    }
}

impl Fixture {
    fn call(&self, args: Value, meta: CallMeta) -> CallOutcome {
        self.call_as(&self.handle, args, meta)
    }

    fn call_as(&self, handle: &str, args: Value, meta: CallMeta) -> CallOutcome {
        block_on(self.guard.call(handle, "refund.issue", args, meta))
    }

    /// The digest Guard returns for `args` without an approval.
    fn required(&self, args: Value) -> ActionDigest {
        self.required_as(&self.handle, args)
    }

    fn required_as(&self, handle: &str, args: Value) -> ActionDigest {
        match self.call_as(handle, args, CallMeta::default()).result {
            CallResult::ApprovalRequired {
                role,
                action_digest,
            } => {
                assert_eq!(role, "supervisor");
                action_digest
            }
            other => panic!("expected approval_required, got {other:?}"),
        }
    }

    fn spec(&self, digest: ActionDigest) -> ApprovalSpec {
        ApprovalSpec {
            action_digest: digest,
            approver: supervisor(),
            approver_tenant: TenantId::new("acme").unwrap(),
            ttl_seconds: 300,
        }
    }

    fn approve(&self, spec: ApprovalSpec) -> ApprovalId {
        self.approvals
            .approve(&app(), spec, self.clock.now())
            .unwrap()
            .approval_id
    }

    /// Approves exactly the big refund on invoice 001.
    fn approve_big(&self) -> ApprovalId {
        let d = self.required(refund("acme-inv-001", BIG));
        self.approve(self.spec(d))
    }

    fn executed(&self) -> usize {
        self.executed.load(Ordering::SeqCst)
    }

    fn decision_record(&self, out: &CallOutcome) -> AuditRecord {
        self.sink
            .0
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.decision_id == out.decision_id && r.record == RecordKind::Decision)
            .cloned()
            .expect("decision record")
    }

    fn assert_invalid(&self, out: &CallOutcome) {
        assert_eq!(out.code(), Some("approval_invalid"), "{out:?}");
        let d = self.decision_record(out);
        assert_eq!(d.reason, Some(DenyReason::ApprovalInvalid));
        assert_eq!(d.enforcement, Some(Enforcement::GuardApproval));
        assert_eq!(d.approval_id, None);
    }
}

/// A stored approval for exactly the digested request executes it once, and the audit decision record carries the approval id.
#[test]
fn valid_approval_allows_once() {
    let f = fixture();
    let id = f.approve_big();
    assert!(id.as_str().starts_with("ap_"));
    let out = f.call(refund("acme-inv-001", BIG), with_id(&id));
    assert!(matches!(out.result, CallResult::Executed { .. }), "{out:?}");
    assert_eq!(f.executed(), 1);
    let d = f.decision_record(&out);
    assert_eq!(d.decision, Some(DecisionKind::Allow));
    assert_eq!(d.approval_id.as_ref(), Some(&id));
    assert_eq!(
        f.approvals.status(&app(), &id, f.clock.now()),
        Some(ApprovalStatus::Used)
    );
}

/// Presenting a used approval again is `approval_invalid` and does not execute (G9 replay).
#[test]
fn replayed_approval_denied() {
    let f = fixture();
    let id = f.approve_big();
    assert!(!f.call(refund("acme-inv-001", BIG), with_id(&id)).is_error());
    let again = f.call(refund("acme-inv-001", BIG), with_id(&id));
    f.assert_invalid(&again);
    assert_eq!(f.executed(), 1);
}

/// A well-formed id that no approval has is `approval_invalid` (G9 random id).
#[test]
fn random_id_denied() {
    let f = fixture();
    let _ = f.approve_big();
    let forged = ApprovalId::new("ap_00000000000000000000000000000000").unwrap();
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&forged)));
    assert_eq!(f.executed(), 0);
}

/// An approval for one amount does not authorize a different amount (different args ⇒ different digest).
#[test]
fn different_amount_denied() {
    let f = fixture();
    let id = f.approve_big();
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG + 1), with_id(&id)));
    assert_eq!(f.executed(), 0);
    // The mismatch did not consume the approval.
    assert_eq!(
        f.approvals.status(&app(), &id, f.clock.now()),
        Some(ApprovalStatus::Pending)
    );
}

/// An approval for one invoice does not authorize the same amount on another invoice.
#[test]
fn different_resource_denied() {
    let f = fixture();
    let id = f.approve_big();
    f.assert_invalid(&f.call(refund("acme-inv-002", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
}

/// An approval pins the resource version the approver saw; a changed version is `approval_invalid`.
#[test]
fn different_resource_version_denied() {
    let f = fixture();
    let id = f.approve_big();
    f.version.store(2, Ordering::SeqCst);
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
}

/// An approval bound to one grant cannot be used through another grant with identical scope.
#[test]
fn different_grant_denied() {
    let f = fixture();
    let id = f.approve_big();
    let other = block_on(f.grants.issue(&app(), grant_spec(), f.clock.now())).unwrap();
    let other = other.handle.expose_secret().to_owned();
    f.assert_invalid(&f.call_as(&other, refund("acme-inv-001", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
}

/// An approval is unusable from its expiry instant on.
#[test]
fn expired_approval_denied() {
    let f = fixture();
    let id = f.approve_big();
    f.clock.0.store(START_MS + 300_000, Ordering::SeqCst);
    assert_eq!(
        f.approvals.status(&app(), &id, f.clock.now()),
        Some(ApprovalStatus::Expired)
    );
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
}

/// An approval is still usable one millisecond before its expiry.
#[test]
fn approval_valid_until_expiry() {
    let f = fixture();
    let id = f.approve_big();
    f.clock.0.store(START_MS + 299_999, Ordering::SeqCst);
    assert!(!f.call(refund("acme-inv-001", BIG), with_id(&id)).is_error());
}

/// An approver from another tenant is rejected before Cedar sees the approval, even with the right role and digest.
#[test]
fn cross_tenant_approver_denied() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    let id = f.approve(ApprovalSpec {
        approver_tenant: TenantId::new("globex").unwrap(),
        ..f.spec(d)
    });
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
    assert_eq!(
        f.approvals.status(&app(), &id, f.clock.now()),
        Some(ApprovalStatus::Pending)
    );
}

/// Another application can neither view nor approve a pending request that arose under a grant it did not issue.
#[test]
fn other_issuer_cannot_see_or_approve() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    let other = AppId::new("other-app").unwrap();
    assert_eq!(f.approvals.pending(&other, &d, f.clock.now()), None);
    assert_eq!(
        f.approvals.approve(&other, f.spec(d), f.clock.now()),
        Err(ApprovalError::UnknownRequest)
    );
    // The application that issued the grant still can.
    let id = f.approve(f.spec(d));
    assert!(!f.call(refund("acme-inv-001", BIG), with_id(&id)).is_error());
}

/// Cedar sees the stored approver: an approval by a non-supervisor matches the digest but Cedar denies, and the approval is consumed anyway.
#[test]
fn stored_approver_role_reaches_cedar_and_deny_burns_approval() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    let clerk = user("acme-clerk", "clerk");
    let id = f.approve(ApprovalSpec {
        approver: clerk,
        ..f.spec(d)
    });
    let out = f.call(refund("acme-inv-001", BIG), with_id(&id));
    assert_eq!(out.code(), Some("policy_deny"), "{out:?}");
    assert_eq!(f.decision_record(&out).approval_id.as_ref(), Some(&id));
    assert_eq!(f.executed(), 0);
    assert_eq!(
        f.approvals.status(&app(), &id, f.clock.now()),
        Some(ApprovalStatus::Used)
    );
}

/// The pending request records exactly what Guard evaluated: action, resource, version, validated args, operation fields, and role.
#[test]
fn pending_request_shows_what_is_approved() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    let p = f.approvals.pending(&app(), &d, f.clock.now()).unwrap();
    assert_eq!(p.action_digest, d);
    assert_eq!(p.action.as_str(), "refund.issue");
    assert_eq!(p.resource, uid("Invoice", "acme-inv-001"));
    assert_eq!(p.resource_version, ResourceVersion::new("1"));
    assert_eq!(p.args, refund("acme-inv-001", BIG));
    assert_eq!(
        p.fields,
        [("amount_cents".to_owned(), AttrValue::Long(BIG))].into()
    );
    assert_eq!(p.role, "supervisor");
    assert_eq!(p.tenant.as_str(), "acme");
    assert_eq!(
        p.expires_at,
        Timestamp::from_unix_millis(START_MS + 900_000)
    );
}

/// `approve` fails for a digest with no pending request, including one already approved, and for an expired pending request.
#[test]
fn approve_requires_pending_request() {
    let f = fixture();
    let unknown = ActionDigest::from_bytes([9; 32]);
    assert_eq!(
        f.approvals.approve(&app(), f.spec(unknown), f.clock.now()),
        Err(ApprovalError::UnknownRequest)
    );
    let d = f.required(refund("acme-inv-001", BIG));
    f.approve(f.spec(d));
    assert_eq!(f.approvals.pending(&app(), &d, f.clock.now()), None);
    assert_eq!(
        f.approvals.approve(&app(), f.spec(d), f.clock.now()),
        Err(ApprovalError::UnknownRequest)
    );
    let d2 = f.required(refund("acme-inv-002", BIG));
    f.clock.0.store(START_MS + 900_000, Ordering::SeqCst);
    assert_eq!(
        f.approvals.approve(&app(), f.spec(d2), f.clock.now()),
        Err(ApprovalError::UnknownRequest)
    );
    assert_eq!(f.approvals.len(), 1);
}

/// A repeated `ApprovalRequired` for the same digest keeps the first pending record, so the version a human reviewed cannot be swapped underneath them.
#[test]
fn pending_request_not_replaced_by_newer_version() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    f.version.store(2, Ordering::SeqCst);
    assert_eq!(f.required(refund("acme-inv-001", BIG)), d);
    let p = f.approvals.pending(&app(), &d, f.clock.now()).unwrap();
    assert_eq!(p.resource_version, ResourceVersion::new("1"));
    // The approval binds version 1, so a retry against version 2 is refused.
    let id = f.approve(f.spec(d));
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
}

/// The approval binds the version recorded with the pending request, whatever version the admin might assume.
#[test]
fn approval_binds_pending_version() {
    let f = fixture();
    f.version.store(2, Ordering::SeqCst);
    let id = f.approve_big();
    f.version.store(1, Ordering::SeqCst);
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
    f.version.store(2, Ordering::SeqCst);
    assert!(!f.call(refund("acme-inv-001", BIG), with_id(&id)).is_error());
}

/// The approved fields Cedar sees are the request's own operation fields, so any approved amount works without the admin restating it.
#[test]
fn approval_fields_come_from_request() {
    let f = fixture();
    let d = f.required(refund("acme-inv-002", 6_000));
    let id = f.approve(f.spec(d));
    assert!(!f
        .call(refund("acme-inv-002", 6_000), with_id(&id))
        .is_error());
    assert_eq!(f.executed(), 1);
}

/// Expiry is judged on Guard's clock at the moment of `verify`, not at call start: time spent resolving counts.
#[test]
fn expiry_checked_after_resolution() {
    let f = fixture();
    let id = f.approve_big();
    f.clock.0.store(START_MS + 299_000, Ordering::SeqCst);
    f.advance_ms.store(1_000, Ordering::SeqCst);
    f.assert_invalid(&f.call(refund("acme-inv-001", BIG), with_id(&id)));
    assert_eq!(f.executed(), 0);
}

/// Pending requests are capped per issuer; a full issuer records nothing new until expired ones are swept.
#[test]
fn pending_quota_per_issuer() {
    let f = fixture_with(ApprovalStoreConfig {
        max_pending_per_issuer: 1,
        ..ApprovalStoreConfig::default()
    });
    let d1 = f.required(refund("acme-inv-001", BIG));
    let d2 = f.required(refund("acme-inv-002", BIG));
    assert!(f.approvals.pending(&app(), &d1, f.clock.now()).is_some());
    assert_eq!(f.approvals.pending(&app(), &d2, f.clock.now()), None);
    f.clock.0.store(START_MS + 900_000, Ordering::SeqCst);
    let d3 = f.required(refund("acme-inv-002", BIG + 1));
    assert!(f.approvals.pending(&app(), &d3, f.clock.now()).is_some());
}

/// Stored approvals are capped per issuer; `approve` sweeps used and expired ones before failing `Unavailable`, and a refused approve keeps the pending request.
#[test]
fn approval_quota_per_issuer() {
    let f = fixture_with(ApprovalStoreConfig {
        max_approvals_per_issuer: 1,
        ..ApprovalStoreConfig::default()
    });
    let id = f.approve_big();
    let d2 = f.required(refund("acme-inv-002", BIG));
    assert_eq!(
        f.approvals.approve(&app(), f.spec(d2), f.clock.now()),
        Err(ApprovalError::Unavailable)
    );
    assert!(f.approvals.pending(&app(), &d2, f.clock.now()).is_some());
    assert!(!f.call(refund("acme-inv-001", BIG), with_id(&id)).is_error());
    f.approve(f.spec(d2));
}

/// Two simultaneous uses of one approval execute exactly once; the rest are `approval_invalid`.
#[test]
fn concurrent_uses_execute_once() {
    let f = Arc::new(fixture());
    let id = f.approve_big();
    let n = 16;
    let barrier = Arc::new(Barrier::new(n));
    let outs: Vec<CallOutcome> = (0..n)
        .map(|_| {
            let (f, id, barrier) = (f.clone(), id.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                f.call(refund("acme-inv-001", BIG), with_id(&id))
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect();
    let ok = outs.iter().filter(|o| !o.is_error()).count();
    assert_eq!(ok, 1, "{outs:?}");
    assert!(outs
        .iter()
        .filter(|o| o.is_error())
        .all(|o| o.code() == Some("approval_invalid")));
    assert_eq!(f.executed(), 1);
}

/// Approval ids presented by the agent and rejected never reach the outcome, audit, or `tracing`; the store's `Debug` shows no approvals.
#[test]
fn rejected_approval_id_not_echoed() {
    let f = fixture();
    let id = f.approve_big();
    let probe = "ap_canary0123456789abcdef0123456789";
    let out = f.call(
        refund("acme-inv-001", BIG),
        CallMeta {
            approval_id: Some(probe.into()),
            ..CallMeta::default()
        },
    );
    f.assert_invalid(&out);
    assert!(!format!("{out:?}").contains(probe));
    let audit = serde_json::to_string(&*f.sink.0.lock().unwrap()).unwrap();
    assert!(!audit.contains(probe));
    let traced = String::from_utf8(traces().0.lock().unwrap().clone()).unwrap();
    assert!(!traced.contains(probe));
    let dbg = format!("{:?}", f.approvals);
    assert!(
        !dbg.contains(id.as_str()) && !dbg.contains("acme-supervisor"),
        "{dbg}"
    );
}

/// `approve` rejects a zero or over-long TTL, stores nothing, and keeps the pending request.
#[test]
fn approve_rejects_bad_specs() {
    let f = fixture();
    let d = f.required(refund("acme-inv-001", BIG));
    for ttl_seconds in [0, DEFAULT_APPROVAL_MAX_TTL_SECONDS + 1] {
        assert!(matches!(
            f.approvals.approve(
                &app(),
                ApprovalSpec {
                    ttl_seconds,
                    ..f.spec(d)
                },
                f.clock.now()
            ),
            Err(ApprovalError::InvalidSpec(_))
        ));
    }
    assert_eq!(f.approvals.len(), 0);
    assert!(f.approvals.pending(&app(), &d, f.clock.now()).is_some());
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

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
