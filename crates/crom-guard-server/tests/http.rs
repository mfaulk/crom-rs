//! The agent endpoint and admin API over real HTTP (#357).

mod common;

use std::time::Duration;

use common::*;
use crom_guard_server::{AdminListen, StartError};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// initialize, tools/list, and tools/call work end to end for a raw JSON-RPC client.
#[tokio::test]
async fn initialize_list_and_call() {
    let mut f = Fixture::start().await;
    let (_, handle) = f.issue(&["customer.read", "refund.issue"]).await;

    let init = f
        .rpc(Some(&handle), "initialize", initialize_params())
        .await;
    assert_eq!(init["result"]["serverInfo"]["name"], "crom-guard", "{init}");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    let list = f.rpc(Some(&handle), "tools/list", json!({})).await;
    assert_eq!(tool_names(&list), ["customer.read", "refund.issue"]);

    let r = f
        .call(
            &handle,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["resource"], "acme-cus-001");
    assert_eq!(r["structuredContent"]["credential_id"], "business-api");
    let decision = r["_meta"]["decision_id"].as_str().unwrap();
    assert!(decision.starts_with("dec-"));
    let head = r["_meta"]["audit_head"]["head_hash"].as_str().unwrap();
    assert_eq!(head.len(), 64);

    // The admin view of the audit head matches the receipt the agent got.
    let (status, body) = f.admin("/admin/audit/head", ADMIN_TOKEN, None).await;
    assert_eq!(status, 200);
    assert_eq!(body["head"]["head_hash"], head);
    f.shutdown().await;
    crom_guard::verify_file(f.audit_path(), &[]).unwrap();
}

/// tools/list shows only the operations the bearer's grant lists, never the whole registry.
#[tokio::test]
async fn tools_list_is_scoped_to_the_grant() {
    let f = Fixture::start().await;
    let (_, read_only) = f.issue(&["customer.read"]).await;
    let list = f.rpc(Some(&read_only), "tools/list", json!({})).await;
    assert_eq!(tool_names(&list), ["customer.read"]);
    let tool = &list["result"]["tools"][0];
    assert_eq!(tool["inputSchema"]["additionalProperties"], false);
    assert_eq!(tool["description"], "reference test operation");
}

/// tools/list without a live session handle is an error carrying only a pipeline code.
#[tokio::test]
async fn tools_list_requires_a_live_handle() {
    let f = Fixture::start().await;
    let r = f.rpc(None, "tools/list", json!({})).await;
    assert_eq!(r["error"]["code"], -32001, "{r}");
    assert_eq!(r["error"]["message"], "no_grant");

    let bogus = format!("gsh_{}", "0".repeat(64));
    let r = f.rpc(Some(&bogus), "tools/list", json!({})).await;
    assert_eq!(r["error"]["message"], "no_grant");

    let (id, handle) = f.issue(&["customer.read"]).await;
    let (status, _) = f
        .admin(&format!("/admin/grants/{id}/revoke"), ADMIN_TOKEN, None)
        .await;
    assert_eq!(status, 200);
    let r = f.rpc(Some(&handle), "tools/list", json!({})).await;
    assert_eq!(r["error"]["message"], "grant_revoked");
    assert_eq!(r["error"]["data"], json!({"code": "grant_revoked"}));
}

/// Unknown tool names, aliases, and case variants are denied with `unknown_action`.
#[tokio::test]
async fn unknown_tool_is_denied() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    for name in [
        "refund_issue_v2",
        "Refund.Issue",
        "mcp__guard__refund.issue",
    ] {
        let r = f
            .call(
                &handle,
                name,
                json!({"invoice_id": "acme-inv-001", "amount_cents": 5}),
            )
            .await;
        assert_eq!(r["isError"], true, "{r}");
        assert_eq!(code_of(&r), "unknown_action");
        assert_eq!(r["content"][0]["text"], "unknown_action");
        assert!(r["_meta"]["decision_id"].is_string());
    }
}

