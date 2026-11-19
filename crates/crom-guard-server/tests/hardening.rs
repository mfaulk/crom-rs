//! Listener hardening from the #357 security review: admin socket
//! placement, connection caps, the deny rate limit, and the admin Host
//! allowlist.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::*;
use crom_guard_server::{AdminListen, StartError};
use serde_json::json;
use tokio::io::AsyncWriteExt;

/// The admin socket is refused in a directory other users can write to.
#[tokio::test]
async fn admin_socket_in_group_writable_dir_is_refused() {
    let parent = tempfile::tempdir().unwrap();
    std::fs::set_permissions(parent.path(), std::fs::Permissions::from_mode(0o770)).unwrap();
    let sock = parent.path().join("admin.sock");
    let (_d, _g, res) =
        Fixture::try_start(|c| c.admin.listen = AdminListen::Unix(sock.clone())).await;
    let err = res.unwrap_err();
    assert!(matches!(err, StartError::Bind { .. }), "{err:?}");
    assert!(err.to_string().contains("directory"), "{err}");
    assert!(!sock.exists());
}

/// A socket another live server is listening on is never replaced.
#[tokio::test]
async fn live_admin_socket_is_not_replaced() {
    let parent = tempfile::tempdir().unwrap();
    let sock = parent.path().join("admin.sock");
    let first = Fixture::start_with(|c| c.admin.listen = AdminListen::Unix(sock.clone())).await;
    let (_d, _g, res) =
        Fixture::try_start(|c| c.admin.listen = AdminListen::Unix(sock.clone())).await;
    let err = res.unwrap_err();
    assert!(err.to_string().contains("in use"), "{err}");
    // The first server still owns and serves its socket.
    assert!(tokio::net::UnixStream::connect(&sock).await.is_ok());
    drop(first);
}

/// A stale socket left by a dead server is replaced.
#[tokio::test]
async fn stale_admin_socket_is_replaced() {
    let parent = tempfile::tempdir().unwrap();
    let sock = parent.path().join("admin.sock");
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    let mut f = Fixture::start_with(|c| c.admin.listen = AdminListen::Unix(sock.clone())).await;
    let mode = std::fs::symlink_metadata(&sock)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    f.shutdown().await;
}

/// Connections beyond `max_connections` are not served until a slot frees.
#[tokio::test]
async fn agent_connection_cap() {
    let f = Fixture::start_with(|c| {
        c.limits.max_connections = 1;
        c.limits.request_read_timeout_ms = 5_000;
    })
    .await;
    let mut hog = tokio::net::TcpStream::connect(f.server().agent_addr)
        .await
        .unwrap();
    hog.write_all(b"POST /mcp HTTP/1.1\r\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let blocked = tokio::time::timeout(
        Duration::from_millis(400),
        f.rpc_raw(None, "initialize", initialize_params()),
    )
    .await;
    assert!(blocked.is_err(), "second connection served past the cap");
    drop(hog);
    let ok = tokio::time::timeout(
        Duration::from_secs(5),
        f.rpc_raw(None, "initialize", initialize_params()),
    )
    .await
    .expect("not served after the slot freed");
    assert_eq!(ok.status(), 200);
}

/// After `deny_burst` denied calls, a peer is refused before Guard runs (nothing audited).
#[tokio::test]
async fn denied_calls_are_rate_limited() {
    let mut f = Fixture::start_with(|c| {
        c.limits.deny_burst = 3;
        c.limits.deny_refill_per_second = 0.001;
    })
    .await;
    let bogus = format!("gsh_{}", "0".repeat(64));
    let args = json!({"customer_id": "acme-cus-001"});
    for _ in 0..3 {
        let r = f.call(&bogus, "customer.read", args.clone()).await;
        assert_eq!(code_of(&r), "no_grant", "{r}");
    }
    let r = f
        .rpc(
            Some(&bogus),
            "tools/call",
            json!({"name": "customer.read", "arguments": args}),
        )
        .await;
    assert_eq!(r["error"]["message"], "rate_limited", "{r}");
    let r = f.rpc(Some(&bogus), "tools/list", json!({})).await;
    assert_eq!(r["error"]["message"], "rate_limited", "{r}");
    f.shutdown().await;
    let log = std::fs::read_to_string(f.audit_path()).unwrap();
    assert_eq!(log.lines().count(), 3, "rate-limited calls reached Guard");
}

/// Allowed calls do not consume the deny budget.
#[tokio::test]
async fn allowed_calls_are_not_rate_limited() {
    let f = Fixture::start_with(|c| {
        c.limits.deny_burst = 1;
        c.limits.deny_refill_per_second = 0.001;
    })
    .await;
    let (_, handle) = f.issue(&["customer.read"]).await;
    for _ in 0..5 {
        let r = f
            .call(
                &handle,
                "customer.read",
                json!({"customer_id": "acme-cus-001"}),
            )
            .await;
        assert_eq!(r["isError"], false, "{r}");
    }
}

/// The admin TCP listener refuses a non-loopback `Host` header (DNS rebinding).
#[tokio::test]
async fn admin_tcp_requires_loopback_host() {
    let f = Fixture::start().await;
    let resp = f
        .http
        .get(f.admin_url("/admin/health"))
        .bearer_auth(ADMIN_TOKEN)
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    for host in ["localhost:1234", "127.0.0.1", "[::1]:9"] {
        let resp = f
            .http
            .get(f.admin_url("/admin/health"))
            .bearer_auth(ADMIN_TOKEN)
            .header("Host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{host}");
    }
}
