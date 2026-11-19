//! Credential canary (#352, #355): the broker's secret never reaches an
//! agent-visible result, an audit line, an error string, or `tracing`
//! output, across allowed, denied, approval-required, and failed calls. Its
//! own test binary, because it installs a global subscriber.
#![cfg(unix)]

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::{env, permissive_export_policies, CANARY, POLICIES};
use crom_guard::{CallMeta, CallOutcome, DecisionKind, RecordKind};
use serde_json::json;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn texts(o: &CallOutcome) -> String {
    format!("{o:?}\n{}", serde_json::to_string(o).unwrap())
}

/// The canary secret appears in no outcome, audit line, error string, ledger entry, or trace event for any kind of call.
#[tokio::test]
async fn broker_secret_never_leaves_guard() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();

    let e = env(POLICIES).await;
    let mut seen = Vec::new();
    // Allowed, approval-required, policy deny, other tenant, invalid and
    // unknown tool.
    for (tool, args) in [
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 1_000}),
        ),
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 6_000}),
        ),
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-002", "amount_cents": 100}),
        ),
        ("customer.read", json!({"customer_id": "globex-cus-000"})),
        ("customer.read", json!({"customer_id": "acme-cus-000"})),
        ("invoice.list", json!({"customer_id": "acme-cus-000"})),
        ("customer.read", json!({"customer_id": 1, "secret": true})),
        ("credentials.dump", json!({})),
        ("customer.export", json!({"customer_id": "acme-cus-000"})),
    ] {
        seen.push(e.call(tool, args).await);
    }
    // No handle at all.
    seen.push(
        e.guard
            .call(
                "",
                "customer.read",
                json!({"customer_id": "acme-cus-000"}),
                CallMeta::default(),
            )
            .await,
    );
    // Failed: the invoice changes between authorize and execute.
    let api = e.api.clone();
    e.set_hook(move |r| {
        if r.record == RecordKind::Decision && r.decision == Some(DecisionKind::Allow) {
            api.update_invoice("acme", "acme-inv-000", |i| i.refunded_cents = 1);
        }
    });
    seen.push(
        e.call(
            "refund.issue",
            json!({"invoice_id": "acme-inv-000", "amount_cents": 100}),
        )
        .await,
    );
    // Failed: an executor that refuses after an Allow.
    let permissive = env(&permissive_export_policies()).await;
    seen.push(
        permissive
            .call("customer.export", json!({"customer_id": "acme-cus-000"}))
            .await,
    );

    let codes: Vec<_> = seen
        .iter()
        .map(|o| o.code().unwrap_or("executed"))
        .collect();
    for expected in [
        "executed",
        "approval_required",
        "policy_deny",
        "resource_not_found",
        "invalid_arguments",
        "unknown_action",
        "no_grant",
        "precondition_failed",
        "execution_failed",
    ] {
        assert!(codes.contains(&expected), "{codes:?} lacks {expected}");
    }

    let mut haystacks = vec![
        e.audit_text(),
        permissive.audit_text(),
        e.broker_debug.clone(),
    ];
    haystacks.extend(seen.iter().map(texts));
    for env in [&e, &permissive] {
        haystacks.push(serde_json::to_string(&env.api.ledger().entries()).unwrap());
    }
    let traces = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(traces.contains("guard call"), "subscriber captured nothing");
    haystacks.push(traces);
    for (i, h) in haystacks.iter().enumerate() {
        assert!(!h.contains(CANARY), "canary leaked in haystack {i}");
        assert!(
            !h.contains(&e.handle),
            "session handle leaked in haystack {i}"
        );
        assert!(
            !h.contains(&permissive.handle),
            "session handle leaked in haystack {i}"
        );
    }
}