/// Calls outside the grant and forbidden calls return stable codes and no internal detail.
#[tokio::test]
async fn denials_are_stable_codes() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["customer.read", "customer.export"]).await;
    let cases = [
        (
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 5}),
            "action_not_granted",
        ),
        (
            "customer.read",
            json!({"customer_id": "globex-cus-001"}),
            "resource_not_found",
        ),
        (
            "customer.read",
            json!({"customer_id": "nope"}),
            "resource_not_found",
        ),
        (
            "customer.read",
            json!({"customer_id": "acme-cus-001", "tenant_id": "globex"}),
            "invalid_arguments",
        ),
        (
            "customer.export",
            json!({"customer_id": "acme-cus-001"}),
            "policy_deny",
        ),
    ];
    for (tool, args, code) in cases {
        let r = f.call(&handle, tool, args).await;
        assert_eq!(r["isError"], true, "{tool}: {r}");
        assert_eq!(code_of(&r), code, "{tool}: {r}");
        assert_eq!(r["structuredContent"]["status"], "denied");
    }
    // An executor failure is reported only as `execution_failed`.
    let (_, refund) = f.issue(&["refund.issue"]).await;
    let r = f
        .call(
            &refund,
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 13}),
        )
        .await;
    assert_eq!(
        r["structuredContent"],
        json!({"status": "failed", "code": "execution_failed"})
    );
    assert!(!r.to_string().contains(SECRET));
}

/// `_meta` correlation reaches the audit log as untrusted data, and `_meta.approval_id` reaches the approval verifier.
#[tokio::test]
async fn request_meta_is_passed_to_the_pipeline() {
    let mut f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    let r = f
        .rpc(
            Some(&handle),
            "tools/call",
            json!({
                "name": "refund.issue",
                "arguments": {"invoice_id": "acme-inv-001", "amount_cents": 9000},
                "_meta": {"approval_id": "apr-forged", "tool_use_id": "toolu_42"}
            }),
        )
        .await;
    // No approval store yet (#354): every presented approval is invalid.
    assert_eq!(code_of(&r["result"]), "approval_invalid", "{r}");
    f.shutdown().await;
    let log = std::fs::read_to_string(f.audit_path()).unwrap();
    assert!(log.contains("\"tool_use_id\":\"toolu_42\""), "{log}");
}

/// Over-limit refunds come back as `approval_required` with the role and digest.
#[tokio::test]
async fn approval_required_is_reported() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    let r = f
        .call(
            &handle,
            "refund.issue",
            json!({"invoice_id": "acme-inv-001", "amount_cents": 9000}),
        )
        .await;
    assert_eq!(code_of(&r), "", "{r}");
    assert_eq!(r["structuredContent"]["status"], "approval_required");
    assert_eq!(r["structuredContent"]["role"], "supervisor");
    assert_eq!(r["content"][0]["text"], "approval_required");
}

