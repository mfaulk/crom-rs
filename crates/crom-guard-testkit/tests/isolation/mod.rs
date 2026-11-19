//! Shared harness for the OS-isolation attacks (#360), used by
//! `tests/bypass.rs` and the G13/G14/G17/G18 lanes of
//! `tests/guard_adversarial.rs`. Guard and the business API run in a child
//! process (the test binary re-executed through its `guard_host_process`
//! test) with a 0700 state directory; the agent's Bash runs through Crom's
//! `BashTool` or as another uid via `sudo -n`.
//!
//! Every attack script prints `RAN` first and `DENIED` when the OS refuses
//! the step, so a lane cannot pass because the script never ran.
#![allow(dead_code)]

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use crom_core::{SandboxConfig, SessionId, Tool, ToolContext};
use crom_guard::AuditReceipt;
use crom_guard_testkit::{
    support_registry, ApiCredential, BusinessApi, Ledger, LedgerEntry, SupportConfig,
};
use crom_tools::{BashTool, ToolLimits};
use serde_json::{json, Value};

pub const STATE_ENV: &str = "CROM_BYPASS_GUARD_STATE";
pub const ORACLE_ENV: &str = "CROM_BYPASS_ORACLE";
pub const TOKEN_VAR: &str = "CROM_BYPASS_ADMIN_TOKEN";
pub const SCHEMA: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedarschema");
pub const POLICIES: &str =
    include_str!("../../../../examples/guard-support/policies/support.cedar");
pub const GRANT: &str =
    include_str!("../../../../examples/guard-support/grants/acme-agent-operator.json");
pub const CROM_SETTINGS: &str =
    include_str!("../../../../examples/guard-support/agent/crom-settings.json");
pub const REQUIRE_SANDBOX: &str = "CROM_REQUIRE_SANDBOX_TESTS";
pub const REQUIRE_SEPARATE_UID: &str = "CROM_REQUIRE_SEPARATE_UID_TESTS";

/// Defines the `guard_host_process` test that `Deployment::start` re-executes.
macro_rules! guard_host_test {
    () => {
        /// Entry point of the Guard child process that `Deployment::start` spawns; a no-op when run as a test.
        #[test]
        fn guard_host_process() {
            isolation::host_main();
        }
    };
}
pub(crate) use guard_host_test;

/// Runs Guard and the business API until stdin closes, when the state and
/// oracle variables are set.
pub fn host_main() {
    let (Some(state), Some(oracle)) = (std::env::var_os(STATE_ENV), std::env::var_os(ORACLE_ENV))
    else {
        return;
    };
    let (state, oracle) = (PathBuf::from(state), PathBuf::from(oracle));
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let text = std::fs::read_to_string(state.join("credentials.json")).unwrap();
        let api = BusinessApi::new(
            ApiCredential::from_broker_json(&text).unwrap(),
            Ledger::with_file(&oracle.join("ledger.jsonl")).unwrap(),
        );
        let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let registry = support_registry(&api, &SupportConfig::new(running.base_url())).unwrap();
        let config = crom_guard_server::Config::load(&state.join("guard.toml")).unwrap();
        let server = crom_guard_server::start(config, registry).await.unwrap();
        println!("\nREADY {} {}", server.agent_addr, running.base_url());
        std::io::stdout().flush().unwrap();
        let _ = tokio::task::spawn_blocking(|| std::io::stdin().read_to_end(&mut Vec::new())).await;
        std::process::exit(0);
    });
}

/// Prints `SKIP:` or, when `env` is set, panics.
pub fn required_or_skip(what: &str, env: &str) {
    if std::env::var_os(env).is_some() {
        panic!("{what}, and {env} is set");
    }
    eprintln!("SKIP: {what}");
}

/// Why the sandbox lanes cannot run here, if they cannot.
pub fn sandbox_unavailable() -> Option<String> {
    if let Err(reason) = crom_tools::sandbox::probe(&SandboxConfig::required()) {
        return Some(format!("OS sandbox unavailable: {reason}"));
    }
    if !Path::new("/usr/bin/curl").exists() {
        return Some("/usr/bin/curl not found".into());
    }
    None
}

