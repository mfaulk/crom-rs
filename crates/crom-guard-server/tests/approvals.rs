//! Admin approval routes (#354 store, #357 HTTP surface).

mod common;

use common::*;
use serde_json::{json, Value};

const OVER_LIMIT: i64 = 9000;

fn approval_body(digest: &str) -> Value {
    json!({
        "action_digest": digest,
        "approver": {"id": "acme-supervisor", "role": "supervisor",
                     "attrs": {"refund_limit_cents": {"long": 100000}}},
        "approver_tenant": "acme",
        "ttl_seconds": 300
    })
}

async fn approval_required(f: &Fixture, handle: &str) -> String {
    let r = f
        .call(
            handle,
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": OVER_LIMIT}),
        )
        .await;
    assert_eq!(r["structuredContent"]["status"], "approval_required", "{r}");
    r["structuredContent"]["action_digest"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn retry(f: &Fixture, handle: &str, approval_id: &str) -> Value {
    f.rpc(
        Some(handle),
        "tools/call",
        json!({
            "name": "refund.issue",
            "arguments": {"invoice_id": "acme-inv-001", "amount_cents": OVER_LIMIT},
            "_meta": {"approval_id": approval_id}
        }),
    )
    .await["result"]
        .clone()
}

/// approval_required, then admin GET and approve, then the agent's retry executes exactly once.
#[tokio::test]
async fn approve_then_retry_executes_once() {
    let mut f = Fixture::start().await;
    let (grant_id, handle) = f.issue(&["refund.issue"]).await;
    let digest = approval_required(&f, &handle).await;

    let (status, view) = f
        .admin_get(&format!("/admin/approvals/{digest}"), ADMIN_TOKEN)
        .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["action_digest"], digest);
    assert_eq!(view["grant_id"], grant_id);
    assert_eq!(view["action"], "refund.issue");
    assert_eq!(view["role"], "supervisor");
    assert_eq!(view["args"]["amount_cents"], OVER_LIMIT);
    assert_eq!(view["resource"]["id"], "acme-inv-001");

    let (status, issued) = f
        .admin(
            "/admin/approvals",
            ADMIN_TOKEN,
            Some(approval_body(&digest)),
        )
        .await;
    assert_eq!(status, 201, "{issued}");
    let id = issued["approval_id"].as_str().unwrap().to_owned();
    assert!(id.starts_with("ap_"));
    assert!(issued["expires_at"].is_u64());

    let r = retry(&f, &handle, &id).await;
    assert_eq!(r["isError"], false, "{r}");
    // Single use: the same id is refused the second time.
    let again = retry(&f, &handle, &id).await;
    assert_eq!(code_of(&again), "approval_invalid", "{again}");

    f.shutdown().await;
    let log = std::fs::read_to_string(f.audit_path()).unwrap();
    assert_eq!(log.matches("\"outcome\":\"executed\"").count(), 1, "{log}");
}

/// Approving consumes the pending request: a second approve and a later GET are 404.
#[tokio::test]
async fn pending_request_is_consumed_by_approve() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    let digest = approval_required(&f, &handle).await;
    let body = approval_body(&digest);
    assert_eq!(
        f.admin("/admin/approvals", ADMIN_TOKEN, Some(body.clone()))
            .await
            .0,
        201
    );
    let (status, err) = f.admin("/admin/approvals", ADMIN_TOKEN, Some(body)).await;
    assert_eq!(status, 404);
    assert_eq!(err["error"], "unknown_request");
    let (status, _) = f
        .admin_get(&format!("/admin/approvals/{digest}"), ADMIN_TOKEN)
        .await;
    assert_eq!(status, 404);
}

/// Unknown or malformed digests, bad TTLs, and missing credentials are refused.
#[tokio::test]
async fn approval_route_errors() {
    let f = Fixture::start().await;
    let zero = "0".repeat(64);
    let (status, _) = f
        .admin_get(&format!("/admin/approvals/{zero}"), ADMIN_TOKEN)
        .await;
    assert_eq!(status, 404);
    let (status, _) = f.admin_get("/admin/approvals/not-hex", ADMIN_TOKEN).await;
    assert_eq!(status, 404);
    let (status, body) = f
        .admin("/admin/approvals", ADMIN_TOKEN, Some(approval_body("zz")))
        .await;
    assert_eq!(status, 422, "{body}");

    let (_, handle) = f.issue(&["refund.issue"]).await;
    let digest = approval_required(&f, &handle).await;
    let mut body = approval_body(&digest);
    body["ttl_seconds"] = json!(0);
    let (status, err) = f.admin("/admin/approvals", ADMIN_TOKEN, Some(body)).await;
    assert_eq!(status, 400, "{err}");
    assert_eq!(err["error"], "invalid_spec");

    // The session handle is not an admin credential here either.
    let (status, _) = f
        .admin("/admin/approvals", &handle, Some(approval_body(&digest)))
        .await;
    assert_eq!(status, 401);
    let (status, _) = f
        .admin_get(&format!("/admin/approvals/{digest}"), &handle)
        .await;
    assert_eq!(status, 401);
}

/// An approver from another tenant yields an approval the pipeline rejects.
#[tokio::test]
async fn cross_tenant_approver_is_rejected_at_use() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    let digest = approval_required(&f, &handle).await;
    let mut body = approval_body(&digest);
    body["approver_tenant"] = json!("globex");
    let (status, issued) = f.admin("/admin/approvals", ADMIN_TOKEN, Some(body)).await;
    assert_eq!(status, 201);
    let r = retry(&f, &handle, issued["approval_id"].as_str().unwrap()).await;
    assert_eq!(code_of(&r), "approval_invalid", "{r}");
}
