//! A full Guard over the support operations, the HTTP business API, a 0600
//! file credential broker, and the hash-chained file audit log.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crom_guard::*;
use crom_guard_testkit::{
    operations, ApiCredential, BusinessApi, Ledger, RunningApi, SupportConfig,
};
use serde_json::Value;

pub const SCHEMA: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedarschema");
pub const POLICIES: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedar");
/// The downstream secret. It must never leave the Guard process.
pub const CANARY: &str = "sk-business-CANARY-3c7e1f9a";

type Hook = Box<dyn Fn(&AuditRecord) + Send + Sync>;

/// `FileAuditSink` plus a hook that runs after a record is durable, to
/// change the world between authorize and execute.
pub struct HookSink {
    inner: FileAuditSink,
    pub hook: Mutex<Option<Hook>>,
}

impl AuditSink for HookSink {
    fn append<'a>(
        &'a self,
        record: &'a AuditRecord,
    ) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
        Box::pin(async move {
            let r = self.inner.append(record).await?;
            if let Some(h) = self.hook.lock().unwrap().as_ref() {
                h(record);
            }
            Ok(r)
        })
    }
}

pub struct Env {
    pub guard: Guard,
    pub api: Arc<BusinessApi>,
    pub sink: Arc<HookSink>,
    pub store: Arc<MemoryGrantStore>,
    pub handle: String,
    pub audit_path: PathBuf,
    pub broker_debug: String,
    _running: RunningApi,
    _dir: tempfile::TempDir,
}

/// Replaces the `no-bulk-export` forbid with a permit, to show the export
/// executor still fails closed.
pub fn permissive_export_policies() -> String {
    let forbid = "@id(\"no-bulk-export\")\nforbid (";
    assert!(POLICIES.contains(forbid));
    POLICIES.replace(forbid, "@id(\"test-permit-export\")\npermit (")
}

pub async fn env(policies: &str) -> Env {
    env_with(policies, grant_spec()).await
}

/// `env` with `guard.delegate` registered and `spec` as the agent's grant.
pub async fn env_with(policies: &str, spec: GrantSpec) -> Env {
    let api = BusinessApi::new(
        vec![ApiCredential {
            id: "business-api".into(),
            secret: CANARY.into(),
        }],
        Ledger::in_memory(),
    );
    let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let mut registry = OperationRegistry::new();
    for op in operations(&api, &SupportConfig::new(running.base_url())).unwrap() {
        registry.register(Arc::new(op)).unwrap();
    }
    registry.register_delegate().unwrap();

    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials.json");
    std::fs::write(
        &creds,
        serde_json::json!({"credentials": [{
            "id": "business-api",
            "secret": CANARY,
            "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"]
        }]})
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
    let broker = FileCredentialBroker::open(&creds).unwrap();
    let broker_debug = format!("{broker:?}");

    let audit_path = dir.path().join("audit.jsonl");
    let sink = Arc::new(HookSink {
        inner: FileAuditSink::open(&audit_path).unwrap(),
        hook: Mutex::new(None),
    });
    let store = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(
        registry.action_ids(),
    )));
    let issued = store
        .issue(&AppId::new("support-app").unwrap(), spec, SystemClock.now())
        .await
        .unwrap();
    let guard = Guard::new(
        registry,
        store.clone(),
        Arc::new(CedarPdp::from_sources("Support", SCHEMA, policies).unwrap()),
        Arc::new(broker),
        sink.clone(),
        Arc::new(SystemClock),
    );
    Env {
        guard,
        api,
        sink,
        store,
        handle: issued.handle.expose_secret().to_owned(),
        audit_path,
        broker_debug,
        _running: running,
        _dir: dir,
    }
}

/// acme operator, limited to customer `acme-cus-000` and refunds up to
/// 50.00 (the delegator's own limit).
pub fn grant_spec() -> GrantSpec {
    let customer = EntityUid::new("Support::Customer", "acme-cus-000").unwrap();
    GrantSpec {
        tenant: TenantId::new("acme").unwrap(),
        delegator: Delegator::new(
            UserId::new("acme-agent-operator").unwrap(),
            "agent-operator",
            [("refund_limit_cents".to_owned(), AttrValue::Long(5_000))].into(),
        )
        .unwrap(),
        run_id: RunId::new("run-e2e").unwrap(),
        actions: [
            "customer.read",
            "invoice.list",
            "refund.issue",
            "customer.export",
        ]
        .into_iter()
        .map(|a| ActionId::new(a).unwrap())
        .collect(),
        constraints: Constraints::new(BTreeMap::from([
            ("customers".to_owned(), Constraint::Set([customer].into())),
            ("max_refund_cents".to_owned(), Constraint::AtMost(10_000)),
        ]))
        .unwrap(),
        ttl_seconds: 600,
        max_depth: 0,
    }
}

/// `grant_spec` plus `guard.delegate` and one level of delegation.
pub fn delegating_grant_spec() -> GrantSpec {
    let mut s = grant_spec();
    s.actions.insert(ActionId::new(DELEGATE_ACTION).unwrap());
    s.max_depth = 1;
    s
}

impl Env {
    pub async fn call(&self, tool: &str, args: Value) -> CallOutcome {
        self.call_as(&self.handle, tool, args).await
    }

    pub async fn call_as(&self, handle: &str, tool: &str, args: Value) -> CallOutcome {
        self.guard
            .call(handle, tool, args, CallMeta::default())
            .await
    }

    pub fn set_hook(&self, f: impl Fn(&AuditRecord) + Send + Sync + 'static) {
        *self.sink.hook.lock().unwrap() = Some(Box::new(f));
    }

    pub fn audit_text(&self) -> String {
        std::fs::read_to_string(&self.audit_path).unwrap()
    }

    /// Verified records of the hash-chained log.
    pub fn audit_records(&self) -> Vec<AuditRecord> {
        let mut out = Vec::new();
        verify_log_with(
            std::io::BufReader::new(std::fs::File::open(&self.audit_path).unwrap()),
            &[],
            |e: AuditEntry| out.push(e.record),
        )
        .unwrap();
        out
    }
}
