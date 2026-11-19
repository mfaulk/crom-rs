//! Reference protected operations for the support deployment (MILESTONE_18
//! §14.1). Resolvers read the business API's data in process, scoped to the
//! principal's tenant; executors call its HTTP API under the version
//! condition documented on [`crom_guard::ConditionalCall`].

use std::sync::Arc;
use std::time::Duration;

use crom_guard::{
    ActionId, AttrValue, AuthorizationContext, AuthorizedAction, BoxFuture, ConditionalCall,
    CredentialLease, DenyReason, EntityUid, ExecuteError, ExecutionOutput, OperationRegistry,
    ProtectedOperation, RegistryError, ResolveError, ResolvedResource, ResourceResolver,
    ResourceVersion, TenantId, TimeoutResolver, ValidatedArgs,
};
use reqwest::header::IF_MATCH;
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::{BusinessApi, DECISION_ID_HEADER, RESOURCE_NOT_FOUND_HEADER};
use crate::data::{Customer, Invoice};

pub const TENANT_TYPE: &str = "Support::Tenant";
pub const CUSTOMER_TYPE: &str = "Support::Customer";
pub const INVOICE_TYPE: &str = "Support::Invoice";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportAction {
    CustomerRead,
    InvoiceList,
    RefundIssue,
    /// Registered so `no-bulk-export` can forbid it (G11). Its executor
    /// always fails closed without a downstream call.
    CustomerExport,
}

