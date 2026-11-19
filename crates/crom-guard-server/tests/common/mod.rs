//! Shared fixture: a real `crom-guard-server` on ephemeral loopback ports
//! with three reference-shaped operations over in-memory data.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crom_guard::*;
use crom_guard_server::{AdminAddr, Config, ServerHandle, StartError};
use serde_json::{json, Value};
use tokio::sync::Semaphore;

pub const SCHEMA: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedarschema");
pub const POLICIES: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedar");
pub const ADMIN_TOKEN: &str = "adm-CANARY-7f3e9a1c5b2d4e6f8a0b1c2d3e4f5a6b";
pub const SECRET: &str = "sk-downstream-CANARY-4b8d";
/// Refunds of this amount block in `execute` until the gate releases them.
pub const HOLD_AMOUNT: i64 = 7;

/// Lets a test hold an executor mid-call.
pub struct Gate {
    pub entered: Semaphore,
    pub release: Semaphore,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        }
    }
}

fn uid(ty: &str, id: &str) -> EntityUid {
    EntityUid::new(format!("Support::{ty}"), id).unwrap()
}

fn data(id: &str) -> Option<(&'static str, ResolvedResource)> {
    let customer = |id: &str, t: &'static str| {
        (
            t,
            ResolvedResource::new(uid("Customer", id), ResourceVersion::new("1"))
                .with_attr("tenant", AttrValue::Entity(uid("Tenant", t)))
                .with_parent(uid("Tenant", t)),
        )
    };
    let invoice = |id: &str, t: &'static str, c: &str| {
        (
            t,
            ResolvedResource::new(uid("Invoice", id), ResourceVersion::new("1"))
                .with_attr("tenant", AttrValue::Entity(uid("Tenant", t)))
                .with_attr("customer", AttrValue::Entity(uid("Customer", c)))
                .with_attr("amount_cents", AttrValue::Long(20_000))
                .with_attr("refunded_cents", AttrValue::Long(0))
                .with_parent(uid("Customer", c)),
        )
    };
    Some(match id {
        "acme-cus-001" => customer(id, "acme"),
        "acme-cus-002" => customer(id, "acme"),
        "globex-cus-001" => customer(id, "globex"),
        "acme-inv-001" => invoice(id, "acme", "acme-cus-001"),
        _ => return None,
    })
}

struct Resolver(&'static str);

impl ResourceResolver for Resolver {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>> {
        Box::pin(async move {
            let id = args.as_json()[self.0].as_str().unwrap_or_default();
            match data(id) {
                Some((t, r)) if t == tenant.as_str() => Ok(r),
                _ => Err(ResolveError::NotFound),
            }
        })
    }
}

struct Op {
    action: ActionId,
    schema: Value,
    resolver: Resolver,
    gate: Arc<Gate>,
}

impl ProtectedOperation for Op {
    fn action(&self) -> &ActionId {
        &self.action
    }
    fn description(&self) -> &str {
        "reference test operation"
    }
    fn input_schema(&self) -> &Value {
        &self.schema
    }
    fn resolver(&self) -> &dyn ResourceResolver {
        &self.resolver
    }
    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason> {
        match args.as_json().get("amount_cents").and_then(Value::as_i64) {
            Some(a) => {
                AuthorizationContext::new([("amount_cents".to_owned(), AttrValue::Long(a))].into())
                    .map_err(|_| DenyReason::InvalidArguments)
            }
            None => Ok(AuthorizationContext::default()),
        }
    }
    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        credential: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
        Box::pin(async move {
            let amount = action.args().as_json().get("amount_cents").cloned();
            if amount == Some(json!(HOLD_AMOUNT)) {
                self.gate.entered.add_permits(1);
                self.gate.release.acquire().await.unwrap().forget();
            }
            if amount == Some(json!(13)) {
                // The detail must never reach the agent.
                return Err(ExecuteError::Failed(format!("downstream said {SECRET}")));
            }
            // Proves the lease was usable without returning it.
            let used = credential.with_secret(|s| !s.is_empty());
            Ok(ExecutionOutput {
                content: json!({
                    "resource": action.resource().id(),
                    "credential_id": credential.credential_id(),
                    "credential_used": used,
                }),
                downstream_ref: Some(format!("ref-{}", action.decision_id().as_str())),
            })
        })
    }
}

fn id_schema() -> Value {
    json!({"type": "string", "minLength": 1, "maxLength": 128, "pattern": "^[A-Za-z0-9._:@-]+$"})
}

pub fn registry(gate: &Arc<Gate>) -> OperationRegistry {
    let customer = json!({
        "type": "object",
        "properties": {"customer_id": id_schema()},
        "required": ["customer_id"],
        "additionalProperties": false
    });
    let refund = json!({
        "type": "object",
        "properties": {"invoice_id": id_schema(), "amount_cents": {"type": "integer", "minimum": 1}},
        "required": ["invoice_id", "amount_cents"],
        "additionalProperties": false
    });
    let mut reg = OperationRegistry::new();
    for (action, schema, field) in [
        ("customer.read", customer.clone(), "customer_id"),
        ("customer.export", customer, "customer_id"),
        ("refund.issue", refund, "invoice_id"),
    ] {
        reg.register(Arc::new(Op {
            action: ActionId::new(action).unwrap(),
            schema,
            resolver: Resolver(field),
            gate: gate.clone(),
        }))
        .unwrap();
    }
    reg
}

