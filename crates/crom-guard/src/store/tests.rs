use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use proptest::prelude::*;

use super::*;
use crate::grant::tests::spec;
use crate::grant::{Constraint, Constraints};
use crate::value::EntityUid;

/// The store's futures never suspend, so polling once completes them.
fn now_ready<F: Future>(f: F) -> F::Output {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("store future suspended"),
    }
}

fn action(s: &str) -> ActionId {
    ActionId::new(s).unwrap()
}

fn store() -> MemoryGrantStore {
    MemoryGrantStore::new(GrantStoreConfig::new(
        [
            action("customer.read"),
            action("invoice.list"),
            action("refund.issue"),
        ]
        .into(),
    ))
}

fn app() -> AppId {
    AppId::new("support-app").unwrap()
}

const T0: Timestamp = Timestamp::from_unix_millis(1_000_000);

fn issue(s: &MemoryGrantStore, spec: GrantSpec) -> Result<IssuedGrant, GrantStoreError> {
    now_ready(s.issue(&app(), spec, T0))
}

fn auth(
    s: &MemoryGrantStore,
    h: &SessionHandle,
    now: Timestamp,
) -> Result<ExecutionContext, DenyReason> {
    now_ready(s.authenticate(h, now))
}

fn customers(ids: &[&str]) -> Constraint {
    Constraint::Set(
        ids.iter()
            .map(|i| EntityUid::new("Support::Customer", *i).unwrap())
            .collect(),
    )
}

fn child(actions: &[&str], cust: &[&str], max_refund: i64, ttl: u64, depth: u8) -> DelegationSpec {
    DelegationSpec {
        actions: actions.iter().map(|a| action(a)).collect(),
        constraints: Constraints::new(
            [
                ("customers".to_owned(), customers(cust)),
                (
                    "max_refund_cents".to_owned(),
                    Constraint::AtMost(max_refund),
                ),
            ]
            .into(),
        )
        .unwrap(),
        ttl_seconds: ttl,
        max_depth: depth,
    }
}

/// An issued handle authenticates to exactly the issued grant, with identity taken from the spec and Guard-assigned ids.
#[test]
fn issue_then_authenticate() {
    let s = store();
    let issued = issue(&s, spec()).unwrap();
    let g = &issued.grant;
    assert_eq!(g.expires_at(), T0.saturating_add_secs(600));
    assert_eq!(g.not_before(), T0);
    assert_eq!(g.issuer(), &app());
    assert!(g.grant_id().as_str().starts_with("g_") && g.agent().as_str().starts_with("a_"));

    let ctx = auth(&s, &issued.handle, T0).unwrap();
    assert_eq!(ctx.leaf(), g);
    assert_eq!(ctx.principal().grant_chain(), [g.grant_id().clone()]);
    assert_eq!(ctx.principal().tenant().as_str(), "acme");
}

/// Each handle maps only to its own grant, and ids and handles are unique per issuance.
#[test]
fn handles_are_distinct_and_bound() {
    let s = store();
    let a = issue(&s, spec()).unwrap();
    let b = issue(&s, spec()).unwrap();
    assert_ne!(a.grant.grant_id(), b.grant.grant_id());
    assert_ne!(a.grant.agent(), b.grant.agent());
    assert_ne!(a.handle.expose_secret(), b.handle.expose_secret());
    assert_eq!(
        auth(&s, &a.handle, T0).unwrap().leaf().grant_id(),
        a.grant.grant_id()
    );
    assert_eq!(
        auth(&s, &b.handle, T0).unwrap().leaf().grant_id(),
        b.grant.grant_id()
    );
}

/// A well-formed but never-issued handle is denied with NoGrant.
#[test]
fn unknown_handle_denied() {
    let s = store();
    issue(&s, spec()).unwrap();
    let unknown = SessionHandle::parse(&format!("gsh_{}", "0".repeat(64))).unwrap();
    assert_eq!(auth(&s, &unknown, T0).unwrap_err(), DenyReason::NoGrant);
}