/// A request body over `max_request_bytes` is refused with 413 before reaching Guard.
#[tokio::test]
async fn oversized_request_is_rejected() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["customer.read"]).await;
    let big = "x".repeat(200 * 1024);
    let resp = f
        .rpc_raw(
            Some(&handle),
            "tools/call",
            json!({"name": "customer.read", "arguments": {"customer_id": big}}),
        )
        .await;
    assert_eq!(resp.status(), 413);
    // Nothing was audited for it.
    let (_, head) = f.admin("/admin/audit/head", ADMIN_TOKEN, None).await;
    assert_eq!(head["head"], Value::Null);
    // The admin listener has its own cap.
    let resp = f
        .http
        .post(f.admin_url("/admin/grants"))
        .bearer_auth(ADMIN_TOKEN)
        .header("content-type", "application/json")
        .body(format!("\"{}\"", "y".repeat(100 * 1024)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

/// A session handle is never accepted as an admin credential.
#[tokio::test]
async fn session_handle_rejected_on_admin_api() {
    let f = Fixture::start().await;
    let (id, handle) = f.issue(&["customer.read"]).await;
    for (path, body) in [
        ("/admin/grants", Some(grant_spec(&["customer.read"]))),
        (&*format!("/admin/grants/{id}/revoke"), None),
        ("/admin/credentials/reload", None),
        ("/admin/health", None),
        ("/admin/audit/head", None),
    ] {
        let (status, body) = f.admin(path, &handle, body).await;
        assert_eq!(status, 401, "{path}");
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    // The grant was not revoked by the rejected request.
    let r = f
        .call(
            &handle,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await;
    assert_eq!(r["isError"], false, "{r}");
}

/// The admin credential is never accepted on the agent endpoint.
#[tokio::test]
async fn admin_credential_rejected_on_agent_endpoint() {
    let f = Fixture::start().await;
    let r = f.rpc(Some(ADMIN_TOKEN), "tools/list", json!({})).await;
    assert_eq!(r["error"]["message"], "no_grant");
    let r = f
        .call(
            ADMIN_TOKEN,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await;
    assert_eq!(code_of(&r), "no_grant", "{r}");
}

/// The admin API refuses missing or wrong credentials and any browser `Origin`.
#[tokio::test]
async fn admin_api_requires_its_credential() {
    let f = Fixture::start().await;
    let resp = f
        .http
        .get(f.admin_url("/admin/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let wrong = format!("{}x", &ADMIN_TOKEN[..ADMIN_TOKEN.len() - 1]);
    assert_eq!(f.admin("/admin/health", &wrong, None).await.0, 401);
    let resp = f
        .http
        .get(f.admin_url("/admin/health"))
        .bearer_auth(ADMIN_TOKEN)
        .header("Origin", "http://localhost:3000")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    let (status, body) = f.admin("/admin/health", ADMIN_TOKEN, None).await;
    assert_eq!(status, 200);
    assert_eq!(body, json!({"status": "ok", "operations": 3}));
    // Unknown admin paths need the credential too.
    let resp = f.http.get(f.admin_url("/admin/nope")).send().await.unwrap();
    assert_eq!(resp.status(), 401);
}

/// The agent endpoint refuses browser-originated requests (no CORS).
#[tokio::test]
async fn agent_endpoint_refuses_origin() {
    let f = Fixture::start().await;
    let resp = f
        .http
        .post(f.mcp_url())
        .header("Accept", "application/json, text/event-stream")
        .header("Origin", "http://evil.example")
        .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": initialize_params()}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
}

/// Issue validation errors and unknown grants map to 400 and 404.
#[tokio::test]
async fn admin_grant_errors() {
    let f = Fixture::start().await;
    let (status, body) = f
        .admin(
            "/admin/grants",
            ADMIN_TOKEN,
            Some(grant_spec(&["not.registered"])),
        )
        .await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"], "invalid_spec");
    let (status, body) = f
        .admin(
            "/admin/grants",
            ADMIN_TOKEN,
            Some(json!({"tenant": "acme"})),
        )
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "invalid_request");
    let (status, _) = f
        .admin("/admin/grants/g_missing/revoke", ADMIN_TOKEN, None)
        .await;
    assert_eq!(status, 404);
}

/// Reload swaps in new downstream credentials; a rejected file keeps the old ones.
#[tokio::test]
async fn credentials_reload() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["customer.read"]).await;
    let creds = f.dir.path().join("creds.json");
    write_private(&creds, &creds_json("business-api-v2", "sk-rotated"));
    let (status, _) = f
        .admin("/admin/credentials/reload", ADMIN_TOKEN, None)
        .await;
    assert_eq!(status, 200);
    let read = || {
        f.call(
            &handle,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
    };
    assert_eq!(
        read().await["structuredContent"]["credential_id"],
        "business-api-v2"
    );

    std::fs::write(&creds, creds_json("business-api-v3", "sk-3")).unwrap();
    std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (status, body) = f
        .admin("/admin/credentials/reload", ADMIN_TOKEN, None)
        .await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(body["error"], "reload_failed");
    assert_eq!(
        read().await["structuredContent"]["credential_id"],
        "business-api-v2"
    );
}

/// A client disconnecting mid-call does not cancel the call or latch Guard closed.
#[tokio::test]
async fn disconnect_mid_call_runs_to_completion() {
    let mut f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue", "customer.read"]).await;
    let req = f
        .http
        .post(f.mcp_url())
        .header("Accept", "application/json, text/event-stream")
        .bearer_auth(&handle)
        .json(
            &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call", "params": {
                "name": "refund.issue",
                "arguments": {"invoice_id": "acme-inv-001", "amount_cents": HOLD_AMOUNT}
            }}),
        )
        .send();
    let client = tokio::spawn(req);
    f.gate.entered.acquire().await.unwrap().forget();
    // Drop the HTTP request mid-execute: the connection closes.
    client.abort();
    let _ = client.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(f.server().in_flight(), 1, "call must still be running");
    f.gate.release.add_permits(1);
    wait_until(|| f.server().in_flight() == 0).await;
    assert!(!f.server().is_failed_closed());

    let r = f
        .call(
            &handle,
            "customer.read",
            json!({"customer_id": "acme-cus-001"}),
        )
        .await;
    assert_eq!(r["isError"], false, "Guard latched: {r}");
    f.shutdown().await;
    let log = std::fs::read_to_string(f.audit_path()).unwrap();
    assert!(log.contains("\"outcome\":\"executed\""), "{log}");
    crom_guard::verify_file(f.audit_path(), &[]).unwrap();
}

/// Shutdown stops new connections but lets an in-flight call finish and answer.
#[tokio::test]
async fn graceful_shutdown_finishes_in_flight_calls() {
    let mut f = Fixture::start().await;
    let (_, handle) = f.issue(&["refund.issue"]).await;
    let url = f.mcp_url();
    let pending = tokio::spawn({
        let http = f.http.clone();
        let handle = handle.clone();
        async move {
            http.post(url)
                .header("Accept", "application/json, text/event-stream")
                .bearer_auth(handle)
                .json(
                    &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
                        "name": "refund.issue",
                        "arguments": {"invoice_id": "acme-inv-001", "amount_cents": HOLD_AMOUNT}
                    }}),
                )
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        }
    });
    f.gate.entered.acquire().await.unwrap().forget();
    let agent_addr = f.server().agent_addr;
    let server = f.server.take().unwrap();
    let stopping = tokio::spawn(server.shutdown());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!stopping.is_finished(), "shutdown must wait for the call");
    assert!(
        tokio::net::TcpStream::connect(agent_addr).await.is_err(),
        "listener still accepting during shutdown"
    );
    f.gate.release.add_permits(1);
    let r = pending.await.unwrap();
    assert_eq!(r["result"]["isError"], false, "{r}");
    tokio::time::timeout(Duration::from_secs(5), stopping)
        .await
        .unwrap()
        .unwrap();
    let log = std::fs::read_to_string(f.audit_path()).unwrap();
    assert!(log.contains("\"outcome\":\"executed\""), "{log}");
}

