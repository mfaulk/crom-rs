//! Compatibility with Crom's own Streamable HTTP client (`crom-mcp`), used
//! here as a transport only: no Crom runtime, policy, or session.

mod common;

use std::collections::BTreeMap;

use common::*;
use crom_mcp::{McpClient, McpLimits, McpServerConfig, StreamableHttpMcpServerConfig};
use serde_json::json;

async fn connect(f: &Fixture, handle: &str) -> McpClient {
    let config = McpServerConfig::StreamableHttp(StreamableHttpMcpServerConfig {
        url: f.mcp_url(),
        headers: BTreeMap::from([("Authorization".into(), format!("Bearer {handle}"))]),
        startup_timeout: None,
        request_timeout: None,
        guard: false,
    });
    McpClient::connect(
        "guard",
        &config,
        McpLimits::default(),
        vec![handle.to_owned()],
    )
    .await
    .unwrap()
}

/// crom-mcp initializes, lists only granted tools, and gets executed and denied results.
#[tokio::test]
async fn crom_mcp_client_round_trip() {
    let f = Fixture::start().await;
    let (_, handle) = f.issue(&["customer.read"]).await;
    let mut client = connect(&f, &handle).await;
    let init = client.initialize().await.unwrap();
    assert_eq!(init.server_info.name, "crom-guard");

    let tools = client.list_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["customer.read"]);

    let ok = client
        .call_tool("customer.read", json!({"customer_id": "acme-cus-001"}))
        .await
        .unwrap();
    assert!(!ok.is_error);
    assert_eq!(ok.structured_content.unwrap()["resource"], "acme-cus-001");
    assert!(ok.extra["_meta"]["decision_id"].is_string());

    let denied = client
        .call_tool("customer.read", json!({"customer_id": "globex-cus-001"}))
        .await
        .unwrap();
    assert!(denied.is_error);
    assert_eq!(
        denied.structured_content.unwrap(),
        json!({"status": "denied", "code": "resource_not_found"})
    );
}

/// crom-mcp sees the session error when its handle carries no live grant.
#[tokio::test]
async fn crom_mcp_client_without_grant() {
    let f = Fixture::start().await;
    let mut client = connect(&f, &format!("gsh_{}", "1".repeat(64))).await;
    client.initialize().await.unwrap();
    let err = client.list_tools().await.unwrap_err();
    assert!(err.to_string().contains("no_grant"), "{err}");
}