/// Authentication fails before `not_before` and from `expires_at` on.
#[test]
fn not_yet_valid_and_expired_denied() {
    let s = store();
    let issued = issue(&s, spec()).unwrap();
    let before = Timestamp::from_unix_millis(T0.as_unix_millis() - 1);
    assert_eq!(
        auth(&s, &issued.handle, before).unwrap_err(),
        DenyReason::GrantNotYetValid
    );
    let last = Timestamp::from_unix_millis(issued.grant.expires_at().as_unix_millis() - 1);
    assert!(auth(&s, &issued.handle, last).is_ok());
    assert_eq!(
        auth(&s, &issued.handle, issued.grant.expires_at()).unwrap_err(),
        DenyReason::GrantExpired
    );
}

/// Revocation denies authentication and recheck; only the issuing app may revoke, and unknown ids are reported as such.
#[test]
fn revoke_denies_and_is_issuer_scoped() {
    let s = store();
    let issued = issue(&s, spec()).unwrap();
    let id = issued.grant.grant_id().clone();
    let ctx = auth(&s, &issued.handle, T0).unwrap();

    let other = AppId::new("other-app").unwrap();
    assert_eq!(
        now_ready(s.revoke(&other, &id, T0)),
        Err(GrantStoreError::UnknownGrant)
    );
    assert!(auth(&s, &issued.handle, T0).is_ok());
    let unknown = GrantId::new("g_missing").unwrap();
    assert_eq!(
        now_ready(s.revoke(&app(), &unknown, T0)),
        Err(GrantStoreError::UnknownGrant)
    );

    assert_eq!(now_ready(s.recheck(&ctx, T0)), Ok(()));
    now_ready(s.revoke(&app(), &id, T0)).unwrap();
    assert_eq!(
        auth(&s, &issued.handle, T0).unwrap_err(),
        DenyReason::GrantRevoked
    );
    assert_eq!(
        now_ready(s.recheck(&ctx, T0)),
        Err(DenyReason::GrantRevoked)
    );
}

/// TTLs of 0 or above the configured maximum are rejected; the maximum itself is accepted.
#[test]
fn max_ttl_enforced() {
    let s = store();
    let with_ttl = |t| GrantSpec {
        ttl_seconds: t,
        ..spec()
    };
    assert!(issue(&s, with_ttl(DEFAULT_MAX_TTL_SECONDS)).is_ok());
    assert!(matches!(
        issue(&s, with_ttl(DEFAULT_MAX_TTL_SECONDS + 1)),
        Err(GrantStoreError::InvalidSpec(_))
    ));
    assert!(matches!(
        issue(&s, with_ttl(0)),
        Err(GrantStoreError::InvalidSpec(_))
    ));

    let mut cfg = GrantStoreConfig::new([action("customer.read")].into());
    cfg.max_ttl_seconds = 60;
    let short = MemoryGrantStore::new(cfg);
    assert!(issue(&short, with_ttl(61)).is_err());
    assert!(issue(&short, with_ttl(60)).is_ok());
}

/// Grants naming an unregistered action, or no action, are rejected at issuance.
#[test]
fn unknown_action_rejected_at_issuance() {
    let s = store();
    let mut bad = spec();
    bad.actions.insert(action("customer.export"));
    assert!(
        matches!(issue(&s, bad), Err(GrantStoreError::InvalidSpec(m)) if m.contains("customer.export"))
    );
    let mut empty = spec();
    empty.actions = BTreeSet::new();
    assert!(issue(&s, empty).is_err());
}

/// A narrower child authenticates with the full chain and copies identity from the parent.
#[test]
fn delegate_issues_narrower_child() {
    let s = store();
    let parent = issue(&s, spec()).unwrap();
    let pctx = auth(&s, &parent.handle, T0).unwrap();
    let c = now_ready(s.delegate(
        &pctx,
        child(&["customer.read"], &["acme-cus-001"], 500, 600, 0),
        T0,
    ))
    .unwrap();
    assert_eq!(c.grant.parent(), Some(parent.grant.grant_id()));
    assert_eq!(c.grant.expires_at(), parent.grant.expires_at());
    assert_eq!(c.grant.tenant(), parent.grant.tenant());
    assert_eq!(c.grant.delegator(), parent.grant.delegator());
    assert_eq!(c.grant.issuer(), parent.grant.issuer());
    assert_ne!(c.grant.agent(), parent.grant.agent());

    let cctx = auth(&s, &c.handle, T0).unwrap();
    let chain: Vec<_> = cctx.chain().iter().map(|g| g.grant_id().clone()).collect();
    assert_eq!(
        chain,
        [c.grant.grant_id().clone(), parent.grant.grant_id().clone()]
    );
}

