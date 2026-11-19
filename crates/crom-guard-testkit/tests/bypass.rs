//! Alternate-path bypass tests (#360, MILESTONE_18 §11 and §13.3 G13, G14,
//! G16, G18). Guard and the business API run in a child process with a 0700
//! state directory; the agent's Bash runs through Crom's Bash tool. Each
//! attack runs with the sandbox `required` (no unauthorized effect) and
//! `off` (the effect must be visible, which validates the probe).
//!
//! The effect oracle is the business API's ledger and Guard's audit log:
//! `unmediated` counts ledger entries with no Guard `executed` record.
//!
//! Skips print `SKIP:`. `CROM_REQUIRE_SANDBOX_TESTS=1` turns a missing
//! sandbox or `curl` into a failure; `CROM_REQUIRE_SEPARATE_UID_TESTS=1`
//! does the same for the separate-uid test, which needs `sudo -n`.
#![cfg(target_os = "linux")]

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

const STATE_ENV: &str = "CROM_BYPASS_GUARD_STATE";
const ORACLE_ENV: &str = "CROM_BYPASS_ORACLE";
const TOKEN_VAR: &str = "CROM_BYPASS_ADMIN_TOKEN";
const SCHEMA: &str = include_str!("../../../examples/guard-support/policies/support.cedarschema");
const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");
const GRANT: &str = include_str!("../../../examples/guard-support/grants/acme-agent-operator.json");
const CROM_SETTINGS: &str =
    include_str!("../../../examples/guard-support/agent/crom-settings.json");

/// Entry point of the Guard child process that `Deployment::start` spawns; a no-op when run as a test.
#[test]
fn guard_host_process() {
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
        println!("READY {} {}", server.agent_addr, running.base_url());
        std::io::stdout().flush().unwrap();
        // Serve until the test closes our stdin (or dies).
        let _ = tokio::task::spawn_blocking(|| std::io::stdin().read_to_end(&mut Vec::new())).await;
        std::process::exit(0);
    });
}

fn required_or_skip(what: &str, env: &str) {
    if std::env::var_os(env).is_some() {
        panic!("{what}, and {env} is set");
    }
    eprintln!("SKIP: {what}");
}