impl SupportAction {
    pub const ALL: [Self; 4] = [
        Self::CustomerRead,
        Self::InvoiceList,
        Self::RefundIssue,
        Self::CustomerExport,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::CustomerRead => "customer.read",
            Self::InvoiceList => "invoice.list",
            Self::RefundIssue => "refund.issue",
            Self::CustomerExport => "customer.export",
        }
    }

    /// The Cedar resource type, per `support.cedarschema`.
    pub fn entity_type(self) -> &'static str {
        match self {
            Self::RefundIssue => INVOICE_TYPE,
            _ => CUSTOMER_TYPE,
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::CustomerRead => "Read one customer record.",
            Self::InvoiceList => "List a customer's invoices.",
            Self::RefundIssue => "Refund part or all of an invoice.",
            Self::CustomerExport => "Export customers. Always fails: not executable in v1.",
        }
    }

    fn logged_fields(self) -> &'static [&'static str] {
        match self {
            Self::RefundIssue => &["invoice_id", "amount_cents"],
            _ => &["customer_id"],
        }
    }

    /// Identifier fields only (plus `amount_cents` for refunds). There is
    /// no tenant field, and `additionalProperties: false` rejects one.
    pub fn input_schema(self) -> Value {
        let id = json!({
            "type": "string",
            "minLength": 1,
            "maxLength": 128,
            "pattern": "^[A-Za-z0-9._:@-]+$"
        });
        match self {
            Self::RefundIssue => json!({
                "type": "object",
                "properties": {
                    "invoice_id": id,
                    "amount_cents": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": i64::MAX
                    }
                },
                "required": ["invoice_id", "amount_cents"],
                "additionalProperties": false
            }),
            _ => json!({
                "type": "object",
                "properties": { "customer_id": id },
                "required": ["customer_id"],
                "additionalProperties": false
            }),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CustomerArgs {
    customer_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefundArgs {
    invoice_id: String,
    amount_cents: u64,
}

fn parse<T: DeserializeOwned>(v: &Value) -> Option<T> {
    T::deserialize(v).ok()
}

#[derive(Debug, Clone)]
pub struct SupportConfig {
    /// Business API base URL, e.g. `RunningApi::base_url()`.
    pub base_url: String,
    /// Resolution slower than this is denied (`engine_unavailable`).
    pub resolve_timeout: Duration,
    pub request_timeout: Duration,
}

impl SupportConfig {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            resolve_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SupportOpsError {
    #[error("base url {0:?} is not an http(s) url")]
    BaseUrl(String),
    #[error("http client: {0}")]
    Client(String),
    #[error("registry: {0}")]
    Registry(#[from] RegistryError),
}

/// The reference deployment's registry: the four support operations and
/// `guard.delegate`. Used by the `crom-guard-support` example binary.
pub fn support_registry(
    api: &Arc<BusinessApi>,
    config: &SupportConfig,
) -> Result<OperationRegistry, SupportOpsError> {
    let mut registry = OperationRegistry::new();
    for op in operations(api, config)? {
        registry.register(Arc::new(op))?;
    }
    registry.register_delegate()?;
    Ok(registry)
}

/// All four support operations.
pub fn operations(
    api: &Arc<BusinessApi>,
    config: &SupportConfig,
) -> Result<Vec<SupportOperation>, SupportOpsError> {
    SupportAction::ALL
        .into_iter()
        .map(|kind| SupportOperation::new(kind, api.clone(), config))
        .collect()
}

pub struct SupportOperation {
    kind: SupportAction,
    action: ActionId,
    description: Option<String>,
    schema: Value,
    resolver: TimeoutResolver<SupportResolver>,
    downstream: Downstream,
}

impl std::fmt::Debug for SupportOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportOperation")
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

impl SupportOperation {
    pub fn new(
        kind: SupportAction,
        api: Arc<BusinessApi>,
        config: &SupportConfig,
    ) -> Result<Self, SupportOpsError> {
        let timeout = config.resolve_timeout;
        Ok(Self {
            kind,
            action: ActionId::new(kind.id()).expect("static action ids are valid"),
            description: None,
            schema: kind.input_schema(),
            resolver: TimeoutResolver::new(SupportResolver { kind, api }, move || {
                Box::pin(tokio::time::sleep(timeout))
            }),
            downstream: Downstream::new(config)?,
        })
    }

    pub fn kind(&self) -> SupportAction {
        self.kind
    }

    /// Advertises `description` instead of the built-in one. Descriptions
    /// are informational to Guard; adversarial scenario G17 serves hostile
    /// text this way.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

impl ProtectedOperation for SupportOperation {
    fn action(&self) -> &ActionId {
        &self.action
    }

    fn description(&self) -> &str {
        self.description
            .as_deref()
            .unwrap_or_else(|| self.kind.description())
    }

    fn input_schema(&self) -> &Value {
        &self.schema
    }

    fn logged_fields(&self) -> &[&'static str] {
        self.kind.logged_fields()
    }

    fn resolver(&self) -> &dyn ResourceResolver {
        &self.resolver
    }

    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        context_for(self.kind, args.as_json())
    }

    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        credential: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        Box::pin(async move {
            // The schema types `customer.export` on one Customer, but the
            // export is tenant-wide: a permit on that customer would leak
            // every customer in the tenant. A real export needs a
            // Tenant-typed resource (a schema change, out of scope for v1),
            // so this executor never calls downstream.
            if self.kind == SupportAction::CustomerExport {
                return Err(export_not_executable());
            }
            if action.action() != &self.action {
                return Err(ExecuteError::Failed(format!(
                    "{} executor given a {} action",
                    self.action,
                    action.action()
                )));
            }
            let call = ConditionalCall::new(&action, self.kind.entity_type())?;
            let target = Target {
                tenant: call.tenant().as_str(),
                id: call.resource_id(),
                if_match: call.if_match(),
                decision_id: call.decision_id().as_str(),
            };
            self.downstream
                .send(self.kind, &target, call.args().as_json(), &credential)
                .await
        })
    }
}

fn export_not_executable() -> ExecuteError {
    ExecuteError::Failed(
        "customer.export is not executable: it needs a Tenant-typed resource".into(),
    )
}

fn context_for(kind: SupportAction, args: &Value) -> Result<AuthorizationContext, DenyReason> {
    match kind {
        SupportAction::RefundIssue => {
            let a: RefundArgs = parse(args).ok_or(DenyReason::InvalidArguments)?;
            let amount = i64::try_from(a.amount_cents).map_err(|_| DenyReason::InvalidArguments)?;
            AuthorizationContext::new([("amount_cents".to_owned(), AttrValue::Long(amount))].into())
                .map_err(|_| DenyReason::InvalidArguments)
        }
        _ => Ok(AuthorizationContext::default()),
    }
}

/// Looks the identifier up in the business API's data for the principal's
/// tenant. Attributes and version come from the stored record; of the
/// arguments, only the identifier is read.
pub struct SupportResolver {
    kind: SupportAction,
    api: Arc<BusinessApi>,
}

impl std::fmt::Debug for SupportResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupportResolver")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl SupportResolver {
    fn lookup(&self, tenant: &TenantId, args: &Value) -> Result<ResolvedResource, ResolveError> {
        let t = tenant.as_str();
        // Unparseable arguments cannot pass the schema; deny the same way
        // as a missing id if they ever do.
        match self.kind {
            SupportAction::RefundIssue => {
                let a: RefundArgs = parse(args).ok_or(ResolveError::NotFound)?;
                let inv = self
                    .api
                    .lookup_invoice(t, &a.invoice_id)
                    .ok_or(ResolveError::NotFound)?;
                invoice_resource(&inv)
            }
            _ => {
                let a: CustomerArgs = parse(args).ok_or(ResolveError::NotFound)?;
                let c = self
                    .api
                    .lookup_customer(t, &a.customer_id)
                    .ok_or(ResolveError::NotFound)?;
                customer_resource(&c)
            }
        }
    }
}

impl SupportResolver {
    /// The lookup takes the business API's lock, so it runs on the blocking
    /// pool when a Tokio runtime is present; either way it runs only when
    /// the future is polled, so `TimeoutResolver` can bound it. A panicked
    /// lookup is `Unavailable`.
    fn resolve_owned(
        &self,
        tenant: TenantId,
        args: Value,
    ) -> BoxFuture<'static, Result<ResolvedResource, ResolveError>> {
        let this = Self {
            kind: self.kind,
            api: self.api.clone(),
        };
        Box::pin(async move {
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => rt
                    .spawn_blocking(move || this.lookup(&tenant, &args))
                    .await
                    .unwrap_or(Err(ResolveError::Unavailable)),
                Err(_) => this.lookup(&tenant, &args),
            }
        })
    }
}