/// A request whose body stalls is answered 408 after the read timeout.
#[tokio::test]
async fn slow_request_body_times_out() {
    let f = Fixture::start().await;
    let mut s = tokio::net::TcpStream::connect(f.server().agent_addr)
        .await
        .unwrap();
    s.write_all(
        b"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n\
          Accept: application/json, text/event-stream\r\nContent-Length: 100\r\n\r\n{\"jsonrpc\"",
    )
    .await
    .unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "server never gave up on the stalled body");
    let text = String::from_utf8_lossy(&buf);
    assert!(text.starts_with("HTTP/1.1 408"), "{text}");
}

/// A connection that never finishes its headers is closed after the read timeout.
#[tokio::test]
async fn slow_request_headers_time_out() {
    let f = Fixture::start().await;
    let mut s = tokio::net::TcpStream::connect(f.server().agent_addr)
        .await
        .unwrap();
    s.write_all(b"POST /mcp HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await
        .unwrap();
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "server never closed the stalled connection");
}

/// The server refuses to start with an invalid policy.
#[tokio::test]
async fn refuses_invalid_policy() {
    let (_d, _g, res) = Fixture::try_start(|c| {
        std::fs::write(
            &c.policy.policies,
            "permit (principal, action, resource) when { nope };",
        )
        .unwrap();
    })
    .await;
    assert!(matches!(res, Err(StartError::Policy(_))), "{res:?}");
}

/// The server refuses to start with a group- or world-readable credential or token file.
#[tokio::test]
async fn refuses_readable_secret_files() {
    use std::os::unix::fs::PermissionsExt;
    let (_d, _g, res) = Fixture::try_start(|c| {
        let p = c.credentials.file.as_ref().unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o644)).unwrap();
    })
    .await;
    assert!(matches!(res, Err(StartError::Credentials(_))), "{res:?}");
    let (_d, _g, res) = Fixture::try_start(|c| {
        let p = c.admin.token_file.as_ref().unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o640)).unwrap();
    })
    .await;
    let err = res.unwrap_err();
    assert!(matches!(err, StartError::AdminToken(_)), "{err:?}");
    assert!(!err.to_string().contains(ADMIN_TOKEN));
}

/// A non-loopback agent bind is refused without an explicit opt-in.
#[tokio::test]
async fn refuses_non_loopback_bind_by_default() {
    let (_d, _g, res) = Fixture::try_start(|c| {
        c.agent.listen = "0.0.0.0:0".parse().unwrap();
    })
    .await;
    assert!(matches!(res, Err(StartError::Config(_))), "{res:?}");
}

/// The unix admin socket is created 0600 and serves the admin API with its token.
#[tokio::test]
async fn unix_admin_socket_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("admin.sock");
    let mut f = Fixture::start_with(|c| c.admin.listen = AdminListen::Unix(sock.clone())).await;
    let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    let send = |token: &'static str| {
        let sock = sock.clone();
        async move {
            let mut s = tokio::net::UnixStream::connect(&sock).await.unwrap();
            let req = format!(
                "GET /admin/health HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\
                 Connection: close\r\n\r\n"
            );
            s.write_all(req.as_bytes()).await.unwrap();
            let mut buf = Vec::new();
            s.read_to_end(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        }
    };
    assert!(send(ADMIN_TOKEN).await.starts_with("HTTP/1.1 200"));
    assert!(send("not-the-token").await.starts_with("HTTP/1.1 401"));
    f.shutdown().await;
    assert!(!sock.exists(), "socket left behind after shutdown");
}

async fn wait_until(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not reached");
}
