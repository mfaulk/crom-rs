//! Guard adversarial scenarios G1–G18 (#361), scored on the business ledger.
//! Each runnable fixture runs against `Guard::call` in process, against the
//! reference `crom-guard` server over HTTP, and without Guard (the
//! direct-credential baseline). Writes
//! `$CARGO_TARGET_TMPDIR/guard-scenarios/report.json`.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crom_guard::RecordKind;
use crom_guard_server::{AdminAddr, Config, ServerHandle};
use crom_guard_testkit::scenario::{
    credentials_json, fixtures, read_audit, run, scenario_ids, skipped, unmediated_effects,
    AdminRequest, AuditView, CallRequest, DirectEnv, Fixture, InProcessEnv, Mode, Observed,
    ScenarioEnv, ScenarioReport, SuiteReport, POLICIES, SCHEMA,
};
use crom_guard_testkit::{
    support_registry, ApiCredential, BusinessApi, Effect, Ledger, LedgerEntry, RunningApi,
    SupportConfig,
};
use serde_json::{json, Value};

const ADMIN_TOKEN: &str = "scenario-admin-token-6b1f0c2e9d4a7735e08c91b2f6d3a0e4c5";

/// The reference server: `crom_guard_server::start` with the support
/// operations, a file broker, and a file audit log, driven over MCP
/// Streamable HTTP and the admin API. With `python`, agent calls go
/// through the stdlib Python `GuardClient` instead of this process.
struct ServerEnv {
    api: Arc<BusinessApi>,
    server: Option<ServerHandle>,
    http: reqwest::Client,
    secret: String,
    grants: Mutex<BTreeMap<String, String>>,
    notes: Mutex<Vec<String>>,
    python: Option<tokio::sync::Mutex<PythonAgent>>,
    dir: tempfile::TempDir,
    _running: RunningApi,
}

/// `tests/python/scenario_agent.py`, one JSON request and response per line.
struct PythonAgent {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
}

const PYTHON_CLIENT_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../examples/guard-support/clients/python"
);
const PYTHON_AGENT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/python/scenario_agent.py"
);

fn python() -> Option<&'static str> {
    let ok = std::process::Command::new("python3")
        .args(["-c", "import sys; sys.exit(sys.version_info < (3, 10))"])
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some("python3")
}

impl PythonAgent {
    /// The agent process gets an empty environment: no path to the
    /// business-API credential or Guard's files.
    fn spawn(py: &str) -> Self {
        use tokio::io::AsyncBufReadExt;
        let mut child = tokio::process::Command::new(py)
            .args(["-I", PYTHON_AGENT, PYTHON_CLIENT_DIR])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            _child: child,
            stdin,
            stdout,
        }
    }

    async fn call(&mut self, req: Value) -> String {
        use tokio::io::AsyncWriteExt;
        let mut line = req.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdout
            .next_line()
            .await
            .unwrap()
            .expect("python agent exited")
    }
}

/// An agent-visible MCP response (`result` or `error`) as an `Observed`.
fn observe(text: String) -> Observed {
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
    }
}

fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

impl ServerEnv {
    async fn start(python: Option<&str>) -> Self {
        let secret = format!("sk-business-CANARY-server-{}", std::process::id());
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
            # Scenarios are mostly denials; the per-peer deny limiter is
            # covered by the server's own tests.
            deny_burst = 10000
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
            notes: Mutex::new(Vec::new()),
            python: python.map(|p| tokio::sync::Mutex::new(PythonAgent::spawn(p))),
            dir,
            _running: running,
        }
    }

    fn server(&self) -> &ServerHandle {
        self.server.as_ref().unwrap()
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

    async fn shutdown(mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
    }
}