/// Delegation is refused for wider actions, constraints, or expiry, non-decreasing depth, a depth-0 parent, and an expired or revoked parent.
#[test]
fn delegate_rejects_widening() {
    let s = store();
    let parent = issue(&s, spec()).unwrap();
    let pctx = auth(&s, &parent.handle, T0).unwrap();
    let try_child = |d: DelegationSpec| now_ready(s.delegate(&pctx, d, T0));
    // spec(): actions {customer.read}, customers {001, 002}, max_refund 10_000, depth 1.
    assert!(try_child(child(
        &["customer.read", "refund.issue"],
        &["acme-cus-001"],
        1,
        60,
        0
    ))
    .is_err());
    assert!(try_child(child(&["customer.read"], &["acme-cus-003"], 1, 60, 0)).is_err());
    assert!(try_child(child(&["customer.read"], &["acme-cus-001"], 10_001, 60, 0)).is_err());
    assert!(try_child(child(&["customer.read"], &["acme-cus-001"], 1, 60, 1)).is_err());
    // spec() ttl is 600 s, so 601 s would outlive the parent.
    assert!(try_child(child(&["customer.read"], &["acme-cus-001"], 1, 601, 0)).is_err());
    let late = Timestamp::from_unix_millis(parent.grant.expires_at().as_unix_millis());
    assert!(now_ready(s.delegate(
        &pctx,
        child(&["customer.read"], &["acme-cus-001"], 1, 1, 0),
        late
    ))
    .is_err());

    let leaf = try_child(child(&["customer.read"], &["acme-cus-001"], 1, 60, 0)).unwrap();
    let leaf_ctx = auth(&s, &leaf.handle, T0).unwrap();
    assert!(now_ready(s.delegate(
        &leaf_ctx,
        child(&["customer.read"], &["acme-cus-001"], 1, 60, 0),
        T0
    ))
    .is_err());

    now_ready(s.revoke(&app(), parent.grant.grant_id(), T0)).unwrap();
    assert!(try_child(child(&["customer.read"], &["acme-cus-001"], 1, 60, 0)).is_err());
}

/// Revoking a parent denies its child's authentication and recheck.
#[test]
fn revoked_parent_disables_child() {
    let s = store();
    let parent = issue(&s, spec()).unwrap();
    let pctx = auth(&s, &parent.handle, T0).unwrap();
    let c = now_ready(s.delegate(
        &pctx,
        child(&["customer.read"], &["acme-cus-001"], 1, 60, 0),
        T0,
    ))
    .unwrap();
    let cctx = auth(&s, &c.handle, T0).unwrap();
    now_ready(s.revoke(&app(), parent.grant.grant_id(), T0)).unwrap();
    assert_eq!(
        auth(&s, &c.handle, T0).unwrap_err(),
        DenyReason::GrantRevoked
    );
    assert_eq!(
        now_ready(s.recheck(&cctx, T0)),
        Err(DenyReason::GrantRevoked)
    );
}

/// The store's Debug output and stored grants never contain a plaintext handle.
#[test]
fn plaintext_handle_not_stored_or_printed() {
    let s = store();
    let issued = issue(&s, spec()).unwrap();
    let secret = issued.handle.expose_secret().to_owned();
    assert!(!format!("{s:?}").contains(&secret));
    let state = s.state.read().unwrap();
    for g in state.grants.values() {
        assert!(!serde_json::to_string(g).unwrap().contains(&secret));
    }
    for entries in state.handles.values() {
        for e in entries {
            assert_ne!(&e.hash[..], &secret.as_bytes()[..32]);
            assert_eq!(e.hash, hash_handle(&issued.handle));
        }
    }
}

