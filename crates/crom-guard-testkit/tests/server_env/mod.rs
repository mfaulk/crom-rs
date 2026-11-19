//! The reference `crom-guard` server (`crom_guard_server::start` with the
//! support operations, a file broker, and a file audit log) as a
//! `ScenarioEnv`, driven by raw JSON-RPC over MCP Streamable HTTP. Same
//! deployment as the `guard_server` lane of `guard_adversarial.rs`.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crom_guard_server::{AdminAddr, Config, ServerHandle};
use crom_guard_testkit::scenario::{
    credentials_json, read_audit, AdminRequest, AuditView, CallRequest, Mode, Observed,
    ScenarioEnv, POLICIES, SCHEMA,
};
use crom_guard_testkit::{
    support_registry, ApiCredential, BusinessApi, Ledger, RunningApi, SupportConfig,
};
use serde_json::{json, Value};

const ADMIN_TOKEN: &str = "experiment-admin-token-0c5e9b2a7f14d3866a1e0b97c2d4f5e3a8";

pub struct HttpEnv {
    api: Arc<BusinessApi>,
    server: Option<ServerHandle>,
    http: reqwest::Client,
    secret: String,
    grants: Mutex<BTreeMap<String, String>>,
    dir: tempfile::TempDir,
    _running: RunningApi,
}

/// An agent-visible MCP response (`result` or `error`) as an `Observed`.
pub fn observe(text: String) -> Observed {
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(err) = body.get("error") {
        return Observed {
            code: err["data"]["code"]
                .as_str()
                .unwrap_or("rpc_error")
                .to_owned(),
            content: err.clone(),
            text,
            ..Observed::default()
        };
    }
    let result = &body["result"];
    let content = result["structuredContent"].clone();
    let code = if result["isError"] == true {
        match content["status"].as_str() {
            Some("approval_required") => "approval_required".to_owned(),
            _ => content["code"].as_str().unwrap_or("error").to_owned(),
        }
    } else {
        "executed".to_owned()
    };
    Observed {
        code,
        decision_id: result["_meta"]["decision_id"].as_str().map(str::to_owned),
        action_digest: content["action_digest"].as_str().map(str::to_owned),
        content,
        text,
        crom_block: None,
    }
}

fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

impl HttpEnv {
    pub async fn start() -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let secret = format!(
            "sk-business-CANARY-experiment-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let api = BusinessApi::new(
            vec![ApiCredential {
                id: "business-api".into(),
                secret: secret.clone(),
            }],
            Ledger::in_memory(),
        );
        let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let registry = support_registry(&api, &SupportConfig::new(running.base_url())).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        std::fs::write(d.join("support.cedarschema"), SCHEMA).unwrap();
        std::fs::write(d.join("support.cedar"), POLICIES).unwrap();
        write_private(&d.join("credentials.json"), &credentials_json(&secret));
        write_private(&d.join("admin.token"), &format!("{ADMIN_TOKEN}\n"));
        let config = r#"
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
            file = "credentials.json"
            [audit]
            path = "audit.jsonl"
            [limits]
            # The matrix and latency runs are mostly denials.
            deny_burst = 100000
        "#;
        std::fs::write(d.join("guard.toml"), config).unwrap();
        let config = Config::load(&d.join("guard.toml")).unwrap();
        let server = crom_guard_server::start(config, registry).await.unwrap();
        Self {
            api,
            server: Some(server),
            http: reqwest::Client::new(),
            secret,
            grants: Mutex::new(BTreeMap::new()),
            dir,
            _running: running,
        }
    }

    fn server(&self) -> &ServerHandle {
        self.server.as_ref().unwrap()
    }

    /// The agent endpoint, `http://<addr>/mcp`.
    pub fn mcp_url(&self) -> String {
        format!("http://{}/mcp", self.server().agent_addr)
    }

    fn admin_url(&self, path: &str) -> String {
        match &self.server().admin_addr {
            AdminAddr::Tcp(a) => format!("http://{a}{path}"),
            AdminAddr::Unix(_) => unreachable!("configured on TCP"),
        }
    }

    async fn admin(&self, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self
            .http
            .post(self.admin_url(path))
            .bearer_auth(ADMIN_TOKEN);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    pub async fn shutdown(mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
    }
}

impl ScenarioEnv for HttpEnv {
    fn mode(&self) -> Mode {
        Mode::GuardServer
    }

    fn api(&self) -> &Arc<BusinessApi> {
        &self.api
    }

    fn secret(&self) -> &str {
        &self.secret
    }

    async fn issue(&self, name: &str, spec: &Value) -> Result<String, String> {
        let (status, body) = self.admin("/admin/grants", Some(spec.clone())).await;
        if status != 201 {
            return Err(format!("{status}: {body}"));
        }
        self.grants.lock().unwrap().insert(
            name.to_owned(),
            body["grant"]["grant_id"].as_str().unwrap().to_owned(),
        );
        Ok(body["handle"].as_str().unwrap().to_owned())
    }

    async fn call(&self, req: CallRequest) -> Observed {
        if let Some(grant) = &req.revoke_after_decision {
            self.revoke(grant).await.unwrap();
        }
        let mut params = json!({"name": req.tool, "arguments": req.arguments});
        if let Some(a) = &req.approval_id {
            params["_meta"] = json!({"approval_id": a});
        }
        let mut http = self
            .http
            .post(self.mcp_url())
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params}));
        if !req.handle.is_empty() {
            http = http.bearer_auth(&req.handle);
        }
        observe(http.send().await.unwrap().text().await.unwrap())
    }

    async fn revoke(&self, grant: &str) -> Result<(), String> {
        let id = self
            .grants
            .lock()
            .unwrap()
            .get(grant)
            .cloned()
            .ok_or_else(|| format!("unknown grant `{grant}`"))?;
        match self
            .admin(&format!("/admin/grants/{id}/revoke"), None)
            .await
        {
            (200, _) => Ok(()),
            (s, b) => Err(format!("{s}: {b}")),
        }
    }

    async fn advance_clock(&self, seconds: u64) {
        tokio::time::sleep(Duration::from_secs(seconds)).await;
    }

    async fn approve(
        &self,
        digest: &str,
        approver: &Value,
        approver_tenant: &str,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        let body = json!({
            "action_digest": digest,
            "approver": approver,
            "approver_tenant": approver_tenant,
            "ttl_seconds": ttl_seconds,
        });
        match self.admin("/admin/approvals", Some(body)).await {
            (201, b) => Ok(b["approval_id"].as_str().unwrap().to_owned()),
            (s, b) => Err(format!("{s}: {b}")),
        }
    }

    async fn admin_request(&self, handle: &str, req: &AdminRequest) -> Option<u16> {
        let url = self.admin_url(&req.path);
        let mut r = match req.method.as_str() {
            "GET" => self.http.get(url),
            _ => self.http.post(url),
        };
        if let Some(b) = &req.body {
            r = r.json(b);
        }
        Some(r.bearer_auth(handle).send().await.ok()?.status().as_u16())
    }

    fn audit(&self) -> Option<AuditView> {
        Some(read_audit(&self.dir.path().join("audit.jsonl")))
    }
}