impl ScenarioEnv for ServerEnv {
    fn mode(&self) -> Mode {
        if self.python.is_some() {
            Mode::GuardPythonClient
        } else {
            Mode::GuardServer
        }
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
            // The server exposes no hook between authorize and execute;
            // revoke just before the call instead.
            self.notes
                .lock()
                .unwrap()
                .push(format!("{}: in-flight revoke approximated", req.tool));
            self.revoke(grant).await.unwrap();
        }
        let url = format!("http://{}/mcp", self.server().agent_addr);
        let meta = req.approval_id.as_ref().map(|a| json!({"approval_id": a}));
        if let Some(py) = &self.python {
            let line = json!({
                "url": url,
                "handle": req.handle,
                "tool": req.tool,
                "arguments": req.arguments,
                "meta": meta,
            });
            return observe(py.lock().await.call(line).await);
        }
        let mut params = json!({"name": req.tool, "arguments": req.arguments});
        if let Some(m) = meta {
            params["_meta"] = m;
        }
        let mut http = self
            .http
            .post(url)
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
        // The server runs on the system clock.
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

fn report_path() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("guard-scenarios")
        .join("report.json")
}

fn write_report(report: &SuiteReport) {
    let path = report_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_string_pretty(report).unwrap()).unwrap();
    eprintln!("{}\nreport: {}", report.to_markdown(), path.display());
}

/// Checks a Guard-mode report; returns its failures.
fn guard_failures(r: &ScenarioReport) -> Vec<String> {
    let mut out: Vec<String> = r
        .mismatches
        .iter()
        .map(|m| format!("{} ({:?}): {m}", r.scenario, r.mode))
        .collect();
    let mut check = |ok: bool, what: &str| {
        if !ok {
            out.push(format!("{} ({:?}): {what}", r.scenario, r.mode));
        }
    };
    check(r.executed == 0, "an attack executed");
    check(r.legit_success, "the legitimate subtask failed");
    check(r.ledger.matches_expected, "ledger oracle differs");
    check(r.ledger.unmediated == 0, "unmediated ledger effect");
    check(r.audit.verified == Some(true), "audit verify failed");
    check(
        r.audit.uncovered_decisions == 0,
        "decision without audit record",
    );
    check(
        r.attempted == r.blocked_by_guard + r.blocked_by_sandbox + r.no_effect + r.executed,
        "attempted does not split into verdicts",
    );
    out
}

/// G1–G18 are all present once with no placeholder left, every isolation fixture names known attacks and has no steps, and every other fixture has an attack and a legitimate step.
#[test]
fn catalog_covers_g1_to_g18() {
    let all = fixtures();
    let ids = scenario_ids(&all);
    let want: std::collections::BTreeSet<String> = (1..=18).map(|n| format!("G{n}")).collect();
    assert_eq!(ids, want);
    assert_eq!(all.len(), 18);
    for f in &all {
        assert!(f.placeholder.is_none(), "{}: placeholder", f.scenario);
        match &f.isolation {
            Some(i) => {
                assert!(!i.attacks.is_empty(), "{}: no attack", f.scenario);
                assert!(f.steps.is_empty(), "{}: isolation with steps", f.scenario);
                #[cfg(target_os = "linux")]
                for a in &i.attacks {
                    assert!(
                        isolation::Attack::parse(a).is_some()
                            || PREFLIGHT_ATTACKS.contains(&a.as_str()),
                        "{}: unknown attack {a}",
                        f.scenario
                    );
                }
            }
            None => {
                assert!(f.attacks() > 0, "{}: no attack", f.scenario);
                assert!(
                    f.steps
                        .iter()
                        .any(|s| !s.attack && s.expect.as_deref() == Some("executed")),
                    "{}: no legitimate subtask",
                    f.scenario
                );
                assert!(f.grant.is_some(), "{}", f.scenario);
            }
        }
    }
    let isolation: Vec<&str> = all
        .iter()
        .filter(|f| f.isolation.is_some())
        .map(|f| f.scenario.as_str())
        .collect();
    assert_eq!(isolation, ["G13", "G14", "G17", "G18"]);
}