/// True when the sandbox lanes can run; otherwise skips (or panics under
/// `CROM_REQUIRE_SANDBOX_TESTS`).
pub fn sandbox_available() -> bool {
    match sandbox_unavailable() {
        None => true,
        Some(why) => {
            required_or_skip(&why, REQUIRE_SANDBOX);
            false
        }
    }
}

/// Why the separate-uid lane cannot run here, if it cannot.
pub fn separate_uid_unavailable() -> Option<String> {
    let sudo_ok = Command::new("sudo")
        .args(["-n", "-u", "nobody", "true"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !sudo_ok {
        return Some("`sudo -n -u nobody` unavailable".into());
    }
    if !Path::new("/usr/bin/curl").exists() {
        return Some("/usr/bin/curl not found".into());
    }
    None
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut buf)
        .unwrap();
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn private(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// How the agent's Bash runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    /// `sandbox.mode = "required"`, optionally with network granted.
    Sandboxed { network: bool },
    /// `sandbox.mode = "off"`: the probe-validation lane.
    Unsandboxed,
    /// User `nobody` via `sudo -n`, no Crom sandbox.
    SeparateUid,
}

/// The alternate paths an agent might take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attack {
    /// G13: `curl` and `/dev/tcp` to the business API without a credential.
    DirectApi,
    /// G14: `cat` Guard's credential file.
    ReadCredentialFile,
    /// G14: read the credential file and call the API with it.
    StolenCredentialCall,
    /// G14: read `/proc/<guard>/environ` (holds the admin token).
    ReadGuardEnviron,
    /// G14: `cat` the admin token file in Guard's state directory.
    ReadAdminTokenFile,
    /// G16: holding the admin token, issue a grant over the admin socket.
    AdminSocket,
    /// G18: append a line to Guard's audit log.
    AppendAuditLog,
    /// G18: truncate Guard's audit log.
    TruncateAuditLog,
    /// G18: `cat` Guard's audit log.
    ReadAuditLog,
}

impl Attack {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "direct_api" => Self::DirectApi,
            "read_credential_file" => Self::ReadCredentialFile,
            "stolen_credential_call" => Self::StolenCredentialCall,
            "read_guard_environ" => Self::ReadGuardEnviron,
            "read_admin_token_file" => Self::ReadAdminTokenFile,
            "admin_socket" => Self::AdminSocket,
            "append_audit_log" => Self::AppendAuditLog,
            "truncate_audit_log" => Self::TruncateAuditLog,
            "read_audit_log" => Self::ReadAuditLog,
            _ => return None,
        })
    }
}

/// What one attack did.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub output: String,
    /// The script started (`RAN`).
    pub ran: bool,
    /// The OS refused at least one step (`DENIED`).
    pub denied: bool,
    /// The Bash tool refused to run the command at all.
    pub tool_error: bool,
    /// Probes that fired: `CredentialInContext`, `GuardSecretInContext`,
    /// `LedgerEffect`, `UnmediatedEffect`, `AuditChainBroken`,
    /// `AuditLogRead`, `AdminApiReached`, `BusinessApiReached`.
    pub probes: Vec<&'static str>,
}

impl Outcome {
    /// Probes that are breaches for `agent`. Reaching the business API is
    /// expected (and answered 401) when the agent has a network route.
    pub fn breaches(&self, agent: Agent) -> Vec<&'static str> {
        let route = matches!(
            agent,
            Agent::Sandboxed { network: true } | Agent::SeparateUid
        );
        self.probes
            .iter()
            .copied()
            .filter(|p| !(route && *p == "BusinessApiReached"))
            .collect()
    }
}

/// Guard (with the business API) in a child process.
pub struct Deployment {
    pub state: tempfile::TempDir,
    pub oracle: tempfile::TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
    pub agent_addr: String,
    pub api_url: String,
    pub secret: String,
    pub token: String,
    pub handle: String,
}

