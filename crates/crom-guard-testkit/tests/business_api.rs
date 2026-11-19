use std::sync::Arc;

use crom_guard_testkit::{
    ApiCredential, BusinessApi, Effect, Ledger, RefundResponse, RunningApi, DECISION_ID_HEADER,
};
use reqwest::{Client, StatusCode};

const GUARD_SECRET: &str = "guard-secret-canary";

fn guard_cred() -> ApiCredential {
    ApiCredential {
        id: "guard".into(),
        secret: GUARD_SECRET.into(),
    }
}

async fn start(ledger: Ledger) -> (Arc<BusinessApi>, RunningApi) {
    let api = BusinessApi::new(vec![guard_cred()], ledger);
    let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
    (api, running)
}

fn authed(c: &Client, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
    c.request(method, url).bearer_auth(GUARD_SECRET)
}

/// Every endpoint, called once with valid input, as (method, path, body, if-match).
fn one_call_per_endpoint() -> Vec<(
    reqwest::Method,
    &'static str,
    Option<serde_json::Value>,
    Option<&'static str>,
)> {
    use reqwest::Method;
    vec![
        (
            Method::GET,
            "/tenants/acme/customers/acme-cus-000",
            None,
            None,
        ),
        (
            Method::GET,
            "/tenants/acme/customers/acme-cus-000/invoices",
            None,
            None,
        ),
        (
            Method::GET,
            "/tenants/acme/invoices/acme-inv-000",
            None,
            None,
        ),
        (
            Method::POST,
            "/tenants/acme/invoices/acme-inv-000/refunds",
            Some(serde_json::json!({"amount_cents": 100})),
            Some("\"1\""),
        ),
        (Method::POST, "/tenants/acme/customers/export", None, None),
    ]
}

/// Each endpoint appends exactly one ledger entry per successful call, with the matching effect kind.
#[tokio::test]
async fn each_endpoint_writes_exactly_one_entry() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    let kinds = [
        "customer_read",
        "invoice_list",
        "invoice_read",
        "refund",
        "customer_export",
    ];
    for (i, (method, path, body, if_match)) in one_call_per_endpoint().into_iter().enumerate() {
        let mut req = authed(&c, method, format!("{}{path}", running.base_url()));
        if let Some(b) = body {
            req = req.json(&b);
        }
        if let Some(v) = if_match {
            req = req.header("if-match", v);
        }
        let resp = req.send().await.unwrap();
        assert!(resp.status().is_success(), "{path}: {}", resp.status());
        let entries = api.ledger().entries();
        assert_eq!(entries.len(), i + 1, "{path}");
        let last = entries.last().unwrap();
        let kind = serde_json::to_value(&last.effect).unwrap()["kind"].clone();
        assert_eq!(kind, kinds[i], "{path}");
        assert_eq!(last.tenant, "acme");
        assert_eq!(last.credential_id, "guard");
    }
}

/// Requests without a bearer credential, or with a wrong one, get 401 and leave no ledger entry.
#[tokio::test]
async fn missing_or_wrong_credential_is_401_without_effect() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    for (method, path, body, if_match) in one_call_per_endpoint() {
        for auth in [None, Some("not-the-guard-secret")] {
            let mut req = c.request(method.clone(), format!("{}{path}", running.base_url()));
            if let Some(a) = auth {
                req = req.bearer_auth(a);
            }
            if let Some(b) = &body {
                req = req.json(b);
            }
            if let Some(v) = if_match {
                req = req.header("if-match", v);
            }
            assert_eq!(
                req.send().await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "{path} {auth:?}"
            );
        }
    }
    assert!(api.ledger().entries().is_empty());
}