/// No authenticate call that starts after `revoke` returns ever yields a context, and some calls provably start after it.
#[test]
fn revoke_racing_authenticate() {
    for _ in 0..20 {
        let s = Arc::new(store());
        let issued = issue(&s, spec()).unwrap();
        let secret = issued.handle.expose_secret().to_owned();
        let revoked = Arc::new(AtomicBool::new(false));
        let violations = Arc::new(AtomicUsize::new(0));
        let successes = Arc::new(AtomicUsize::new(0));
        let denied_after = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (s, revoked, violations, successes, denied_after, secret) = (
                    s.clone(),
                    revoked.clone(),
                    violations.clone(),
                    successes.clone(),
                    denied_after.clone(),
                    secret.clone(),
                );
                std::thread::spawn(move || {
                    let h = SessionHandle::parse(&secret).unwrap();
                    // Run until some calls have started after the revoke.
                    let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    // Bounded, so a store that never denies fails instead of hanging.
                    while std::time::Instant::now() < give_up {
                        let after_revoke = revoked.load(Ordering::SeqCst);
                        match auth(&s, &h, T0) {
                            Ok(_) => {
                                successes.fetch_add(1, Ordering::SeqCst);
                                if after_revoke {
                                    violations.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                            Err(_) if after_revoke => {
                                if denied_after.fetch_add(1, Ordering::SeqCst) >= 64 {
                                    return;
                                }
                            }
                            Err(_) => {}
                        }
                    }
                })
            })
            .collect();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while successes.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "workers never authenticated"
            );
            std::hint::spin_loop();
        }
        now_ready(s.revoke(&app(), issued.grant.grant_id(), T0)).unwrap();
        revoked.store(true, Ordering::SeqCst);
        for t in threads {
            t.join().expect("worker panicked");
        }
        assert_eq!(violations.load(Ordering::SeqCst), 0);
        assert!(denied_after.load(Ordering::SeqCst) > 0);
    }
}

/// Revoking a parent while its child is being authenticated never lets a later child call succeed.
#[test]
fn parent_revoke_racing_child_authenticate() {
    let s = Arc::new(store());
    let parent = issue(&s, spec()).unwrap();
    let pctx = auth(&s, &parent.handle, T0).unwrap();
    let c = now_ready(s.delegate(
        &pctx,
        child(&["customer.read"], &["acme-cus-001"], 1, 60, 0),
        T0,
    ))
    .unwrap();
    let secret = c.handle.expose_secret().to_owned();
    let revoked = Arc::new(AtomicBool::new(false));
    let worker = {
        let (s, revoked) = (s.clone(), revoked.clone());
        std::thread::spawn(move || {
            let h = SessionHandle::parse(&secret).unwrap();
            let mut checked_after = 0;
            let give_up = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while checked_after < 64 && std::time::Instant::now() < give_up {
                let after = revoked.load(Ordering::SeqCst);
                let r = auth(&s, &h, T0);
                if after {
                    assert!(
                        r.is_err(),
                        "child authenticated after parent revoke returned"
                    );
                    checked_after += 1;
                }
            }
        })
    };
    now_ready(s.revoke(&app(), parent.grant.grant_id(), T0)).unwrap();
    revoked.store(true, Ordering::SeqCst);
    worker.join().expect("worker panicked");
}

/// A grant can delegate at most `max_children_per_grant` children.
#[test]
fn children_per_grant_bounded() {
    let mut cfg = GrantStoreConfig::new([action("customer.read")].into());
    cfg.max_children_per_grant = 2;
    let s = MemoryGrantStore::new(cfg);
    let parent = issue(&s, spec()).unwrap();
    let pctx = auth(&s, &parent.handle, T0).unwrap();
    let d = || child(&["customer.read"], &["acme-cus-001"], 1, 60, 0);
    assert!(now_ready(s.delegate(&pctx, d(), T0)).is_ok());
    assert!(now_ready(s.delegate(&pctx, d(), T0)).is_ok());
    assert!(now_ready(s.delegate(&pctx, d(), T0)).is_err());
}

/// A full store sweeps expired grants before refusing, and refuses with Unavailable when nothing has expired.
#[test]
fn capacity_bounded_and_expired_swept() {
    let mut cfg = GrantStoreConfig::new([action("customer.read")].into());
    cfg.max_grants = 2;
    let s = MemoryGrantStore::new(cfg);
    let short = || GrantSpec {
        ttl_seconds: 1,
        ..spec()
    };
    let a = issue(&s, short()).unwrap();
    issue(&s, short()).unwrap();
    assert_eq!(issue(&s, short()).err(), Some(GrantStoreError::Unavailable));
    let later = T0.saturating_add_secs(1);
    let fresh = now_ready(s.issue(&app(), short(), later)).unwrap();
    assert_eq!(auth(&s, &a.handle, later).unwrap_err(), DenyReason::NoGrant);
    assert!(auth(&s, &fresh.handle, later).is_ok());
}