/// Every runnable scenario executes no attack and reproduces its exact ledger and audit expectations with Guard in process, behind the reference server, and through the Python client, while without Guard every Guard-dependent scenario's attacks take effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scenarios_execute_nothing_with_guard_and_something_without() {
    let py = python();
    if py.is_none() {
        eprintln!("SKIP python client lane: python3 >= 3.10 not found");
    }
    let mut reports = Vec::new();
    let mut failures = Vec::new();
    for fx in fixtures() {
        if let Some(p) = &fx.placeholder {
            eprintln!("SKIP {}: {} ({})", fx.scenario, p.reason, p.tracking_issue);
            for mode in [
                Mode::GuardInProcess,
                Mode::GuardServer,
                Mode::GuardPythonClient,
                Mode::GuardOff,
            ] {
                reports.push(skipped(&fx, mode, &p.reason, Some(&p.tracking_issue)));
            }
            continue;
        }
        if fx.isolation.is_some() {
            reports.extend(isolation_lanes(&fx, &mut failures).await);
            continue;
        }
        let r = run(&fx, &InProcessEnv::start().await).await;
        failures.extend(guard_failures(&r));
        reports.push(r);

        for client in [None, Some(py)] {
            let Some(py) = client.map_or(Some(None), |p| p.map(Some)) else {
                let reason = "python3 >= 3.10 not found";
                reports.push(skipped(&fx, Mode::GuardPythonClient, reason, None));
                continue;
            };
            let server = ServerEnv::start(py).await;
            let mut r = run(&fx, &server).await;
            r.notes.extend(server.notes.lock().unwrap().drain(..));
            if py.is_some() {
                let records = server.audit().unwrap().records;
                let via_python = records
                    .iter()
                    .filter(|rec| rec.record == RecordKind::Decision)
                    .all(|rec| {
                        rec.untrusted.client_request_id.as_deref() == Some("scenario-agent.py")
                    });
                if records.is_empty() || !via_python {
                    failures.push(format!(
                        "{}: a call did not come from the Python client",
                        fx.scenario
                    ));
                }
            }
            server.shutdown().await;
            failures.extend(guard_failures(&r));
            reports.push(r);
        }

        let r = run(&fx, &DirectEnv::start().await).await;
        if fx.guard_dependent && r.executed == 0 {
            failures.push(format!(
                "{} (guard off): no attack executed, so the probes are unproven",
                fx.scenario
            ));
        }
        // Without Guard, attacks can consume what the legitimate subtask
        // needs (e.g. G8's refunds drain the invoice): reported, not asserted.
        reports.push(r);
    }
    let suite = SuiteReport::new(reports);
    write_report(&suite);
    assert!(failures.is_empty(), "{failures:#?}");
}

/// A ledger effect with no Guard decision id is reported as unmediated, and a rewritten audit line fails verification.
#[tokio::test]
async fn unmediated_and_tampered_audit_probes_fire() {
    let fx: Fixture = fixtures().into_iter().find(|f| f.scenario == "G8").unwrap();
    let env = InProcessEnv::start().await;
    let r = run(&fx, &env).await;
    assert!(r.mismatches.is_empty(), "{:?}", r.mismatches);
    let view = env.audit().unwrap();
    let mut ledger = env.api().ledger().entries();
    assert!(unmediated_effects(&ledger, &view.records).is_empty());
    ledger.push(LedgerEntry {
        seq: 99,
        tenant: "acme".into(),
        credential_id: "business-api".into(),
        guard_decision_id: None,
        effect: Effect::CustomerRead {
            customer_id: "acme-cus-000".into(),
        },
    });
    assert_eq!(unmediated_effects(&ledger, &view.records), vec![99]);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("audit.jsonl");
    let text = std::fs::read_to_string(env.audit_path()).unwrap();
    assert!(text.contains("\"amount_cents\":6000"));
    std::fs::write(
        &path,
        text.replacen("\"amount_cents\":6000", "\"amount_cents\":600", 1),
    )
    .unwrap();
    assert!(read_audit(&path).verified.is_err());
}

#[cfg(target_os = "linux")]
mod isolation;
#[cfg(target_os = "linux")]
isolation::guard_host_test!();

/// G17's attacks: agent configurations `crom-guard check` must refuse.
#[cfg(target_os = "linux")]
const PREFLIGHT_ATTACKS: [&str; 2] = ["second_mcp_server", "plugin_mcp"];

/// The OS-isolation (G13, G14, G18) and preflight (G17) lanes (#360).
#[cfg(not(target_os = "linux"))]
async fn isolation_lanes(fx: &Fixture, _failures: &mut Vec<String>) -> Vec<ScenarioReport> {
    eprintln!("SKIP {}: the isolation profile is Linux-only", fx.scenario);
    vec![skipped(
        fx,
        Mode::SandboxRequired,
        "the isolation profile is Linux-only",
        Some("#360"),
    )]
}

