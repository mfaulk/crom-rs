//! The support application: the trusted side of the deployment. It opens a
//! grant per ticket, hands the session handle to the agent runtime (never to
//! the model), puts supervisor approval requests in front of a human,
//! delegates narrower authority to subagents, and revokes on ticket close.
//! It makes no authorization decision itself: Guard does.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crom_mcp::{McpClient, McpError, McpOptions, McpServerConfig, StreamableHttpMcpServerConfig};
use crom_sdk::{GuardApprovalRequest, GuardApprover};
use serde_json::{json, Value};

use crate::admin::{Admin, Issued, Secret};

/// Who answers Guard's supervisor approval requests.
pub enum Supervisor {
    /// Fixed answers, in order (the scripted demo); exhausted means decline.
    Scripted(Mutex<VecDeque<bool>>),
    /// Ask on the terminal.
    Prompt,
    Always(bool),
}

pub struct TrustedApp {
    pub admin: Arc<Admin>,
    agent_url: String,
    supervisor: Supervisor,
    /// What the supervisor was shown and decided, in order.
    approvals: Mutex<Vec<Value>>,
}

/// The supervisor identity the app approves as (acme tenant).
fn supervisor_identity() -> Value {
    json!({
        "id": "acme-supervisor",
        "role": "supervisor",
        "attrs": {"refund_limit_cents": {"long": 100_000}},
    })
}

impl TrustedApp {
    pub fn new(admin: Arc<Admin>, agent_url: String, supervisor: Supervisor) -> Arc<Self> {
        Arc::new(Self {
            admin,
            agent_url,
            supervisor,
            approvals: Mutex::new(Vec::new()),
        })
    }

    /// Opens a ticket: issues the grant in `spec_path` for run `run_id`.
    pub async fn open_ticket(&self, spec_path: &Path, run_id: &str) -> Result<Issued> {
        let text = std::fs::read_to_string(spec_path)
            .with_context(|| format!("reading {}", spec_path.display()))?;
        let mut spec: Value = serde_json::from_str(&text)?;
        spec["run_id"] = json!(run_id);
        self.admin.issue(&spec).await
    }

    /// Closes a ticket: the grant stops working for every later request.
    pub async fn close_ticket(&self, grant: &Issued) -> Result<()> {
        self.admin.revoke(&grant.grant_id).await
    }

    pub fn approvals(&self) -> Vec<Value> {
        self.approvals.lock().unwrap().clone()
    }

    pub fn approval_count(&self) -> usize {
        self.approvals.lock().unwrap().len()
    }

    async fn mcp(&self, handle: &Secret) -> Result<McpClient> {
        let server = McpServerConfig::StreamableHttp(StreamableHttpMcpServerConfig {
            url: self.agent_url.clone(),
            headers: [(
                "Authorization".to_string(),
                format!("Bearer {}", handle.expose()),
            )]
            .into(),
            startup_timeout: None,
            request_timeout: None,
            guard: false,
        });
        let secrets = vec![handle.expose().to_owned()];
        let mut client =
            McpClient::connect("guard", &server, McpOptions::default().limits(), secrets).await?;
        client.initialize().await?;
        Ok(client)
    }

    /// Guard's answer to `tools/list` for `handle`: `listed`, or its session
    /// error code (`grant_revoked`, `no_grant`, ...).
    pub async fn session_status(&self, handle: &Secret) -> Result<String> {
        let mut client = self.mcp(handle).await?;
        Ok(match client.list_tools().await {
            Ok(_) => "listed".into(),
            Err(McpError::JsonRpc { error, .. }) => error
                .data
                .as_ref()
                .and_then(|d| d["code"].as_str())
                .unwrap_or("rpc_error")
                .to_owned(),
            Err(_) => "unreachable".into(),
        })
    }

    /// Calls `guard.delegate` with the parent's handle. Returns Guard's code
    /// (`executed` or a deny code) and, if executed, the child's handle.
    pub async fn delegate(&self, parent: &Secret, args: Value) -> Result<(String, Option<Secret>)> {
        let mut client = self.mcp(parent).await?;
        let result = client.call_tool("guard.delegate", args).await?;
        let structured = result.structured_content.unwrap_or(Value::Null);
        if result.is_error {
            let code = structured["code"].as_str().unwrap_or("unknown");
            return Ok((code.to_owned(), None));
        }
        let child = structured["handle"].as_str().map(Secret::new);
        Ok(("executed".into(), child))
    }

    async fn decide(&self, pending: &Value) -> bool {
        match &self.supervisor {
            Supervisor::Scripted(answers) => answers.lock().unwrap().pop_front().unwrap_or(false),
            Supervisor::Always(answer) => *answer,
            Supervisor::Prompt => {
                let summary = format!(
                    "\nSupervisor approval requested: {} {} args={} (needs role {})\nApprove? [y/N] ",
                    pending["action"].as_str().unwrap_or("?"),
                    pending["resource"],
                    pending["args"],
                    pending["role"].as_str().unwrap_or("?"),
                );
                tokio::task::spawn_blocking(move || {
                    print!("{summary}");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    let _ = std::io::stdin().lock().read_line(&mut line);
                    matches!(line.trim(), "y" | "Y" | "yes")
                })
                .await
                .unwrap_or(false)
            }
        }
    }
}

#[async_trait]
impl GuardApprover for TrustedApp {
    async fn request_approval(&self, request: GuardApprovalRequest) -> Option<String> {
        // Show the human Guard's own record of the request, not the agent's.
        let pending = match self.admin.pending(&request.action_digest).await {
            Ok(p) => p,
            Err(e) => {
                eprintln!("support-app: cannot load approval request: {e}");
                return None;
            }
        };
        let approved = self.decide(&pending).await;
        self.approvals.lock().unwrap().push(json!({
            "action": pending["action"],
            "resource": pending["resource"],
            "args": pending["args"],
            "role": pending["role"],
            "decision": if approved { "approved" } else { "declined" },
        }));
        if !approved {
            return None;
        }
        let tenant = pending["tenant"].as_str().unwrap_or_default();
        match self
            .admin
            .approve(&request.action_digest, &supervisor_identity(), tenant)
            .await
        {
            Ok(id) => Some(id),
            Err(e) => {
                eprintln!("support-app: approval failed: {e}");
                None
            }
        }
    }
}