impl ResourceResolver for SupportResolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        self.resolve_owned(tenant.clone(), args.as_json().clone())
    }
}

/// Stored data that cannot be expressed as a Cedar entity fails closed.
fn entity(ty: &str, id: &str) -> Result<EntityUid, ResolveError> {
    EntityUid::new(ty, id).map_err(|_| ResolveError::Unavailable)
}

fn long(v: u64) -> Result<AttrValue, ResolveError> {
    i64::try_from(v)
        .map(AttrValue::Long)
        .map_err(|_| ResolveError::Unavailable)
}

fn customer_resource(c: &Customer) -> Result<ResolvedResource, ResolveError> {
    let tenant = entity(TENANT_TYPE, &c.tenant)?;
    Ok(ResolvedResource::new(
        entity(CUSTOMER_TYPE, &c.id)?,
        ResourceVersion::new(c.version.to_string()),
    )
    .with_attr("tenant", AttrValue::Entity(tenant.clone()))
    .with_parent(tenant))
}

fn invoice_resource(i: &Invoice) -> Result<ResolvedResource, ResolveError> {
    let customer = entity(CUSTOMER_TYPE, &i.customer_id)?;
    Ok(ResolvedResource::new(
        entity(INVOICE_TYPE, &i.id)?,
        ResourceVersion::new(i.version.to_string()),
    )
    .with_attr("tenant", AttrValue::Entity(entity(TENANT_TYPE, &i.tenant)?))
    .with_attr("customer", AttrValue::Entity(customer.clone()))
    .with_attr("amount_cents", long(i.amount_cents)?)
    .with_attr("refunded_cents", long(i.refunded_cents)?)
    .with_parent(customer))
}

/// What [`ConditionalCall`] yields, as plain values.
struct Target<'a> {
    tenant: &'a str,
    id: &'a str,
    if_match: &'a str,
    decision_id: &'a str,
}

struct Downstream {
    client: reqwest::Client,
    base: Url,
}