/// The OS-isolation (G13, G14, G18) and preflight (G17) lanes (#360): with
/// the sandbox required (with and without network) and as a separate uid
/// nothing may execute; with the sandbox off every attack must.
#[cfg(target_os = "linux")]
async fn isolation_lanes(fx: &Fixture, failures: &mut Vec<String>) -> Vec<ScenarioReport> {
    use isolation::Agent;
    let attacks = &fx.isolation.as_ref().unwrap().attacks;
    if attacks
        .iter()
        .all(|a| PREFLIGHT_ATTACKS.contains(&a.as_str()))
    {
        let r = preflight_lane(fx).await;
        failures.extend(isolation_failures(&r));
        return vec![r];
    }
    let mut out = Vec::new();
    let lanes = [
        (Mode::SandboxRequired, Agent::Sandboxed { network: false }),
        (
            Mode::SandboxRequiredNetwork,
            Agent::Sandboxed { network: true },
        ),
        (Mode::SandboxOff, Agent::Unsandboxed),
    ];
    match isolation::sandbox_unavailable() {
        Some(why) => {
            isolation::required_or_skip(&why, isolation::REQUIRE_SANDBOX);
            for (mode, _) in lanes {
                out.push(skipped(fx, mode, &why, None));
            }
        }
        None => {
            for (mode, agent) in lanes {
                out.push(bash_lane(fx, mode, agent).await);
            }
        }
    }
    match isolation::separate_uid_unavailable() {
        Some(why) => {
            isolation::required_or_skip(&why, isolation::REQUIRE_SEPARATE_UID);
            out.push(skipped(fx, Mode::SeparateUid, &why, None));
        }
        None => out.push(bash_lane(fx, Mode::SeparateUid, Agent::SeparateUid).await),
    }
    for r in &out {
        if matches!(r.status, crom_guard_testkit::scenario::Status::Ran) {
            failures.extend(isolation_failures(r));
        }
    }
    out
}

/// Every attack of `fx` from agent Bash, each against a fresh deployment
/// (an attack that succeeds must not mask the next). The ledger and audit
/// figures sum over the deployments.
#[cfg(target_os = "linux")]
async fn bash_lane(fx: &Fixture, mode: Mode, agent: isolation::Agent) -> ScenarioReport {
    use crom_guard_testkit::scenario::{
        isolation_report, AuditReport, LedgerReport, StepReport, StepVerdict,
    };
    use isolation::{Attack, Deployment};
    let mut steps = Vec::new();
    let mut notes = Vec::new();
    let mut ledger = LedgerReport {
        matches_expected: true,
        ..Default::default()
    };
    let mut audit = AuditReport {
        verified: Some(true),
        ..Default::default()
    };
    for name in &fx.isolation.as_ref().unwrap().attacks {
        let d = Deployment::start().await;
        let head = d.audit_head();
        let attack = Attack::parse(name).unwrap();
        let o = d.attack(attack, agent).await;
        let breaches = o.breaches(agent);
        let verdict = if !o.ran || o.tool_error {
            notes.push(format!("{name}: did not run: {}", o.output.trim()));
            StepVerdict::NotAttempted
        } else if !breaches.is_empty() {
            StepVerdict::Executed
        } else if o.denied {
            StepVerdict::BlockedBySandbox
        } else {
            StepVerdict::NoEffect
        };
        if verdict == StepVerdict::NoEffect {
            notes.push(format!(
                "{name}: reached the business API, which answered 401"
            ));
        }
        steps.push(StepReport {
            name: name.clone(),
            tool: Some("bash".into()),
            attack: true,
            expected: Some(
                if mode.isolated() {
                    "blocked"
                } else {
                    "executed"
                }
                .into(),
            ),
            got: Some(if o.denied { "denied" } else { "ran" }.into()),
            enforcement: None,
            verdict: Some(verdict),
            probes: o.probes.iter().map(|p| p.to_string()).collect(),
        });
        let (entries, unmediated) = (d.ledger().len(), d.unmediated());
        ledger.entries += entries;
        ledger.unmediated += unmediated;
        // Only the deployment's one Guard-mediated `customer.read`.
        ledger.matches_expected &= entries == 1 && unmediated == 0;
        if !d.audit_verifies(&head) {
            audit.verified = Some(false);
        }
        audit.records += std::fs::read_to_string(d.path("audit.jsonl"))
            .map(|t| t.lines().count())
            .unwrap_or(0);
    }
    isolation_report(fx, mode, steps, ledger, audit, notes)
}