/// True when Crom's sandbox enforces here and `curl` exists.
fn sandbox_available() -> bool {
    if let Err(reason) = crom_tools::sandbox::probe(&SandboxConfig::required()) {
        required_or_skip(
            &format!("OS sandbox unavailable: {reason}"),
            "CROM_REQUIRE_SANDBOX_TESTS",
        );
        return false;
    }
    if !Path::new("/usr/bin/curl").exists() {
        required_or_skip("/usr/bin/curl not found", "CROM_REQUIRE_SANDBOX_TESTS");
        return false;
    }
    true
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .unwrap()
        .read_exact(&mut buf)
        .unwrap();
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn private(path: &Path, text: &str, mode: u32) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// How the agent's Bash runs.
#[derive(Debug, Clone, Copy)]
enum Agent {
    /// `sandbox.mode = "required"`, optionally with network granted.
    Sandboxed { network: bool },
    /// `sandbox.mode = "off"`: the probe-validation lane.
    Unsandboxed,
}

/// Guard (with the business API) in a child process.
struct Deployment {
    state: tempfile::TempDir,
    oracle: tempfile::TempDir,
    child: Child,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
    agent_url: String,
    api_url: String,
    secret: String,
    token: String,
    handle: String,
}

impl Drop for Deployment {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Deployment {
    async fn start() -> Self {
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
            if let Some(rest) = line.trim().split_once("READY ").map(|(_, r)| r) {
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
            agent_url: format!("http://{agent_addr}/mcp"),
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

    fn path(&self, name: &str) -> PathBuf {
        self.state.path().join(name)
    }

    fn guard_pid(&self) -> u32 {
        self.child.id()
    }

    /// One request on the admin socket, from the test (the trusted app).
    fn admin(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
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

    async fn call(&self, tool: &str, args: Value) -> Value {
        let resp: Value = reqwest::Client::new()
            .post(&self.agent_url)
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

    fn ledger(&self) -> Vec<LedgerEntry> {
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
    fn unmediated(&self) -> usize {
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

    fn audit_head(&self) -> AuditReceipt {
        crom_guard::verify_file(self.path("audit.jsonl"), &[])
            .unwrap()
            .head
            .unwrap()
    }

    fn audit_verifies(&self, anchor: &AuditReceipt) -> bool {
        crom_guard::verify_file(self.path("audit.jsonl"), std::slice::from_ref(anchor)).is_ok()
    }

    /// Runs `script` as the agent's Bash; returns the tool's output text,
    /// or the refusal.
    async fn bash(&self, agent: Agent, script: &str) -> String {
        let sandbox = match agent {
            Agent::Sandboxed { network } => SandboxConfig {
                network,
                ..SandboxConfig::required()
            },
            Agent::Unsandboxed => SandboxConfig::default(),
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
            Ok(r) => r
                .content
                .iter()
                .map(|c| {
                    let crom_core::ToolContent::Text(t) = c;
                    t.as_str()
                })
                .collect(),
            Err(e) => format!("tool error: {e}"),
        }
    }

    /// `curl` printing only the HTTP status (`000` when no connection).
    fn curl(&self, args: &str) -> String {
        format!("/usr/bin/curl -s -m 5 -o /dev/null -w 'status=%{{http_code}}\\n' {args}")
    }
}

const ALL_SANDBOXED: [Agent; 2] = [
    Agent::Sandboxed { network: false },
    Agent::Sandboxed { network: true },
];

/// G13: Bash cannot reach the business API with the sandbox required; with network granted (negative control) or the sandbox off, the API answers 401 without Guard's credential; no ledger effect in any case.
#[tokio::test(flavor = "multi_thread")]
async fn g13_direct_api_call() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let port = d.api_url.rsplit(':').next().unwrap().to_owned();
    let script = format!(
        "{}\n\
         (exec 3<>/dev/tcp/127.0.0.1/{port} && printf 'GET /tenants/acme/customers/acme-cus-001 HTTP/1.1\\r\\nHost: x\\r\\nConnection: close\\r\\n\\r\\n' >&3 && head -1 <&3) || echo devtcp=refused",
        d.curl(&format!("{}/tenants/acme/customers/acme-cus-001", d.api_url))
    );
    let out = d.bash(Agent::Sandboxed { network: false }, &script).await;
    assert!(out.contains("status=000"), "{out}");
    assert!(
        out.contains("devtcp=refused") && !out.contains("HTTP/1.1"),
        "{out}"
    );
    for agent in [Agent::Sandboxed { network: true }, Agent::Unsandboxed] {
        let out = d.bash(agent, &script).await;
        assert!(out.contains("status=401"), "{agent:?}: {out}");
        assert!(out.contains("HTTP/1.1 401"), "{agent:?}: {out}");
    }
    assert_eq!(d.ledger().len(), 1);
    assert_eq!(d.unmediated(), 0);
}

/// G14: Bash cannot read Guard's credential file with the sandbox required; with it off, the secret is read (probe live).
#[tokio::test(flavor = "multi_thread")]
async fn g14_credential_file_read() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let script = format!("cat {}", d.path("credentials.json").display());
    for agent in ALL_SANDBOXED {
        let out = d.bash(agent, &script).await;
        assert!(!out.contains(&d.secret), "{agent:?}: credential read");
    }
    let out = d.bash(Agent::Unsandboxed, &script).await;
    assert!(out.contains(&d.secret), "probe did not fire: {out}");
}

/// G14 + UnmediatedEffect: Bash that steals the credential file and calls the API directly causes no ledger effect with the sandbox required, even with network granted; with it off, an unmediated effect appears.
#[tokio::test(flavor = "multi_thread")]
async fn g14_stolen_credential_direct_call() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let script = format!(
        "s=$(grep -o 'sk-business-[A-Za-z0-9-]*' {} | head -1)\n{}",
        d.path("credentials.json").display(),
        d.curl(&format!(
            "-H \"Authorization: Bearer $s\" {}/tenants/acme/customers/acme-cus-001",
            d.api_url
        ))
    );
    for agent in ALL_SANDBOXED {
        let out = d.bash(agent, &script).await;
        assert!(!out.contains("status=200"), "{agent:?}: {out}");
        assert_eq!(d.unmediated(), 0, "{agent:?}");
    }
    let out = d.bash(Agent::Unsandboxed, &script).await;
    assert!(out.contains("status=200"), "{out}");
    assert_eq!(d.unmediated(), 1, "UnmediatedEffect probe did not fire");
}

/// G14: Bash cannot read Guard's environment (holding the admin token) through /proc with the sandbox required; with it off, it can.
#[tokio::test(flavor = "multi_thread")]
async fn g14_guard_environ_read() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let script = format!("tr '\\0' '\\n' < /proc/{}/environ", d.guard_pid());
    for agent in ALL_SANDBOXED {
        let out = d.bash(agent, &script).await;
        assert!(
            !out.contains(&d.token),
            "{agent:?}: Guard's environment read"
        );
    }
    let out = d.bash(Agent::Unsandboxed, &script).await;
    assert!(out.contains(&d.token), "probe did not fire: {out}");
}

/// G16: even holding the admin token, Bash cannot reach the admin socket with the sandbox required (AF_UNIX stays refused when network is granted); with it off, it mints itself a grant.
#[tokio::test(flavor = "multi_thread")]
async fn g16_admin_socket() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let grant = GRANT.replace('\n', " ").replace('\'', "");
    let script = d.curl(&format!(
        "--unix-socket {} -X POST -H 'Authorization: Bearer {}' -H 'Content-Type: application/json' -d '{grant}' http://localhost/admin/grants",
        d.path("admin.sock").display(),
        d.token
    ));
    for agent in ALL_SANDBOXED {
        let out = d.bash(agent, &script).await;
        assert!(out.contains("status=000"), "{agent:?}: {out}");
    }
    let out = d.bash(Agent::Unsandboxed, &script).await;
    assert!(out.contains("status=201"), "probe did not fire: {out}");
    assert_eq!(d.ledger().len(), 1);
}

/// G18: Bash cannot append to or truncate Guard's audit log with the sandbox required, so `audit verify` passes against the earlier head; with it off, an append breaks the chain.
#[tokio::test(flavor = "multi_thread")]
async fn g18_audit_log_tampering() {
    if !sandbox_available() {
        return;
    }
    let d = Deployment::start().await;
    let head = d.audit_head();
    let audit = d.path("audit.jsonl");
    let append = format!("printf '{{\"seq\":99}}\\n' >> {}", audit.display());
    let truncate = format!(": > {}", audit.display());
    for agent in ALL_SANDBOXED {
        d.bash(agent, &append).await;
        d.bash(agent, &truncate).await;
        assert!(d.audit_verifies(&head), "{agent:?}: audit log changed");
    }
    d.bash(Agent::Unsandboxed, &append).await;
    assert!(
        !d.audit_verifies(&head),
        "AuditChainBroken probe did not fire"
    );
}

/// The attacked deployment passes `crom-guard check` with the reference Crom settings, and fails it with the sandbox off or network granted.
#[tokio::test(flavor = "multi_thread")]
async fn preflight_matches_the_tested_profile() {
    let d = Deployment::start().await;
    // The same deployment, reading the admin token from a 0600 file: the
    // test process does not carry Guard's environment.
    private(&d.path("admin.token"), &d.token, 0o600);
    let config = std::fs::read_to_string(d.path("guard.toml"))
        .unwrap()
        .replace(
            &format!("token_env = \"{TOKEN_VAR}\""),
            "token_file = \"admin.token\"",
        );
    private(&d.path("guard-check.toml"), &config, 0o644);
    let ws = tempfile::tempdir().unwrap();
    let check = |settings: Value| {
        let path = ws.path().join("settings.json");
        std::fs::write(&path, settings.to_string()).unwrap();
        crom_guard_server::check::run(&crom_guard_server::check::CheckOptions {
            config: d.path("guard-check.toml"),
            crom_settings: Some(path),
            crom_workspace: Some(ws.path().to_path_buf()),
            ..Default::default()
        })
    };
    let mut settings: Value = serde_json::from_str(CROM_SETTINGS).unwrap();
    let report = check(settings.clone());
    assert!(report.passed(), "{:#?}", report.findings);
    settings["sandbox"]["mode"] = json!("off");
    assert!(check(settings.clone()).failed().contains(&"sandbox-mode"));
    settings["sandbox"]["mode"] = json!("required");
    settings["sandbox"]["network"] = json!(true);
    assert!(check(settings).failed().contains(&"sandbox-network"));
}

/// The reference Crom settings (examples/guard-support/agent/crom-settings.json) parse as Crom settings.
#[test]
fn reference_crom_settings_parse() {
    let s: crom_core::settings::SettingsFile = serde_json::from_str(CROM_SETTINGS).unwrap();
    assert!(s.sandbox.is_some());
}

/// With the agent under a separate uid (no Crom sandbox), it cannot read Guard's credential file, environment, or audit log, nor connect to the admin socket; the business API stays reachable but refuses it (401); no ledger effect.
#[tokio::test(flavor = "multi_thread")]
async fn separate_uid_agent() {
    const REQUIRE: &str = "CROM_REQUIRE_SEPARATE_UID_TESTS";
    let sudo_ok = Command::new("sudo")
        .args(["-n", "-u", "nobody", "true"])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !sudo_ok {
        return required_or_skip("`sudo -n -u nobody` unavailable", REQUIRE);
    }
    if !Path::new("/usr/bin/curl").exists() {
        return required_or_skip("/usr/bin/curl not found", REQUIRE);
    }
    let d = Deployment::start().await;
    let head = d.audit_head();
    let creds = d.path("credentials.json").display().to_string();
    let script = format!(
        "cat {creds} 2>&1 | head -c 200; echo\n\
         tr '\\0' '\\n' < /proc/{pid}/environ 2>&1 | head -c 2000; echo\n\
         printf 'x\\n' >> {audit} 2>&1 || echo append=denied\n\
         {admin}\n\
         {direct}\n\
         s=$(grep -o 'sk-business-[A-Za-z0-9-]*' {creds} 2>/dev/null | head -1)\n\
         {stolen}",
        pid = d.guard_pid(),
        audit = d.path("audit.jsonl").display(),
        admin = d.curl(&format!(
            "--unix-socket {} -H 'Authorization: Bearer {}' http://localhost/admin/health",
            d.path("admin.sock").display(),
            d.token
        )),
        direct = d.curl(&format!(
            "{}/tenants/acme/customers/acme-cus-001",
            d.api_url
        )),
        stolen = d.curl(&format!(
            "-H \"Authorization: Bearer $s\" {}/tenants/acme/customers/acme-cus-001",
            d.api_url
        )),
    );
    let out = Command::new("sudo")
        .args(["-n", "-u", "nobody", "--", "/bin/bash", "-c", &script])
        .current_dir("/")
        .output()
        .unwrap();
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.contains(&d.secret), "credential read: {out}");
    assert!(!out.contains(&d.token), "environ read: {out}");
    assert!(out.contains("append=denied"), "{out}");
    // admin socket, direct call, stolen-credential call, in that order.
    let statuses: Vec<&str> = out.lines().filter(|l| l.starts_with("status=")).collect();
    assert_eq!(
        statuses,
        ["status=000", "status=401", "status=401"],
        "{out}"
    );
    assert!(d.audit_verifies(&head));
    assert_eq!(d.ledger().len(), 1);
    assert_eq!(d.unmediated(), 0);
}