pub fn write_private(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

pub fn creds_json(id: &str, secret: &str) -> String {
    json!({"credentials": [{
        "id": id,
        "secret": secret,
        "actions": ["customer.read", "customer.export", "refund.issue"]
    }]})
    .to_string()
}

/// Files for a config in `dir`; returns the config text.
pub fn write_files(dir: &Path) -> String {
    std::fs::write(dir.join("support.cedarschema"), SCHEMA).unwrap();
    std::fs::write(dir.join("support.cedar"), POLICIES).unwrap();
    write_private(&dir.join("creds.json"), &creds_json("business-api", SECRET));
    write_private(&dir.join("admin.token"), &format!("{ADMIN_TOKEN}\n"));
    r#"
        [agent]
        listen = "127.0.0.1:0"
        [admin]
        listen = "127.0.0.1:0"
        token_file = "admin.token"
        app_id = "support-app"
        [policy]
        namespace = "Support"
        schema = "support.cedarschema"
        policies = "support.cedar"
        [credentials]
        file = "creds.json"
        [audit]
        path = "audit.jsonl"
        [limits]
        max_request_bytes = 131072
        request_read_timeout_ms = 1000
    "#
    .to_owned()
}

pub fn load(dir: &Path, text: &str) -> Config {
    let path = dir.join("guard.toml");
    std::fs::write(&path, text).unwrap();
    Config::load(&path).unwrap()
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub server: Option<ServerHandle>,
    pub gate: Arc<Gate>,
    pub http: reqwest::Client,
}

impl Fixture {
    pub async fn start() -> Self {
        Self::start_with(|_| {}).await
    }

    pub async fn start_with(edit: impl FnOnce(&mut Config)) -> Self {
        let (dir, gate, res) = Self::try_start(edit).await;
        Self {
            dir,
            server: Some(res.unwrap()),
            gate,
            http: reqwest::Client::new(),
        }
    }

    pub async fn try_start(
        edit: impl FnOnce(&mut Config),
    ) -> (
        tempfile::TempDir,
        Arc<Gate>,
        Result<ServerHandle, StartError>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let text = write_files(dir.path());
        let mut config = load(dir.path(), &text);
        edit(&mut config);
        let gate = Arc::new(Gate::default());
        let res = crom_guard_server::start(config, registry(&gate)).await;
        (dir, gate, res)
    }

    pub fn server(&self) -> &ServerHandle {
        self.server.as_ref().unwrap()
    }

    pub fn mcp_url(&self) -> String {
        format!("http://{}/mcp", self.server().agent_addr)
    }

    pub fn admin_url(&self, path: &str) -> String {
        match &self.server().admin_addr {
            AdminAddr::Tcp(a) => format!("http://{a}{path}"),
            AdminAddr::Unix(_) => panic!("unix admin socket in an HTTP helper"),
        }
    }

    pub fn audit_path(&self) -> PathBuf {
        self.dir.path().join("audit.jsonl")
    }

    pub async fn admin(&self, path: &str, token: &str, body: Option<Value>) -> (u16, Value) {
        let req = match body {
            Some(b) => self.http.post(self.admin_url(path)).json(&b),
            None if path.ends_with("/health") || path.ends_with("/head") => {
                self.http.get(self.admin_url(path))
            }
            None => self.http.post(self.admin_url(path)),
        };
        let resp = req.bearer_auth(token).send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    pub async fn admin_get(&self, path: &str, token: &str) -> (u16, Value) {
        let resp = self
            .http
            .get(self.admin_url(path))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// Issues a grant through the admin API; returns (grant id, handle).
    pub async fn issue(&self, actions: &[&str]) -> (String, String) {
        let (status, body) = self
            .admin("/admin/grants", ADMIN_TOKEN, Some(grant_spec(actions)))
            .await;
        assert_eq!(status, 201, "{body}");
        (
            body["grant"]["grant_id"].as_str().unwrap().to_owned(),
            body["handle"].as_str().unwrap().to_owned(),
        )
    }

    /// One raw JSON-RPC request; returns the parsed response body.
    pub async fn rpc(&self, bearer: Option<&str>, method: &str, params: Value) -> Value {
        let resp = self.rpc_raw(bearer, method, params).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    pub async fn rpc_raw(
        &self,
        bearer: Option<&str>,
        method: &str,
        params: Value,
    ) -> reqwest::Response {
        let mut req = self
            .http
            .post(self.mcp_url())
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        req.send().await.unwrap()
    }

    pub async fn call(&self, bearer: &str, tool: &str, args: Value) -> Value {
        let r = self
            .rpc(
                Some(bearer),
                "tools/call",
                json!({"name": tool, "arguments": args}),
            )
            .await;
        r["result"].clone()
    }

    pub async fn shutdown(&mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
    }
}

pub fn grant_spec(actions: &[&str]) -> Value {
    json!({
        "tenant": "acme",
        "delegator": {"id": "acme-agent-operator", "role": "agent-operator",
                      "attrs": {"refund_limit_cents": {"long": 5000}}},
        "run_id": "run-1",
        "actions": actions,
        "constraints": {
            "customers": {"set": [{"type": "Support::Customer", "id": "acme-cus-001"}]},
            "max_refund_cents": {"at_most": 10000}
        },
        "ttl_seconds": 600
    })
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": "2025-06-18",
        "capabilities": {},
        "clientInfo": {"name": "raw-jsonrpc", "version": "0"}
    })
}

pub fn code_of(result: &Value) -> &str {
    result["structuredContent"]["code"].as_str().unwrap_or("")
}

pub fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}