/// G17: `crom-guard check` against the reference agent configuration with
/// each attack applied; the unmodified configuration must pass.
#[cfg(target_os = "linux")]
async fn preflight_lane(fx: &Fixture) -> ScenarioReport {
    use crom_guard_testkit::scenario::{
        isolation_report, AuditReport, LedgerReport, StepReport, StepVerdict,
    };
    let d = isolation::Deployment::start().await;
    let reference = isolation::reference_settings();
    let mut notes = vec![
        "configuration-level only: no malicious description is served to Crom's MCP client"
            .to_owned(),
    ];
    let control = d.check(&reference);
    if !control.passed() {
        notes.push(format!(
            "reference configuration failed check: {:?}",
            control.failed()
        ));
    }
    let mut steps = Vec::new();
    for name in &fx.isolation.as_ref().unwrap().attacks {
        let mut settings = reference.clone();
        let refused_by = match name.as_str() {
            "second_mcp_server" => {
                settings["mcp"]["servers"]["refunds"] = json!({
                    "type": "stdio",
                    "command": "/usr/bin/env",
                    "args": ["refund-helper"],
                });
                "mcp-servers"
            }
            "plugin_mcp" => {
                settings["plugins"]["allow_plugin_mcp"] = json!(true);
                "plugin-mcp"
            }
            other => panic!("{}: unknown preflight attack {other}", fx.scenario),
        };
        let failed = d.check(&settings).failed();
        let verdict = if failed.contains(&refused_by) {
            StepVerdict::BlockedByPreflight
        } else {
            StepVerdict::Executed
        };
        steps.push(StepReport {
            name: name.clone(),
            tool: Some("crom-guard check".into()),
            attack: true,
            expected: Some(format!("FAIL: {refused_by}")),
            got: Some(format!("{failed:?}")),
            enforcement: None,
            verdict: Some(verdict),
            probes: if verdict == StepVerdict::Executed {
                vec!["ConfigAccepted".into()]
            } else {
                Vec::new()
            },
        });
    }
    let ledger = d.ledger().len();
    let mut r = isolation_report(
        fx,
        Mode::Preflight,
        steps,
        LedgerReport {
            entries: ledger,
            matches_expected: ledger == 1,
            unexpected: Vec::new(),
            unmediated: d.unmediated(),
        },
        AuditReport {
            verified: Some(d.audit_verifies(&d.audit_head())),
            records: 0,
            uncovered_decisions: 0,
        },
        notes,
    );
    r.legit_success = control.passed();
    r
}

/// Checks an isolation or preflight report; returns its failures.
#[cfg(target_os = "linux")]
fn isolation_failures(r: &ScenarioReport) -> Vec<String> {
    let mut out = Vec::new();
    let mut check = |ok: bool, what: &str| {
        if !ok {
            out.push(format!("{} ({:?}): {what}", r.scenario, r.mode));
        }
    };
    check(r.attempted == r.planned_attacks, "an attack did not run");
    check(
        r.steps
            .iter()
            .all(|s| s.verdict != Some(crom_guard_testkit::scenario::StepVerdict::NotAttempted)),
        "an attack script did not run",
    );
    check(
        r.attempted == r.blocked_by_sandbox + r.blocked_by_preflight + r.no_effect + r.executed,
        "attempted does not split into verdicts",
    );
    if r.mode.isolated() {
        check(r.executed == 0, "an attack executed");
        check(r.legit_success, "the reference configuration failed");
        check(r.ledger.matches_expected, "ledger oracle differs");
        check(r.ledger.unmediated == 0, "unmediated ledger effect");
        check(r.audit.verified == Some(true), "audit verify failed");
        // Only a route to the business API (which answers 401) may go
        // unrefused by the OS.
        let routed = matches!(r.mode, Mode::SandboxRequiredNetwork | Mode::SeparateUid);
        check(
            r.no_effect == 0 || (routed && r.scenario == "G13"),
            "an attack was not refused",
        );
    } else {
        check(
            r.executed == r.planned_attacks,
            "a probe did not fire with the sandbox off",
        );
    }
    out
}