/// An id belonging to another tenant returns 404, identical to a missing id, and records nothing.
#[tokio::test]
async fn cross_tenant_ids_are_not_found() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    let base = running.base_url();
    let paths = [
        "/tenants/acme/customers/globex-cus-000",
        "/tenants/acme/customers/globex-cus-000/invoices",
        "/tenants/acme/invoices/globex-inv-000",
        "/tenants/acme/invoices/no-such-invoice",
    ];
    let mut bodies = Vec::new();
    for p in paths {
        let resp = authed(&c, reqwest::Method::GET, format!("{base}{p}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{p}");
        bodies.push(resp.text().await.unwrap());
    }
    assert!(
        bodies.windows(2).all(|w| w[0] == w[1]),
        "other-tenant and missing must look the same"
    );
    let resp = authed(
        &c,
        reqwest::Method::POST,
        format!("{base}/tenants/acme/invoices/globex-inv-000/refunds"),
    )
    .header("if-match", "\"1\"")
    .json(&serde_json::json!({"amount_cents": 100}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = authed(
        &c,
        reqwest::Method::POST,
        format!("{base}/tenants/nobody/customers/export"),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(api.ledger().entries().is_empty());
    assert!(api
        .snapshot()
        .invoices
        .iter()
        .all(|i| i.refunded_cents == 0));
}

/// Refunds require a current `If-Match` version and a positive amount within the remaining balance.
#[tokio::test]
async fn refunds_are_version_conditioned_and_bounded() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    let url = format!(
        "{}/tenants/acme/invoices/acme-inv-001/refunds",
        running.base_url()
    );
    let refund = |if_match: Option<&'static str>, amount: u64| {
        let mut r = authed(&c, reqwest::Method::POST, url.clone())
            .json(&serde_json::json!({"amount_cents": amount}));
        if let Some(v) = if_match {
            r = r.header("if-match", v);
        }
        r.send()
    };
    assert_eq!(
        refund(None, 100).await.unwrap().status(),
        StatusCode::PRECONDITION_REQUIRED
    );
    assert_eq!(
        refund(Some("\"7\""), 100).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        refund(Some("\"1\""), 0).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // acme-inv-001 is 12_500 cents.
    assert_eq!(
        refund(Some("\"1\""), 12_501).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(api.ledger().entries().is_empty());

    let ok = refund(Some("\"1\""), 12_000).await.unwrap();
    assert_eq!(ok.status(), StatusCode::CREATED);
    let body: RefundResponse = ok.json().await.unwrap();
    assert_eq!(body.invoice.version, 2);
    assert_eq!(body.invoice.remaining_cents(), 500);
    // The old version is now stale, so replaying the same request fails.
    assert_eq!(
        refund(Some("\"1\""), 100).await.unwrap().status(),
        StatusCode::PRECONDITION_FAILED
    );
    assert_eq!(
        refund(Some("\"2\""), 501).await.unwrap().status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(api.ledger().entries().len(), 1);
}

/// The ledger records `X-Guard-Decision-Id` when sent and `None` in direct (Guard-off) mode.
#[tokio::test]
async fn ledger_records_guard_decision_id() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    let url = format!(
        "{}/tenants/globex/customers/globex-cus-001",
        running.base_url()
    );
    authed(&c, reqwest::Method::GET, url.clone())
        .header(DECISION_ID_HEADER, "dec-42")
        .send()
        .await
        .unwrap();
    authed(&c, reqwest::Method::GET, url).send().await.unwrap();
    let e = api.ledger().entries();
    assert_eq!(e[0].guard_decision_id.as_deref(), Some("dec-42"));
    assert_eq!(e[1].guard_decision_id, None);
    assert_eq!(
        e[0].effect,
        Effect::CustomerRead {
            customer_id: "globex-cus-001".into()
        }
    );
}

/// List and export entries name every resource id the response returned.
#[tokio::test]
async fn read_entries_list_returned_ids() {
    let (api, running) = start(Ledger::in_memory()).await;
    let c = Client::new();
    let base = running.base_url();
    let invoices: Vec<serde_json::Value> = authed(
        &c,
        reqwest::Method::GET,
        format!("{base}/tenants/acme/customers/acme-cus-003/invoices"),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let customers: Vec<serde_json::Value> = authed(
        &c,
        reqwest::Method::POST,
        format!("{base}/tenants/globex/customers/export"),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    let ids = |v: &[serde_json::Value]| {
        v.iter()
            .map(|x| x["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let e = api.ledger().entries();
    match &e[0].effect {
        Effect::InvoiceList { invoice_ids, .. } => assert_eq!(*invoice_ids, ids(&invoices)),
        other => panic!("{other:?}"),
    }
    match &e[1].effect {
        Effect::CustomerExport { customer_ids } => {
            assert_eq!(*customer_ids, ids(&customers));
            assert_eq!(customer_ids.len(), 10);
            assert!(customer_ids.iter().all(|id| id.starts_with("globex-")));
        }
        other => panic!("{other:?}"),
    }
}

/// The JSONL file mirrors the in-memory ledger, and `reset` restores seed data and empties both.
#[tokio::test]
async fn jsonl_mirror_and_reset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let (api, running) = start(Ledger::with_file(&path).unwrap()).await;
    let c = Client::new();
    authed(
        &c,
        reqwest::Method::POST,
        format!(
            "{}/tenants/acme/invoices/acme-inv-002/refunds",
            running.base_url()
        ),
    )
    .header("if-match", "\"1\"")
    .json(&serde_json::json!({"amount_cents": 5}))
    .send()
    .await
    .unwrap();
    authed(
        &c,
        reqwest::Method::GET,
        format!("{}/tenants/acme/invoices/acme-inv-002", running.base_url()),
    )
    .send()
    .await
    .unwrap();
    assert_eq!(Ledger::read_file(&path).unwrap(), api.ledger().entries());
    assert_eq!(api.ledger().entries().len(), 2);
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains(GUARD_SECRET),
        "ledger must never contain the credential secret"
    );

    api.reset().unwrap();
    assert!(api.ledger().entries().is_empty());
    assert!(Ledger::read_file(&path).unwrap().is_empty());
    assert_eq!(api.snapshot(), crom_guard_testkit::seed());
    running.shutdown().await;
}

/// A ledger write failure blocks the refund, so the oracle never misses a state change.
#[tokio::test]
async fn ledger_failure_blocks_the_effect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    let (api, running) = start(Ledger::with_file(&path).unwrap()).await;
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap(); // appending to a directory fails
    let resp = authed(
        &c(),
        reqwest::Method::POST,
        format!(
            "{}/tenants/acme/invoices/acme-inv-000/refunds",
            running.base_url()
        ),
    )
    .header("if-match", "\"1\"")
    .json(&serde_json::json!({"amount_cents": 100}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(api.ledger().entries().is_empty());
    assert_eq!(api.snapshot().invoices[0].refunded_cents, 0);
    assert_eq!(api.snapshot().invoices[0].version, 1);
}

fn c() -> Client {
    Client::new()
}

/// Reads honour an optional `If-Match`: a stale one is 412 with no ledger entry, a current one succeeds with an `ETag`.
#[tokio::test]
async fn reads_honour_if_match() {
    let (api, running) = start(Ledger::in_memory()).await;
    let base = running.base_url();
    for path in [
        "/tenants/acme/customers/acme-cus-000",
        "/tenants/acme/customers/acme-cus-000/invoices",
        "/tenants/acme/invoices/acme-inv-000",
    ] {
        let stale = authed(&c(), reqwest::Method::GET, format!("{base}{path}"))
            .header("if-match", "\"7\"")
            .send()
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED, "{path}");
        let ok = authed(&c(), reqwest::Method::GET, format!("{base}{path}"))
            .header("if-match", "\"1\"")
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK, "{path}");
        assert_eq!(ok.headers()["etag"], "\"1\"", "{path}");
    }
    assert_eq!(api.ledger().entries().len(), 3);
}

/// Out-of-band updates bump the version, and trusted lookups are tenant-scoped and leave no ledger entry.
#[tokio::test]
async fn updates_bump_version_and_lookups_are_unledgered() {
    let (api, _running) = start(Ledger::in_memory()).await;
    assert!(api.update_invoice("acme", "acme-inv-000", |i| {
        i.customer_id = "acme-cus-001".into()
    }));
    let inv = api.lookup_invoice("acme", "acme-inv-000").unwrap();
    assert_eq!((inv.customer_id.as_str(), inv.version), ("acme-cus-001", 2));
    assert!(api.update_customer("acme", "acme-cus-000", |c| c.notes.clear()));
    assert_eq!(
        api.lookup_customer("acme", "acme-cus-000").unwrap().version,
        2
    );

    assert!(!api.update_invoice("acme", "globex-inv-000", |_| {}));
    assert!(!api.update_customer("acme", "globex-cus-000", |_| {}));
    assert!(api.lookup_invoice("acme", "globex-inv-000").is_none());
    assert!(api.lookup_customer("acme", "globex-cus-000").is_none());
    assert!(api.lookup_customer("acme", "acme-cus-999").is_none());
    assert!(api.ledger().entries().is_empty());
}