/// Delegation narrows across several levels, and every ancestor stays in the chain.
#[test]
fn multi_level_delegation() {
    let s = store();
    let root = issue(
        &s,
        GrantSpec {
            max_depth: 2,
            ..spec()
        },
    )
    .unwrap();
    let rctx = auth(&s, &root.handle, T0).unwrap();
    let mid = now_ready(s.delegate(
        &rctx,
        child(
            &["customer.read"],
            &["acme-cus-001", "acme-cus-002"],
            900,
            300,
            1,
        ),
        T0,
    ))
    .unwrap();
    let mctx = auth(&s, &mid.handle, T0).unwrap();
    // Wider than mid (but within root) is still rejected.
    assert!(now_ready(s.delegate(
        &mctx,
        child(&["customer.read"], &["acme-cus-001"], 1_000, 60, 0),
        T0
    ))
    .is_err());
    let leaf = now_ready(s.delegate(
        &mctx,
        child(&["customer.read"], &["acme-cus-001"], 100, 60, 0),
        T0,
    ))
    .unwrap();
    let lctx = auth(&s, &leaf.handle, T0).unwrap();
    assert_eq!(lctx.chain().len(), 3);
    now_ready(s.revoke(&app(), root.grant.grant_id(), T0)).unwrap();
    assert_eq!(
        auth(&s, &leaf.handle, T0).unwrap_err(),
        DenyReason::GrantRevoked
    );
}

fn arb_spec() -> impl Strategy<Value = GrantSpec> {
    let actions = prop::collection::btree_set(
        prop::sample::select(vec![
            "customer.read",
            "invoice.list",
            "refund.issue",
            "customer.export",
        ]),
        0..4,
    );
    let custs = prop::collection::btree_set(0u8..20, 0..5);
    (
        actions,
        custs,
        any::<i64>(),
        0u64..=2 * DEFAULT_MAX_TTL_SECONDS,
        any::<u8>(),
    )
        .prop_map(|(actions, custs, limit, ttl, depth)| {
            let ids: Vec<String> = custs.iter().map(|c| format!("acme-cus-{c:03}")).collect();
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            GrantSpec {
                actions: actions.into_iter().map(action).collect(),
                constraints: Constraints::new(
                    [
                        ("customers".to_owned(), customers(&refs)),
                        ("max_refund_cents".to_owned(), Constraint::AtMost(limit)),
                    ]
                    .into(),
                )
                .unwrap(),
                ttl_seconds: ttl,
                max_depth: depth,
                ..spec()
            }
        })
}

proptest! {
    /// An arbitrary spec is rejected exactly when it is invalid, and otherwise authenticates with identical authority.
    #[test]
    fn arbitrary_spec_rejected_or_round_trips(spec in arb_spec()) {
        let s = store();
        let valid = !spec.actions.is_empty()
            && !spec.actions.contains(&action("customer.export"))
            && spec.ttl_seconds > 0
            && spec.ttl_seconds <= DEFAULT_MAX_TTL_SECONDS;
        match issue(&s, spec.clone()) {
            Err(_) => prop_assert!(!valid),
            Ok(issued) => {
                prop_assert!(valid);
                let ctx = auth(&s, &issued.handle, T0).unwrap();
                let g = ctx.leaf();
                prop_assert_eq!(g.tenant(), &spec.tenant);
                prop_assert_eq!(g.delegator(), &spec.delegator);
                prop_assert_eq!(g.run_id(), &spec.run_id);
                prop_assert_eq!(g.actions(), &spec.actions);
                prop_assert_eq!(g.constraints(), &spec.constraints);
                prop_assert_eq!(g.max_depth(), spec.max_depth);
                prop_assert_eq!(g.expires_at(), T0.saturating_add_secs(spec.ttl_seconds));
                prop_assert!(g.parent().is_none() && g.revoked_at().is_none());
            }
        }
    }
}
