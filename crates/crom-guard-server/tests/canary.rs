//! Canary scan at the HTTP layer: the session handle, the admin token, and
//! the downstream credential appear in no agent response, admin response
//! (other than the issue response that delivers the handle), audit line,
//! or `tracing` output captured at TRACE from every crate, rmcp included.

mod common;

use std::io::Write;
use std::sync::{Arc, Mutex};

use common::*;
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

/// No secret leaks through any HTTP response, audit record, or trace event.
#[tokio::test]
async fn secrets_never_leave_guard() {
    let logs = Capture::default();
    let sink = logs.clone();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .init();

    let mut f = Fixture::start().await;
    let (grant_id, handle) = f.issue(&["customer.read", "refund.issue"]).await;
    let mut seen = Vec::new();

    let mut rpc = |v: serde_json::Value| seen.push(v.to_string());
    rpc(f
        .rpc(Some(&handle), "initialize", initialize_params())
        .await);
    rpc(f.rpc(Some(&handle), "tools/list", json!({})).await);
    for (tool, args) in [
        ("customer.read", json!({"customer_id": "acme-cus-001"})),
        ("customer.read", json!({"customer_id": "globex-cus-001"})),
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 9000}),
        ),
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 13}),
        ),
        ("refund_issue_v2", json!({})),
    ] {
        rpc(f.call(&handle, tool, args).await);
    }
    rpc(f
        .call(
            ADMIN_TOKEN,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await);
    rpc(f.rpc(Some(ADMIN_TOKEN), "tools/list", json!({})).await);
    for path in [
        "/admin/health",
        "/admin/audit/head",
        "/admin/credentials/reload",
    ] {
        rpc(f.admin(path, ADMIN_TOKEN, None).await.1);
        rpc(f.admin(path, &handle, None).await.1);
    }
    rpc(f
        .admin(
            &format!("/admin/grants/{grant_id}/revoke"),
            ADMIN_TOKEN,
            None,
        )
        .await
        .1);
    rpc(f
        .call(
            &handle,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await);
    f.shutdown().await;

    let audit = std::fs::read_to_string(f.audit_path()).unwrap();
    assert!(audit.lines().count() >= 8, "{audit}");
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("grant issued"), "tracing was not captured");
    assert!(
        logs.contains("TRACE serve_inner: rmcp::service"),
        "rmcp not captured"
    );
    for secret in [handle.as_str(), ADMIN_TOKEN, SECRET] {
        for (i, body) in seen.iter().enumerate() {
            assert!(
                !body.contains(secret),
                "response {i} leaks a secret: {body}"
            );
        }
        assert!(!audit.contains(secret), "audit leaks a secret");
        assert!(!logs.contains(secret), "tracing leaks a secret");
    }
}
