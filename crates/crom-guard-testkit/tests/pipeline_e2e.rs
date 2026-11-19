//! End to end through `Guard::call` and `SupportOperation::execute` against
//! the HTTP business API; the ledger is the oracle (#351, deferred from #350).
#![cfg(unix)]

mod common;

use common::{env, permissive_export_policies, POLICIES};
use crom_guard::{
    CallResult, Clock, DecisionKind, Enforcement, GrantStore, Outcome, RecordKind, SystemClock,
    PRECONDITION_FAILED,
};
use crom_guard_testkit::Effect;
use serde_json::json;

/// An allowed refund produces exactly one ledger effect, stamped with the call's decision id and matched by an `executed` outcome record.
#[tokio::test]
async fn allowed_refund_has_one_ledger_effect_with_the_decision_id() {
    let e = env(POLICIES).await;
    let out = e
        .call(
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 1_000}),
        )
        .await;
    assert!(matches!(out.result, CallResult::Executed { .. }), "{out:?}");

    let ledger = e.api.ledger().entries();
    assert_eq!(ledger.len(), 1);
    assert_eq!(
        ledger[0].guard_decision_id.as_deref(),
        Some(out.decision_id.as_str())
    );
    assert_eq!(ledger[0].credential_id, "business-api");
    assert!(matches!(
        &ledger[0].effect,
        Effect::Refund { invoice_id, amount_cents: 1_000, .. } if invoice_id == "acme-inv-001"
    ));

    let recs = e.audit_records();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[0].record, RecordKind::Decision);
    assert_eq!(recs[0].decision, Some(DecisionKind::Allow));
    assert_eq!(recs[1].outcome, Some(Outcome::Executed));
    assert_eq!(recs[1].decision_id, out.decision_id);
    assert_eq!(recs[1].downstream_ref.as_deref(), Some("acme-ref-0000"));
    assert_eq!(out.audit_head.unwrap().seq, 2);
}

/// An ownership change after the decision record but before execution yields `precondition_failed` and no ledger effect.
#[tokio::test]
async fn ownership_change_between_authorize_and_execute_has_no_effect() {
    let e = env(POLICIES).await;
    let api = e.api.clone();
    e.set_hook(move |r| {
        if r.decision == Some(DecisionKind::Allow) && r.record == RecordKind::Decision {
            assert!(api.update_invoice("acme", "acme-inv-001", |i| {
                i.customer_id = "acme-cus-005".into()
            }));
        }
    });
    let out = e
        .call(
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 1_000}),
        )
        .await;
    assert_eq!(
        out.result,
        CallResult::Failed {
            code: PRECONDITION_FAILED
        }
    );
    assert!(e.api.ledger().entries().is_empty());
    let recs = e.audit_records();
    assert_eq!(recs[1].outcome, Some(Outcome::Failed));
    assert_eq!(recs[1].enforcement, Some(Enforcement::ExecutorPrecondition));
}

/// Revoking the grant after authorization blocks execution: no ledger effect.
#[tokio::test]
async fn revoke_between_authorize_and_execute_has_no_effect() {
    let e = env(POLICIES).await;
    let store = e.store.clone();
    e.set_hook(move |r| {
        if r.decision == Some(DecisionKind::Allow) && r.record == RecordKind::Decision {
            let id = r.identity.as_ref().unwrap();
            poll_ready(store.revoke(&id.issuer, &id.grant_id, SystemClock.now())).unwrap();
        }
    });
    let out = e
        .call(
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 1_000}),
        )
        .await;
    assert_eq!(out.code(), Some("grant_revoked"));
    assert!(e.api.ledger().entries().is_empty());
}

/// `customer.export` is denied by policy, and with a policy that permits it the executor still fails closed without any downstream effect.
#[tokio::test]
async fn export_denied_by_policy_and_fails_closed_if_permitted() {
    let e = env(POLICIES).await;
    let out = e
        .call("customer.export", json!({"customer_id": "acme-cus-000"}))
        .await;
    assert_eq!(out.code(), Some("policy_deny"));

    let e = env(&permissive_export_policies()).await;
    let out = e
        .call("customer.export", json!({"customer_id": "acme-cus-000"}))
        .await;
    assert_eq!(out.code(), Some("execution_failed"), "{out:?}");
    assert!(e.api.ledger().entries().is_empty());
    let recs = e.audit_records();
    assert_eq!(recs[0].decision, Some(DecisionKind::Allow));
    assert_eq!(recs[1].outcome, Some(Outcome::Failed));
}

/// Another tenant's invoice and a missing one give the same agent-visible denial and no ledger effect.
#[tokio::test]
async fn other_tenant_and_missing_ids_look_the_same() {
    let e = env(POLICIES).await;
    let other = e
        .call(
            "refund.issue",
            json!({"invoice_id": "globex-inv-001", "amount_cents": 100}),
        )
        .await;
    let missing = e
        .call(
            "refund.issue",
            json!({"invoice_id": "acme-inv-999", "amount_cents": 100}),
        )
        .await;
    assert_eq!(other.result, missing.result);
    assert_eq!(other.code(), Some("resource_not_found"));
    assert!(e.api.ledger().entries().is_empty());
}

/// A forged `tenant_id` argument is rejected by the schema before resolution.
#[tokio::test]
async fn forged_tenant_argument_rejected() {
    let e = env(POLICIES).await;
    let out = e
        .call(
            "customer.read",
            json!({"customer_id": "globex-cus-000", "tenant_id": "globex"}),
        )
        .await;
    assert_eq!(out.code(), Some("invalid_arguments"));
    assert!(e.api.ledger().entries().is_empty());
}

/// Polls a future that the in-memory store completes immediately.
fn poll_ready<F: std::future::Future>(f: F) -> F::Output {
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("store future pending"),
    }
}
