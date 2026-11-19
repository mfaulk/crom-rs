//! In-process Guard for a trusted Rust host (ADR 0002 §2.1 library mode).
//! The Crom runtime must not embed Guard this way; agents reach Guard only
//! through `crom-guard-server`.
//!
//! Run: `cargo run -p crom-guard --example embedded`

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crom_guard::*;
use futures::executor::block_on;
use serde_json::{json, Value};

const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");

/// Application data: customer id → tenant. A real host reads its database.
fn customers() -> BTreeMap<&'static str, &'static str> {
    [("acme-cus-001", "acme"), ("globex-cus-001", "globex")].into()
}

struct Resolver;

impl ResourceResolver for Resolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        Box::pin(async move {
            let id = args.as_json()["customer_id"].as_str().unwrap_or_default();
            match customers().get(id) {
                Some(t) if *t == tenant.as_str() => {
                    let tenant = EntityUid::new("Support::Tenant", *t).expect("valid uid");
                    Ok(ResolvedResource::new(
                        EntityUid::new("Support::Customer", id)
                            .map_err(|_| ResolveError::NotFound)?,
                        ResourceVersion::new("1"),
                    )
                    .with_attr("tenant", AttrValue::Entity(tenant.clone()))
                    .with_parent(tenant))
                }
                _ => Err(ResolveError::NotFound),
            }
        })
    }
}

struct CustomerRead {
    action: ActionId,
    schema: Value,
}

impl ProtectedOperation for CustomerRead {
    fn action(&self) -> &ActionId {
        &self.action
    }
    fn description(&self) -> &str {
        "Read one customer record."
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn logged_fields(&self) -> &[&'static str] {
        &["customer_id"]
    }
    fn resolver(&self) -> &dyn ResourceResolver {
        &Resolver
    }
    fn context(&self, _: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        Ok(AuthorizationContext::default())
    }
    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        credential: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        Box::pin(async move {
            // A real executor sends the downstream request here, following
            // `ConditionalCall`, with `credential.with_secret(..)` as auth.
            let call = ConditionalCall::new(&action, "Support::Customer")?;
            Ok(ExecutionOutput {
                content: json!({
                    "customer_id": call.resource_id(),
                    "credential": credential.credential_id(),
                }),
                downstream_ref: None,
            })
        })
    }
}

struct StaticBroker;

impl CredentialBroker for StaticBroker {
    fn lease<'a>(&'a self, _: &'a ActionId) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async { Ok(CredentialLease::new("business-api", "example-secret")) })
    }
}

/// Use `FileAuditSink::open(path)` in production.
#[derive(Default)]
struct MemoryAudit(Mutex<Vec<AuditRecord>>);

impl AuditSink for MemoryAudit {
    fn append<'a>(&'a self, r: &'a AuditRecord) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
        Box::pin(async move {
            let mut v = self
                .0
                .lock()
                .map_err(|_| AuditError::Unavailable("lock".into()))?;
            v.push(r.clone());
            Ok(AuditReceipt {
                seq: v.len() as u64,
                head_hash: String::new(),
            })
        })
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut registry = OperationRegistry::new();
    registry.register(Arc::new(CustomerRead {
        action: ActionId::new("customer.read")?,
        schema: json!({
            "type": "object",
            "properties": {"customer_id": {"type": "string", "maxLength": 128}},
            "required": ["customer_id"],
            "additionalProperties": false
        }),
    }))?;
    let store = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(
        registry.action_ids(),
    )));
    let audit = Arc::new(MemoryAudit::default());
    let guard = Guard::new(
        registry,
        store.clone(),
        Arc::new(CedarPdp::from_sources("Support", SCHEMA, POLICIES)?),
        Arc::new(StaticBroker),
        audit.clone(),
        Arc::new(SystemClock),
    );

    // The trusted host authenticated the user and issues the agent a grant.
    let spec = GrantSpec {
        tenant: TenantId::new("acme")?,
        delegator: Delegator::new(
            UserId::new("acme-agent-operator")?,
            "agent-operator",
            [("refund_limit_cents".to_owned(), AttrValue::Long(0))].into(),
        )?,
        run_id: RunId::new("run-1")?,
        actions: [ActionId::new("customer.read")?].into(),
        constraints: Constraints::new(BTreeMap::from([
            (
                "customers".to_owned(),
                Constraint::Set([EntityUid::new("Support::Customer", "acme-cus-001")?].into()),
            ),
            ("max_refund_cents".to_owned(), Constraint::AtMost(0)),
        ]))?,
        ttl_seconds: 300,
        max_depth: 0,
    };
    let issued = block_on(store.issue(&AppId::new("host")?, spec, SystemClock.now()))?;
    let handle = issued.handle.expose_secret();

    for (tool, args) in [
        ("customer.read", json!({"customer_id": "acme-cus-001"})),
        ("customer.read", json!({"customer_id": "globex-cus-001"})),
        (
            "customer.read",
            json!({"customer_id": "acme-cus-001", "tenant_id": "globex"}),
        ),
        ("customer.export", json!({})),
    ] {
        let out = block_on(guard.call(handle, tool, args.clone(), CallMeta::default()));
        println!("{tool} {args} -> {}", serde_json::to_string(&out.result)?);
    }
    println!(
        "audit records: {}",
        audit.0.lock().map(|v| v.len()).unwrap_or(0)
    );
    Ok(())
}