impl Downstream {
    fn new(config: &SupportConfig) -> Result<Self, SupportOpsError> {
        let base = Url::parse(&config.base_url)
            .ok()
            .filter(|u| matches!(u.scheme(), "http" | "https") && !u.cannot_be_a_base())
            .ok_or_else(|| SupportOpsError::BaseUrl(config.base_url.clone()))?;
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            // A redirect would carry the bearer credential elsewhere.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SupportOpsError::Client(e.to_string()))?;
        Ok(Self { client, base })
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("checked in Downstream::new")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn send(
        &self,
        kind: SupportAction,
        t: &Target<'_>,
        args: &Value,
        credential: &CredentialLease,
    ) -> Result<ExecutionOutput, ExecuteError> {
        let (method, url, body) = match kind {
            SupportAction::CustomerRead => (
                Method::GET,
                self.url(&["tenants", t.tenant, "customers", t.id]),
                None,
            ),
            SupportAction::InvoiceList => (
                Method::GET,
                self.url(&["tenants", t.tenant, "customers", t.id, "invoices"]),
                None,
            ),
            SupportAction::RefundIssue => {
                // The invoice is the bound resource; only the amount comes
                // from the arguments.
                let a: RefundArgs =
                    parse(args).ok_or_else(|| ExecuteError::Failed("refund arguments".into()))?;
                (
                    Method::POST,
                    self.url(&["tenants", t.tenant, "invoices", t.id, "refunds"]),
                    Some(json!({ "amount_cents": a.amount_cents })),
                )
            }
            // Also refused here, so no path reaches the export endpoint.
            SupportAction::CustomerExport => return Err(export_not_executable()),
        };
        let mut req = self
            .client
            .request(method, url)
            .header(DECISION_ID_HEADER, t.decision_id);
        req = req.header(IF_MATCH, t.if_match);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let req = credential.with_secret(|s| req.bearer_auth(s));
        let resp = req.send().await.map_err(|e| {
            ExecuteError::Failed(format!("downstream request: {}", e.without_url()))
        })?;
        let missing = resp.headers().get(RESOURCE_NOT_FOUND_HEADER).is_some();
        ConditionalCall::check_status(resp.status().as_u16(), missing)?;
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ExecuteError::Failed(format!("downstream body: {}", e.without_url())))?;
        Ok(match kind {
            SupportAction::CustomerRead => ExecutionOutput {
                content: body,
                downstream_ref: None,
            },
            SupportAction::InvoiceList => ExecutionOutput {
                content: json!({ "invoices": body }),
                downstream_ref: None,
            },
            SupportAction::RefundIssue => ExecutionOutput {
                downstream_ref: body["refund_id"].as_str().map(str::to_owned),
                content: body,
            },
            SupportAction::CustomerExport => return Err(export_not_executable()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiCredential, RunningApi};
    use crate::ledger::{Effect, Ledger};

    const SECRET: &str = "guard-secret-canary";

    async fn start() -> (Arc<BusinessApi>, RunningApi, Vec<SupportOperation>) {
        let api = BusinessApi::new(
            vec![ApiCredential {
                id: "guard".into(),
                secret: SECRET.into(),
            }],
            Ledger::in_memory(),
        );
        let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let ops = operations(&api, &SupportConfig::new(running.base_url())).unwrap();
        (api, running, ops)
    }

    fn op(ops: &[SupportOperation], kind: SupportAction) -> &SupportOperation {
        ops.iter().find(|o| o.kind == kind).unwrap()
    }

    fn acme() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn lease() -> CredentialLease {
        CredentialLease::new("guard", SECRET)
    }

    /// Mirrors what `ConditionalCall` derives from the `AuthorizedAction` bound to `r`.
    async fn execute_bound(
        o: &SupportOperation,
        r: &ResolvedResource,
        args: &Value,
        credential: &CredentialLease,
    ) -> Result<ExecutionOutput, ExecuteError> {
        let if_match = format!("\"{}\"", r.version().as_str());
        let t = Target {
            tenant: "acme",
            id: r.uid().id(),
            if_match: &if_match,
            decision_id: "dec-1",
        };
        o.downstream.send(o.kind, &t, args, credential).await
    }

    /// The reference registry holds the four support operations plus `guard.delegate`.
    #[tokio::test]
    async fn support_registry_registers_support_ops_and_delegate() {
        let (api, running, _) = start().await;
        let registry = support_registry(&api, &SupportConfig::new(running.base_url())).unwrap();
        let ids: Vec<String> = registry
            .action_ids()
            .into_iter()
            .map(|a| a.as_str().to_owned())
            .collect();
        assert_eq!(
            ids,
            [
                "customer.export",
                "customer.read",
                "guard.delegate",
                "invoice.list",
                "refund.issue"
            ]
        );
    }

    /// Every schema forbids extra properties, declares no tenant field, and rejects a forged `tenant_id`.
    #[test]
    fn schemas_reject_forged_tenant_id() {
        for kind in SupportAction::ALL {
            let schema = kind.input_schema();
            assert_eq!(schema["additionalProperties"], false, "{kind:?}");
            let props = schema["properties"].as_object().unwrap();
            assert!(props.keys().all(|k| !k.contains("tenant")), "{kind:?}");
            let v = jsonschema::validator_for(&schema).unwrap();
            let mut ok = match kind {
                SupportAction::RefundIssue => {
                    json!({"invoice_id": "acme-inv-000", "amount_cents": 100})
                }
                _ => json!({"customer_id": "acme-cus-000"}),
            };
            assert!(v.is_valid(&ok), "{kind:?}");
            ok["tenant_id"] = "globex".into();
            assert!(!v.is_valid(&ok), "{kind:?}");
        }
    }

    /// Refund schemas reject non-positive amounts and ids outside the identifier charset.
    #[test]
    fn refund_schema_bounds_amount_and_id() {
        let v = jsonschema::validator_for(&SupportAction::RefundIssue.input_schema()).unwrap();
        for bad in [
            json!({"invoice_id": "acme-inv-000", "amount_cents": 0}),
            json!({"invoice_id": "acme-inv-000", "amount_cents": -5}),
            json!({"invoice_id": "acme-inv-000", "amount_cents": 1.5}),
            json!({"invoice_id": "../globex-inv-000", "amount_cents": 1}),
            json!({"invoice_id": "acme-inv-000"}),
        ] {
            assert!(!v.is_valid(&bad), "{bad}");
        }
    }

    /// The refund context carries `amount_cents` as a Cedar long; customer operations add nothing.
    #[test]
    fn context_carries_only_the_amount() {
        let c = context_for(
            SupportAction::RefundIssue,
            &json!({"invoice_id": "acme-inv-000", "amount_cents": 250}),
        )
        .unwrap();
        assert_eq!(
            c.fields(),
            &[("amount_cents".to_owned(), AttrValue::Long(250))].into()
        );
        let c = context_for(
            SupportAction::CustomerRead,
            &json!({"customer_id": "acme-cus-000"}),
        )
        .unwrap();
        assert!(c.fields().is_empty());
    }

    /// Resolvers take attributes, parents, and version from stored data in the principal's tenant.
    #[tokio::test]
    async fn resolvers_use_trusted_data() {
        let (api, _running, ops) = start().await;
        let r = op(&ops, SupportAction::CustomerRead)
            .resolver
            .inner()
            .lookup(&acme(), &json!({"customer_id": "acme-cus-001"}))
            .unwrap();
        let tenant = EntityUid::new(TENANT_TYPE, "acme").unwrap();
        assert_eq!(
            r.uid(),
            &EntityUid::new(CUSTOMER_TYPE, "acme-cus-001").unwrap()
        );
        assert_eq!(r.attrs()["tenant"], AttrValue::Entity(tenant.clone()));
        assert!(r.parents().contains(&tenant));
        assert_eq!(r.version().as_str(), "1");

        api.update_invoice("acme", "acme-inv-003", |i| i.refunded_cents = 40);
        let r = op(&ops, SupportAction::RefundIssue)
            .resolver
            .inner()
            .lookup(
                &acme(),
                &json!({"invoice_id": "acme-inv-003", "amount_cents": 1}),
            )
            .unwrap();
        let inv = api.lookup_invoice("acme", "acme-inv-003").unwrap();
        let customer = EntityUid::new(CUSTOMER_TYPE, &inv.customer_id).unwrap();
        assert_eq!(
            r.uid(),
            &EntityUid::new(INVOICE_TYPE, "acme-inv-003").unwrap()
        );
        assert_eq!(r.attrs()["customer"], AttrValue::Entity(customer.clone()));
        assert_eq!(
            r.attrs()["amount_cents"],
            AttrValue::Long(inv.amount_cents as i64)
        );
        assert_eq!(r.attrs()["refunded_cents"], AttrValue::Long(40));
        assert_eq!(r.attrs()["tenant"], AttrValue::Entity(tenant));
        assert!(r.parents().contains(&customer));
        assert_eq!(r.version().as_str(), "2");
        assert!(
            api.ledger().entries().is_empty(),
            "resolution is unledgered"
        );
    }

    /// An id from tenant B resolves to the same not-found denial as a missing id, for every operation.
    #[tokio::test]
    async fn other_tenant_id_is_not_found() {
        let (_api, _running, ops) = start().await;
        for o in &ops {
            let (other, missing) = match o.kind {
                SupportAction::RefundIssue => (
                    json!({"invoice_id": "globex-inv-000", "amount_cents": 1}),
                    json!({"invoice_id": "acme-inv-999", "amount_cents": 1}),
                ),
                _ => (
                    json!({"customer_id": "globex-cus-000"}),
                    json!({"customer_id": "acme-cus-999"}),
                ),
            };
            let r = o.resolver.inner();
            assert_eq!(r.lookup(&acme(), &other), Err(ResolveError::NotFound));
            assert_eq!(r.lookup(&acme(), &missing), Err(ResolveError::NotFound));
            assert_eq!(
                DenyReason::from(ResolveError::NotFound).code(),
                "resource_not_found"
            );
        }
    }

    /// An authorized refund executes once on exactly the bound invoice, tagged with the decision and credential ids.
    #[tokio::test]
    async fn refund_executes_on_the_bound_invoice() {
        let (api, _running, ops) = start().await;
        let o = op(&ops, SupportAction::RefundIssue);
        let args = json!({"invoice_id": "acme-inv-001", "amount_cents": 100});
        let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
        let out = execute_bound(o, &r, &args, &lease()).await.unwrap();
        assert_eq!(out.downstream_ref.as_deref(), Some("acme-ref-0000"));
        assert_eq!(out.content["invoice"]["refunded_cents"], 100);

        let entries = api.ledger().entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].guard_decision_id.as_deref(), Some("dec-1"));
        assert_eq!(entries[0].credential_id, "guard");
        assert!(matches!(
            &entries[0].effect,
            Effect::Refund { invoice_id, amount_cents: 100, .. } if invoice_id == "acme-inv-001"
        ));
    }

    /// An ownership change between authorize and execute yields `Precondition` and no ledger effect.
    #[tokio::test]
    async fn ownership_change_before_execute_has_no_effect() {
        let (api, _running, ops) = start().await;
        let o = op(&ops, SupportAction::RefundIssue);
        let args = json!({"invoice_id": "acme-inv-001", "amount_cents": 100});
        let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
        assert!(api.update_invoice("acme", "acme-inv-001", |i| {
            i.customer_id = "acme-cus-005".into()
        }));
        assert_eq!(
            execute_bound(o, &r, &args, &lease()).await,
            Err(ExecuteError::Precondition)
        );
        assert!(api.ledger().entries().is_empty());
        assert_eq!(
            api.lookup_invoice("acme", "acme-inv-001")
                .unwrap()
                .refunded_cents,
            0
        );
    }

    /// A resource moved to another tenant between authorize and execute yields `Precondition` and no ledger effect.
    #[tokio::test]
    async fn tenant_move_before_execute_has_no_effect() {
        let (api, _running, ops) = start().await;
        for kind in [SupportAction::RefundIssue, SupportAction::CustomerRead] {
            api.reset().unwrap();
            let o = op(&ops, kind);
            let args = match kind {
                SupportAction::RefundIssue => {
                    json!({"invoice_id": "acme-inv-002", "amount_cents": 100})
                }
                _ => json!({"customer_id": "acme-cus-002"}),
            };
            let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
            let id = r.uid().id().to_owned();
            let moved = match kind {
                SupportAction::RefundIssue => {
                    api.update_invoice("acme", &id, |i| i.tenant = "globex".into())
                }
                _ => api.update_customer("acme", &id, |c| c.tenant = "globex".into()),
            };
            assert!(moved);
            assert_eq!(
                execute_bound(o, &r, &args, &lease()).await,
                Err(ExecuteError::Precondition),
                "{kind:?}"
            );
            assert!(api.ledger().entries().is_empty(), "{kind:?}");
        }
    }

    /// Reads are version-conditioned too: a changed customer is a precondition failure with no ledger read.
    #[tokio::test]
    async fn reads_are_version_conditioned() {
        let (api, _running, ops) = start().await;
        for kind in [SupportAction::CustomerRead, SupportAction::InvoiceList] {
            api.reset().unwrap();
            let o = op(&ops, kind);
            let args = json!({"customer_id": "acme-cus-003"});
            let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
            let out = execute_bound(o, &r, &args, &lease()).await.unwrap();
            assert_eq!(api.ledger().entries().len(), 1, "{kind:?}");
            match kind {
                SupportAction::CustomerRead => assert_eq!(out.content["id"], "acme-cus-003"),
                _ => assert_eq!(out.content["invoices"].as_array().unwrap().len(), 2),
            }

            api.update_customer("acme", "acme-cus-003", |_| {});
            assert_eq!(
                execute_bound(o, &r, &args, &lease()).await,
                Err(ExecuteError::Precondition),
                "{kind:?}"
            );
            assert_eq!(api.ledger().entries().len(), 1, "{kind:?}");
        }
    }

    /// Export fails closed without any downstream call, so it cannot leak the tenant even if a policy permitted it.
    #[tokio::test]
    async fn export_fails_closed_without_effect() {
        let (api, _running, ops) = start().await;
        let o = op(&ops, SupportAction::CustomerExport);
        let args = json!({"customer_id": "acme-cus-000"});
        let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
        assert!(matches!(
            execute_bound(o, &r, &args, &lease()).await,
            Err(ExecuteError::Failed(_))
        ));
        assert!(api.ledger().entries().is_empty());
    }

    /// A 404 the API does not mark as resource-not-found (here, a wrong route) is a plain failure, not a precondition.
    #[tokio::test]
    async fn unmarked_404_is_failed_not_precondition() {
        let (api, running, _ops) = start().await;
        let o = SupportOperation::new(
            SupportAction::RefundIssue,
            api.clone(),
            &SupportConfig::new(format!("{}/no-such-prefix", running.base_url())),
        )
        .unwrap();
        let args = json!({"invoice_id": "acme-inv-001", "amount_cents": 100});
        let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
        assert!(matches!(
            execute_bound(&o, &r, &args, &lease()).await,
            Err(ExecuteError::Failed(_))
        ));
        assert!(api.ledger().entries().is_empty());
    }

    /// Resolution happens when the future is polled, not when `resolve` returns it, so the timeout can bound it.
    #[tokio::test]
    async fn resolution_runs_inside_the_future() {
        let (api, _running, ops) = start().await;
        let r = op(&ops, SupportAction::CustomerRead).resolver.inner();
        let fut = r.resolve_owned(acme(), json!({"customer_id": "acme-cus-004"}));
        api.update_customer("acme", "acme-cus-004", |_| {});
        assert_eq!(fut.await.unwrap().version().as_str(), "2");
    }

    /// A credential the API does not accept fails the call without any effect.
    #[tokio::test]
    async fn wrong_credential_fails_without_effect() {
        let (api, _running, ops) = start().await;
        let o = op(&ops, SupportAction::RefundIssue);
        let args = json!({"invoice_id": "acme-inv-001", "amount_cents": 100});
        let r = o.resolver.inner().lookup(&acme(), &args).unwrap();
        let bad = CredentialLease::new("guard", "wrong");
        assert!(matches!(
            execute_bound(o, &r, &args, &bad).await,
            Err(ExecuteError::Failed(_))
        ));
        assert!(api.ledger().entries().is_empty());
    }

    /// Non-http base URLs are rejected when the operations are built.
    #[test]
    fn bad_base_url_rejected() {
        for bad in ["not a url", "mailto:a@b", "file:///tmp"] {
            assert!(matches!(
                Downstream::new(&SupportConfig::new(bad)),
                Err(SupportOpsError::BaseUrl(_))
            ));
        }
    }
}