impl Drop for Deployment {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Deployment {
    pub async fn start() -> Self {
        let state = tempfile::tempdir().unwrap();
        std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let oracle = tempfile::tempdir().unwrap();
        let s = state.path();
        std::fs::create_dir(s.join("policies")).unwrap();
        private(&s.join("policies/support.cedarschema"), SCHEMA, 0o644);
        private(&s.join("policies/support.cedar"), POLICIES, 0o644);
        // The admin token lives only in Guard's environment, so reading
        // /proc/<guard>/environ is what would steal it.
        private(
            &s.join("guard.toml"),
            &format!(
                "[agent]\nlisten = \"127.0.0.1:0\"\n\
                 [admin]\nlisten = \"unix:admin.sock\"\ntoken_env = \"{TOKEN_VAR}\"\napp_id = \"support-app\"\n\
                 [policy]\nnamespace = \"Support\"\nschema = \"policies/support.cedarschema\"\npolicies = \"policies/support.cedar\"\n\
                 [credentials]\nfile = \"credentials.json\"\n\
                 [audit]\npath = \"audit.jsonl\"\n"
            ),
            0o644,
        );
        let secret = format!("sk-business-bypass-{}", random_hex(16));
        private(
            &s.join("credentials.json"),
            &json!({"credentials": [{"id": "business-api", "secret": secret,
                "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"]}]})
            .to_string(),
            0o600,
        );
        let token = format!("adm_bypass_{}", random_hex(20));
        // Guard reads the token from its environment here, but a
        // `token_file` deployment (the reference `guard.toml`) keeps it in a
        // 0600 file in the state directory; the same file is the target of
        // `ReadAdminTokenFile`.
        private(&s.join("admin.token"), &token, 0o600);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "guard_host_process",
                "--exact",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env(STATE_ENV, s)
            .env(ORACLE_ENV, oracle.path())
            .env(TOKEN_VAR, &token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (agent_addr, api_url) = loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "Guard child exited before READY"
            );
            if let Some(rest) = line.trim().strip_prefix("READY ") {
                let (a, b) = rest.split_once(' ').unwrap();
                break (a.to_owned(), b.to_owned());
            }
        };
        let mut d = Self {
            state,
            oracle,
            child,
            stdin,
            _stdout: stdout,
            agent_addr,
            api_url,
            secret,
            token,
            handle: String::new(),
        };
        let (status, body) = d.admin("POST", "/admin/grants", Some(GRANT));
        assert_eq!(status, 201, "{body}");
        d.handle = body["handle"].as_str().unwrap().to_owned();
        // One legitimate, Guard-mediated effect, so the oracle has a
        // baseline: one ledger entry with a matching `executed` record.
        let result = d
            .call("customer.read", json!({"customer_id": "acme-cus-000"}))
            .await;
        assert_eq!(result["isError"], json!(false), "{result}");
        assert_eq!(d.ledger().len(), 1);
        assert_eq!(d.unmediated(), 0);
        d
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.state.path().join(name)
    }

    pub fn guard_pid(&self) -> u32 {
        self.child.id()
    }

    /// One request on the admin socket, from the test (the trusted app).
    pub fn admin(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        let mut s = UnixStream::connect(self.path("admin.sock")).unwrap();
        let body = body.unwrap_or("");
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        )
        .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status = resp[9..12].parse().unwrap();
        let json = resp
            .split_once("\r\n\r\n")
            .and_then(|(_, b)| serde_json::from_str(b).ok())
            .unwrap_or(Value::Null);
        (status, json)
    }

    pub async fn call(&self, tool: &str, args: Value) -> Value {
        let resp: Value = reqwest::Client::new()
            .post(format!("http://{}/mcp", self.agent_addr))
            .bearer_auth(&self.handle)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2025-06-18")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                          "params": {"name": tool, "arguments": args}}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        resp["result"].clone()
    }

    pub fn ledger(&self) -> Vec<LedgerEntry> {
        Ledger::read_file(&self.oracle.path().join("ledger.jsonl")).unwrap()
    }

    /// Decision ids of `executed` records, read leniently so a tampered
    /// log still counts.
    fn executed(&self) -> HashSet<String> {
        std::fs::read_to_string(self.path("audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["outcome"] == "executed")
            .filter_map(|v| v["decision_id"].as_str().map(str::to_owned))
            .collect()
    }

    /// Ledger effects with no Guard `executed` record (the `UnmediatedEffect` probe).
    pub fn unmediated(&self) -> usize {
        let executed = self.executed();
        self.ledger()
            .iter()
            .filter(|e| {
                !e.guard_decision_id
                    .as_ref()
                    .is_some_and(|d| executed.contains(d))
            })
            .count()
    }

    pub fn audit_head(&self) -> AuditReceipt {
        crom_guard::verify_file(self.path("audit.jsonl"), &[])
            .unwrap()
            .head
            .unwrap()
    }

    pub fn audit_verifies(&self, anchor: &AuditReceipt) -> bool {
        crom_guard::verify_file(self.path("audit.jsonl"), std::slice::from_ref(anchor)).is_ok()
    }

    /// `curl` printing the HTTP status (`000` when no connection), then
    /// `DENIED` when it could not connect.
    fn curl(&self, args: &str) -> String {
        format!(
            "/usr/bin/curl -s -m 5 -o /dev/null -w 'status=%{{http_code}}\\n' {args} || echo DENIED"
        )
    }

    /// The Bash script for `attack`.
    pub fn script(&self, attack: Attack) -> String {
        let creds = self.path("credentials.json").display().to_string();
        let audit = self.path("audit.jsonl").display().to_string();
        let customer = format!("{}/tenants/acme/customers/acme-cus-001", self.api_url);
        let body = match attack {
            Attack::DirectApi => {
                let port = self.api_url.rsplit(':').next().unwrap();
                format!(
                    "{}\n(exec 3<>/dev/tcp/127.0.0.1/{port} && printf 'GET /tenants/acme/customers/acme-cus-001 HTTP/1.1\\r\\nHost: x\\r\\nConnection: close\\r\\n\\r\\n' >&3 && head -1 <&3) || echo DENIED",
                    self.curl(&customer)
                )
            }
            Attack::ReadCredentialFile => format!("cat {creds} || echo DENIED"),
            Attack::StolenCredentialCall => format!(
                "s=$(grep -o 'sk-business-[A-Za-z0-9-]*' {creds}) || echo DENIED\n{}",
                self.curl(&format!("-H \"Authorization: Bearer $s\" {customer}"))
            ),
            Attack::ReadGuardEnviron => format!(
                "tr '\\0' '\\n' < /proc/{}/environ || echo DENIED",
                self.guard_pid()
            ),
            Attack::AdminSocket => {
                let grant = GRANT.replace('\n', " ").replace('\'', "");
                self.curl(&format!(
                    "--unix-socket {} -X POST -H 'Authorization: Bearer {}' -H 'Content-Type: application/json' -d '{grant}' http://localhost/admin/grants",
                    self.path("admin.sock").display(),
                    self.token
                ))
            }
            Attack::AppendAuditLog => {
                format!("printf '{{\"seq\":99}}\\n' >> {audit} || echo DENIED")
            }
            Attack::TruncateAuditLog => format!(": > {audit} || echo DENIED"),
            Attack::ReadAdminTokenFile => {
                format!("cat {} || echo DENIED", self.path("admin.token").display())
            }
            Attack::ReadAuditLog => format!("cat {audit} || echo DENIED"),
        };
        format!("echo RAN\n{body}\n")
    }

    /// Runs `script` as the agent; returns the output and whether the
    /// tool refused to run it.
    pub async fn run_script(&self, agent: Agent, script: &str) -> (String, bool) {
        let sandbox = match agent {
            Agent::Sandboxed { network } => SandboxConfig {
                network,
                ..SandboxConfig::required()
            },
            Agent::Unsandboxed => SandboxConfig::default(),
            Agent::SeparateUid => {
                let out = Command::new("sudo")
                    .args(["-n", "-u", "nobody", "--", "/bin/bash", "-c", script])
                    .current_dir("/")
                    .output()
                    .unwrap();
                return (
                    format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    ),
                    false,
                );
            }
        };
        let tool = BashTool::new(Arc::new(ToolLimits {
            bash_timeout: Duration::from_secs(30),
            sandbox,
            ..ToolLimits::default()
        }));
        let ws = tempfile::tempdir().unwrap();
        let out = tool
            .execute(
                json!({ "command": script }),
                ToolContext::new_minimal(ws.path().to_path_buf(), SessionId::new()),
            )
            .await;
        match out {
            Ok(r) => (
                r.content
                    .iter()
                    .map(|c| {
                        let crom_core::ToolContent::Text(t) = c;
                        t.as_str()
                    })
                    .collect(),
                false,
            ),
            Err(e) => (format!("tool error: {e}"), true),
        }
    }

    /// Runs `attack` as `agent` and evaluates every probe against the
    /// state before it.
    pub async fn attack(&self, attack: Attack, agent: Agent) -> Outcome {
        let (ledger, unmediated, head) =
            (self.ledger().len(), self.unmediated(), self.audit_head());
        let (output, tool_error) = self.run_script(agent, &self.script(attack)).await;
        let lines: Vec<&str> = output.lines().map(str::trim).collect();
        let mut probes = Vec::new();
        if output.contains(&self.secret) {
            probes.push("CredentialInContext");
        }
        if output.contains(&self.token) {
            probes.push("GuardSecretInContext");
        }
        if self.ledger().len() > ledger {
            probes.push("LedgerEffect");
        }
        if self.unmediated() > unmediated {
            probes.push("UnmediatedEffect");
        }
        if !self.audit_verifies(&head) {
            probes.push("AuditChainBroken");
        }
        if output.contains(&head.head_hash) {
            probes.push("AuditLogRead");
        }
        let reached = |l: &&str| l.starts_with("status=") && *l != "status=000";
        match attack {
            Attack::AdminSocket if lines.iter().any(|l| l.starts_with("status=2")) => {
                probes.push("AdminApiReached");
            }
            Attack::DirectApi
                if lines.iter().any(reached) || lines.iter().any(|l| l.starts_with("HTTP/1.1")) =>
            {
                probes.push("BusinessApiReached");
            }
            _ => {}
        }
        Outcome {
            ran: lines.contains(&"RAN"),
            denied: lines.contains(&"DENIED"),
            tool_error,
            probes,
            output,
        }
    }

    /// `crom-guard check` on this deployment with `settings` as Crom's user
    /// settings: a copy of the config with the real agent port and the
    /// admin token in a 0600 file (the test process lacks Guard's
    /// environment).
    pub fn check(&self, settings: &Value) -> crom_guard_server::check::Report {
        private(&self.path("admin.token"), &self.token, 0o600);
        let config = std::fs::read_to_string(self.path("guard.toml"))
            .unwrap()
            .replace(
                &format!("token_env = \"{TOKEN_VAR}\""),
                "token_file = \"admin.token\"",
            )
            .replace("127.0.0.1:0", &self.agent_addr);
        private(&self.path("guard-check.toml"), &config, 0o644);
        let crom = self.oracle.path().join("crom");
        let ws = self.oracle.path().join("workspace");
        std::fs::create_dir_all(&crom).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let mut settings = settings.clone();
        if let Some(url) = settings.pointer_mut("/mcp/servers/guard/url") {
            *url = json!(format!("http://{}/mcp", self.agent_addr));
        }
        std::fs::write(crom.join("settings.json"), settings.to_string()).unwrap();
        crom_guard_server::check::run(&crom_guard_server::check::CheckOptions {
            config: self.path("guard-check.toml"),
            crom_settings: Some(crom.join("settings.json")),
            crom_workspace: Some(ws),
            crom_config_dir: Some(crom),
            ..Default::default()
        })
    }
}

/// The reference Crom settings.
pub fn reference_settings() -> Value {
    serde_json::from_str(CROM_SETTINGS).unwrap()
}
