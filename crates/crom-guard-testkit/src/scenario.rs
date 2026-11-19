//! Scripted Guard adversarial scenarios scored on the business ledger
//! (MILESTONE_18 §13.2–§13.3, #361). A step's verdict comes from probes over
//! the ledger and the agent-visible result; Guard's audit log is checked
//! against those probes, never used in their place.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crom_guard::{
    verify_log_with, ActionDigest, AppId, ApprovalSpec, ApprovalStoreConfig, AuditEntry,
    AuditError, AuditReceipt, AuditRecord, AuditSink, BoxFuture, CallMeta, CallResult, CedarPdp,
    Clock, DecisionKind, Delegator, FileAuditSink, FileCredentialBroker, GrantId, GrantSpec,
    GrantStore, GrantStoreConfig, Guard, MemoryApprovalStore, MemoryGrantStore, RecordKind,
    TenantId, Timestamp, HANDLE_PREFIX,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{ApiCredential, BusinessApi, RunningApi};
use crate::data::TENANTS;
use crate::ledger::{Ledger, LedgerEntry};
use crate::support_ops::{support_registry, SupportConfig};

/// Report format written by the scenario driver; read by the #363 scoreboard.
pub const REPORT_FORMAT: &str = "guard-scenarios/v1";

pub const SCHEMA: &str =
    include_str!("../../../examples/guard-support/policies/support.cedarschema");
pub const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");

macro_rules! fixture {
    ($f:literal) => {
        ($f, include_str!(concat!("../fixtures/scenarios/", $f)))
    };
}
/// Fixture catalog, G1–G18 in order (`fixtures/scenarios/`).
pub const FIXTURES: &[(&str, &str)] = &[
    fixture!("g01-cross-tenant-read.json"),
    fixture!("g02-tenant-id-forgery.json"),
    fixture!("g03-resource-id-substitution.json"),
    fixture!("g04-unknown-principal.json"),
    fixture!("g05-expired-grant.json"),
    fixture!("g06-revoked-grant.json"),
    fixture!("g07-unauthorized-action.json"),
    fixture!("g08-excessive-amount.json"),
    fixture!("g09-forged-approval.json"),
    fixture!("g10-subagent-escalation.json"),
    fixture!("g11-bulk-export.json"),
    fixture!("g12-tool-alias.json"),
    fixture!("g13-direct-api-bypass.json"),
    fixture!("g14-credential-access.json"),
    fixture!("g15-injection-in-customer-record.json"),
    fixture!("g16-injection-in-tool-output.json"),
    fixture!("g17-malicious-tool-description.json"),
    fixture!("g18-audit-tampering.json"),
];

/// Session-handle names a step's `as` may use besides bound handles.
pub const NO_HANDLE: &str = "$none";
/// Well-formed but never issued.
pub const FORGED_HANDLE: &str = "$forged";
pub const MALFORMED_HANDLE: &str = "$malformed";

/// One scenario, as JSON under `fixtures/scenarios/`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    pub scenario: String,
    pub title: String,
    pub spec: String,
    pub vector: String,
    pub expected: String,
    /// Set for scenarios this harness cannot run (OS isolation); reported
    /// as skipped with the reason, never as passing.
    #[serde(default)]
    pub placeholder: Option<Placeholder>,
    /// Without Guard (the direct-credential baseline), at least one attack
    /// must execute. False when the vector does not exist without Guard.
    #[serde(default)]
    pub guard_dependent: bool,
    /// Text that must reach the agent through a tool result.
    #[serde(default)]
    pub injection_marker: Option<String>,
    /// The agent's grant (a `GrantSpec`), bound as `parent`.
    #[serde(default)]
    pub grant: Option<Value>,
    /// More grants the application issues at the start, by name.
    #[serde(default)]
    pub grants: BTreeMap<String, Value>,
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Every ledger effect a Guard run may produce, in order, as partial
    /// `LedgerEntry` objects. Empty means none.
    #[serde(default)]
    pub expected_ledger_effects: Vec<serde_json::Map<String, Value>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placeholder {
    pub tracking_issue: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    /// An unauthorized attempt: any effect it causes is `executed`.
    #[serde(default)]
    pub attack: bool,
    /// A trusted-application action, not an agent call.
    #[serde(default)]
    pub app: Option<AppOp>,
    /// The agent sends an admin-API request with its own session handle.
    #[serde(default)]
    pub admin_request: Option<AdminRequest>,
    #[serde(default = "parent", rename = "as")]
    pub as_: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub arguments: Value,
    /// Bound approval name to present as `_meta.approval_id`.
    #[serde(default)]
    pub approval: Option<String>,
    /// Literal (forged) approval id to present.
    #[serde(default)]
    pub approval_id: Option<String>,
    /// Revoke this grant once the call's allow record is durable, before
    /// execution (in-process only; the server lane revokes just before the
    /// call).
    #[serde(default)]
    pub revoke_after_decision: Option<String>,
    /// `executed` or the agent-visible code (for `admin_request`,
    /// `http_<status>`).
    #[serde(default)]
    pub expect: Option<String>,
    #[serde(default)]
    pub expect_audit: Option<AuditExpect>,
    #[serde(default)]
    pub bind_handle: Option<String>,
    /// Store the `approval_required` action digest under this name.
    #[serde(default)]
    pub bind_digest: Option<String>,
}

fn parent() -> String {
    "parent".into()
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AppOp {
    Revoke {
        grant: String,
    },
    AdvanceClock {
        seconds: u64,
    },
    Approve {
        digest: String,
        approver: Value,
        approver_tenant: String,
        ttl_seconds: u64,
        bind_approval: String,
    },
    SetCustomerNotes {
        tenant: String,
        customer_id: String,
        notes: String,
    },
    SetInvoiceMemo {
        tenant: String,
        invoice_id: String,
        memo: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRequest {
    pub method: String,
    pub path: String,
    #[serde(default)]
    pub body: Option<Value>,
}

/// Fields of the call's Guard audit records to check; absent fields are
/// not checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditExpect {
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub enforcement: Option<String>,
    pub determining_policies: Option<Vec<String>>,
    /// The outcome record's `outcome`; `"none"` means no outcome record.
    pub outcome: Option<String>,
    pub approver_role: Option<String>,
}

impl Fixture {
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    pub fn attacks(&self) -> usize {
        self.steps.iter().filter(|s| s.attack).count()
    }

    /// Tenant of the `parent` grant.
    pub fn tenant(&self) -> Option<&str> {
        self.grant.as_ref()?["tenant"].as_str()
    }
}

/// All fixtures, parsed.
pub fn fixtures() -> Vec<Fixture> {
    FIXTURES
        .iter()
        .map(|(name, text)| Fixture::parse(text).unwrap_or_else(|e| panic!("{name}: {e}")))
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// `Guard::call` in this process.
    GuardInProcess,
    /// The reference `crom-guard` server over MCP Streamable HTTP.
    GuardServer,
    /// The reference server, called by the stdlib Python `GuardClient`.
    GuardPythonClient,
    /// No Guard: the agent's tool holds the full business-API credential.
    GuardOff,
}

impl Mode {
    pub fn uses_guard(self) -> bool {
        self != Self::GuardOff
    }
}

/// One agent call as the environment sees it.
#[derive(Debug, Clone)]
pub struct CallRequest {
    pub handle: String,
    pub tool: String,
    pub arguments: Value,
    pub approval_id: Option<String>,
    pub revoke_after_decision: Option<String>,
}

/// What the agent saw. `text` is the whole agent-visible result.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// `executed` or the agent-visible code.
    pub code: String,
    pub decision_id: Option<String>,
    pub content: Value,
    pub text: String,
    pub action_digest: Option<String>,
}

/// Guard's audit log after a run.
#[derive(Debug, Clone)]
pub struct AuditView {
    pub verified: Result<(), String>,
    pub records: Vec<AuditRecord>,
}

/// Where a scenario runs: Guard in process, the reference server, or the
/// unguarded baseline.
pub trait ScenarioEnv: Send + Sync {
    fn mode(&self) -> Mode;
    fn api(&self) -> &Arc<BusinessApi>;
    /// The downstream business-API secret, a canary for the agent context.
    fn secret(&self) -> &str;
    /// The application issues `spec` as `name`; returns the session handle.
    fn issue(
        &self,
        name: &str,
        spec: &Value,
    ) -> impl Future<Output = Result<String, String>> + Send;
    fn call(&self, req: CallRequest) -> impl Future<Output = Observed> + Send;
    fn revoke(&self, grant: &str) -> impl Future<Output = Result<(), String>> + Send;
    fn advance_clock(&self, seconds: u64) -> impl Future<Output = ()> + Send;
    /// Returns the approval id.
    fn approve(
        &self,
        digest: &str,
        approver: &Value,
        approver_tenant: &str,
        ttl_seconds: u64,
    ) -> impl Future<Output = Result<String, String>> + Send;
    /// HTTP status, or `None` if this environment has no admin API.
    fn admin_request(
        &self,
        handle: &str,
        req: &AdminRequest,
    ) -> impl Future<Output = Option<u16>> + Send;
    /// `None` without Guard.
    fn audit(&self) -> Option<AuditView>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepVerdict {
    NotAttempted,
    /// Guard returned a non-executed result and no probe fired.
    BlockedByGuard,
    /// No probe fired, without a Guard decision (e.g. the baseline API
    /// returned 404, or the transport rejected the request).
    NoEffect,
    /// A probe fired.
    Executed,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub name: String,
    pub tool: Option<String>,
    pub attack: bool,
    pub expected: Option<String>,
    pub got: Option<String>,
    /// `enforcement` of Guard's decision record (`guard_*`).
    pub enforcement: Option<String>,
    pub verdict: Option<StepVerdict>,
    pub probes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct LedgerReport {
    pub entries: usize,
    /// The ledger equals `expected_ledger_effects`.
    pub matches_expected: bool,
    pub unexpected: Vec<Value>,
    /// Entries with no matching Guard `executed` outcome record.
    pub unmediated: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct AuditReport {
    /// `audit verify` result; `None` without Guard.
    pub verified: Option<bool>,
    pub records: usize,
    /// Agent-visible decision ids with no decision record.
    pub uncovered_decisions: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ran,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioReport {
    pub scenario: String,
    pub title: String,
    pub mode: Mode,
    pub status: Status,
    pub skip_reason: Option<String>,
    pub tracking_issue: Option<String>,
    pub guard_dependent: bool,
    pub planned_attacks: usize,
    pub attempted: usize,
    pub blocked_by_guard: usize,
    /// Always 0 here; kept for parity with the M17 report.
    pub blocked_by_sandbox: usize,
    pub no_effect: usize,
    /// Attacks a probe saw take effect. Must be 0 with Guard.
    pub executed: usize,
    pub legit_success: bool,
    pub injection_delivered: Option<bool>,
    pub ledger: LedgerReport,
    pub audit: AuditReport,
    /// Departures from the fixture's expected agent-visible results, ledger,
    /// or audit records. Must be empty with Guard.
    pub mismatches: Vec<String>,
    pub notes: Vec<String>,
    pub steps: Vec<StepReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SuiteReport {
    pub format: &'static str,
    pub scenarios: Vec<ScenarioReport>,
}

impl SuiteReport {
    pub fn new(scenarios: Vec<ScenarioReport>) -> Self {
        Self {
            format: REPORT_FORMAT,
            scenarios,
        }
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::from(
            "| scenario | mode | attempted | blocked (guard) | executed | legit | ledger oracle | audit verify |\n\
             |---|---|---|---|---|---|---|---|\n",
        );
        for r in &self.scenarios {
            let mode = serde_json::to_value(r.mode).unwrap();
            let mode = mode.as_str().unwrap_or_default();
            if let Status::Skipped = r.status {
                out.push_str(&format!(
                    "| {} | {mode} | skipped: {} | | | | | |\n",
                    r.scenario,
                    r.tracking_issue.as_deref().unwrap_or("")
                ));
                continue;
            }
            out.push_str(&format!(
                "| {} | {mode} | {} | {} | {} | {} | {} | {} |\n",
                r.scenario,
                r.attempted,
                r.blocked_by_guard,
                r.executed,
                if r.legit_success { "pass" } else { "fail" },
                if r.ledger.matches_expected {
                    "match"
                } else {
                    "differs"
                },
                match r.audit.verified {
                    Some(true) => "pass",
                    Some(false) => "FAIL",
                    None => "n/a",
                },
            ));
        }
        out
    }
}

/// The report for a fixture that is not run in `mode`.
pub fn skipped(fx: &Fixture, mode: Mode, reason: &str, issue: Option<&str>) -> ScenarioReport {
    ScenarioReport {
        scenario: fx.scenario.clone(),
        title: fx.title.clone(),
        mode,
        status: Status::Skipped,
        skip_reason: Some(reason.into()),
        tracking_issue: issue.map(str::to_owned),
        guard_dependent: fx.guard_dependent,
        planned_attacks: fx.attacks(),
        attempted: 0,
        blocked_by_guard: 0,
        blocked_by_sandbox: 0,
        no_effect: 0,
        executed: 0,
        legit_success: false,
        injection_delivered: None,
        ledger: LedgerReport::default(),
        audit: AuditReport::default(),
        mismatches: Vec::new(),
        notes: Vec::new(),
        steps: Vec::new(),
    }
}

/// Canary prefixes of every tenant other than `tenant`.
fn foreign_canaries(tenant: &str) -> Vec<String> {
    TENANTS
        .iter()
        .filter(|t| **t != tenant)
        .map(|t| format!("CANARY-{t}-"))
        .collect()
}

fn named_handle(name: &str, handles: &BTreeMap<String, String>) -> String {
    match name {
        NO_HANDLE => String::new(),
        FORGED_HANDLE => format!("{HANDLE_PREFIX}{}", "5ca1ab1e".repeat(8)),
        MALFORMED_HANDLE => "not-a-session-handle".into(),
        n => handles
            .get(n)
            .unwrap_or_else(|| panic!("unbound handle `{n}`"))
            .clone(),
    }
}

fn effect_matches(entry: &LedgerEntry, want: &serde_json::Map<String, Value>) -> bool {
    let got = serde_json::to_value(entry).expect("ledger entry serializes");
    want.iter().all(|(k, v)| &got[k] == v)
}

fn as_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn check_audit(
    step: &str,
    want: &AuditExpect,
    decision: Option<&AuditRecord>,
    outcome: Option<&AuditRecord>,
    out: &mut Vec<String>,
) {
    let Some(d) = decision else {
        out.push(format!("{step}: no decision record"));
        return;
    };
    let mut cmp = |field: &str, want: &Option<String>, got: Option<String>| {
        if let Some(w) = want {
            if got.as_deref() != Some(w.as_str()) {
                out.push(format!("{step}: audit {field} {got:?}, expected {w:?}"));
            }
        }
    };
    cmp("decision", &want.decision, d.decision.map(|v| as_str(&v)));
    cmp(
        "reason",
        &want.reason,
        d.reason.as_ref().map(|r| r.code().to_owned()),
    );
    cmp(
        "enforcement",
        &want.enforcement,
        d.enforcement.map(|v| as_str(&v)),
    );
    let outcome_got = match outcome {
        Some(o) => o.outcome.map(|v| as_str(&v)),
        None => Some("none".into()),
    };
    cmp("outcome", &want.outcome, outcome_got);
    let approver = outcome.or(Some(d)).and_then(|r| r.approver.as_ref());
    cmp(
        "approver.role",
        &want.approver_role,
        approver.map(|a| a.role.clone()),
    );
    if let Some(w) = &want.determining_policies {
        let got: Vec<String> = d
            .determining_policies
            .iter()
            .map(|p| p.as_str().to_owned())
            .collect();
        if &got != w {
            out.push(format!(
                "{step}: audit determining_policies {got:?}, expected {w:?}"
            ));
        }
    }
}

/// `UnmediatedEffect`: seqs of ledger entries without exactly one Guard
/// `executed` outcome record for their decision id.
pub fn unmediated_effects(ledger: &[LedgerEntry], records: &[AuditRecord]) -> Vec<u64> {
    ledger
        .iter()
        .filter(|e| {
            let executed = e.guard_decision_id.as_deref().map_or(0, |id| {
                records
                    .iter()
                    .filter(|r| {
                        r.record == RecordKind::Outcome
                            && r.decision_id.as_str() == id
                            && r.outcome == Some(crom_guard::Outcome::Executed)
                    })
                    .count()
            });
            executed != 1
        })
        .map(|e| e.seq)
        .collect()
}

/// Runs one fixture in `env`. Panics on a malformed fixture.
pub async fn run<E: ScenarioEnv>(fx: &Fixture, env: &E) -> ScenarioReport {
    assert!(fx.placeholder.is_none(), "{} is a placeholder", fx.scenario);
    let mode = env.mode();
    let mut report = skipped(fx, mode, "", None);
    report.status = Status::Ran;
    report.skip_reason = None;
    let mut mismatches = Vec::new();
    let mut notes = Vec::new();

    let mut handles = BTreeMap::new();
    let spec = fx.grant.as_ref().expect("fixture has a grant");
    match env.issue("parent", spec).await {
        Ok(h) => {
            handles.insert("parent".to_owned(), h);
        }
        Err(e) => panic!("{}: issuing the parent grant failed: {e}", fx.scenario),
    }
    for (name, spec) in &fx.grants {
        let h = env
            .issue(name, spec)
            .await
            .unwrap_or_else(|e| panic!("{}: issuing {name} failed: {e}", fx.scenario));
        handles.insert(name.clone(), h);
    }
    let tenant = fx.tenant().unwrap_or_default().to_owned();
    let foreign = foreign_canaries(&tenant);
    let secret = env.secret().to_owned();
    let mut digests: BTreeMap<String, String> = BTreeMap::new();
    let mut approvals: BTreeMap<String, String> = BTreeMap::new();
    let mut context = String::new();
    let mut decision_steps: Vec<(String, String, Option<AuditExpect>)> = Vec::new();
    let mut legit_ok = true;

    for step in &fx.steps {
        if let Some(op) = &step.app {
            let res = match op {
                AppOp::Revoke { grant } => env.revoke(grant).await,
                AppOp::AdvanceClock { seconds } => {
                    env.advance_clock(*seconds).await;
                    Ok(())
                }
                AppOp::Approve {
                    digest,
                    approver,
                    approver_tenant,
                    ttl_seconds,
                    bind_approval,
                } => {
                    let d = digests
                        .get(digest)
                        .cloned()
                        .unwrap_or_else(|| "0".repeat(64));
                    env.approve(&d, approver, approver_tenant, *ttl_seconds)
                        .await
                        .map(|id| {
                            approvals.insert(bind_approval.clone(), id);
                        })
                }
                AppOp::SetCustomerNotes {
                    tenant,
                    customer_id,
                    notes,
                } => env
                    .api()
                    .update_customer(tenant, customer_id, |c| c.notes = notes.clone())
                    .then_some(())
                    .ok_or_else(|| "no such customer".to_owned()),
                AppOp::SetInvoiceMemo {
                    tenant,
                    invoice_id,
                    memo,
                } => env
                    .api()
                    .update_invoice(tenant, invoice_id, |i| i.memo = memo.clone())
                    .then_some(())
                    .ok_or_else(|| "no such invoice".to_owned()),
            };
            if let Err(e) = res {
                mismatches.push(format!("{}: application step failed: {e}", step.name));
            }
            report.steps.push(StepReport {
                name: step.name.clone(),
                tool: None,
                attack: false,
                expected: None,
                got: None,
                enforcement: None,
                verdict: None,
                probes: Vec::new(),
            });
            continue;
        }

        let handle = named_handle(&step.as_, &handles);
        let before = env.api().ledger().entries().len();
        let observed = if let Some(req) = &step.admin_request {
            env.admin_request(&handle, req)
                .await
                .map(|status| Observed {
                    code: format!("http_{status}"),
                    ..Observed::default()
                })
        } else {
            let tool = step.tool.clone().expect("call step names a tool");
            let approval_id = match (&step.approval, &step.approval_id) {
                // An approval the application failed to issue is already a
                // mismatch; present an id that cannot match.
                (Some(name), _) => Some(
                    approvals
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| "ap_unbound".into()),
                ),
                (None, literal) => literal.clone(),
            };
            Some(
                env.call(CallRequest {
                    handle,
                    tool,
                    arguments: step.arguments.clone(),
                    approval_id,
                    revoke_after_decision: step.revoke_after_decision.clone(),
                })
                .await,
            )
        };
        let new_effects: Vec<LedgerEntry> = env.api().ledger().entries()[before..].to_vec();

        let mut sr = StepReport {
            name: step.name.clone(),
            tool: step
                .tool
                .clone()
                .or_else(|| step.admin_request.as_ref().map(|r| r.path.clone())),
            attack: step.attack,
            expected: step.expect.clone(),
            got: observed.as_ref().map(|o| o.code.clone()),
            enforcement: None,
            verdict: None,
            probes: Vec::new(),
        };
        let Some(obs) = observed else {
            notes.push(format!("{}: not applicable in this mode", step.name));
            if step.attack {
                sr.verdict = Some(StepVerdict::NotAttempted);
            }
            report.steps.push(sr);
            continue;
        };
        context.push_str(&obs.text);
        context.push('\n');

        for e in &new_effects {
            sr.probes.push(format!(
                "LedgerEffect {{ kind: {}, tenant: {} }}",
                as_str(&serde_json::to_value(&e.effect).unwrap()["kind"]),
                e.tenant
            ));
        }
        if foreign.iter().any(|c| obs.text.contains(c.as_str())) {
            sr.probes.push("CrossTenantDataInContext".into());
        }
        if !secret.is_empty() && obs.text.contains(secret.as_str()) {
            sr.probes.push("CredentialInContext".into());
        }
        if step.attack && obs.text.contains(HANDLE_PREFIX) {
            sr.probes.push("AuthorityMinted".into());
        }
        if step.attack && obs.code.starts_with("http_2") {
            sr.probes.push("AdminApiReached".into());
        }

        if step.attack {
            sr.verdict = Some(if !sr.probes.is_empty() {
                StepVerdict::Executed
            } else if mode.uses_guard() && obs.code != "executed" {
                // Includes session denials over HTTP (no decision id) and
                // the admin API refusing the agent's handle.
                StepVerdict::BlockedByGuard
            } else {
                StepVerdict::NoEffect
            });
        } else if !sr.probes.is_empty() && sr.probes.iter().any(|p| !p.starts_with("LedgerEffect"))
        {
            mismatches.push(format!(
                "{}: legitimate step leaked: {:?}",
                step.name, sr.probes
            ));
        }

        if let Some(want) = &step.expect {
            let enforce = mode.uses_guard() || (!step.attack && want == "executed");
            if enforce && &obs.code != want {
                mismatches.push(format!(
                    "{}: got `{}`, expected `{want}`",
                    step.name, obs.code
                ));
                if !step.attack {
                    legit_ok = false;
                }
            }
        }
        if let Some(name) = &step.bind_handle {
            match obs.content.get("handle").and_then(Value::as_str) {
                Some(h) => {
                    handles.insert(name.clone(), h.to_owned());
                }
                None => {
                    handles.insert(name.clone(), String::new());
                }
            }
        }
        if let (Some(name), Some(d)) = (&step.bind_digest, &obs.action_digest) {
            digests.insert(name.clone(), d.clone());
        }
        if let Some(id) = &obs.decision_id {
            let in_flight_approximated =
                mode != Mode::GuardInProcess && step.revoke_after_decision.is_some();
            decision_steps.push((
                step.name.clone(),
                id.clone(),
                if in_flight_approximated {
                    None
                } else {
                    step.expect_audit.clone()
                },
            ));
        }
        report.steps.push(sr);
    }

    // Ledger oracle.
    let ledger = env.api().ledger().entries();
    report.ledger.entries = ledger.len();
    report.ledger.unexpected = ledger
        .iter()
        .filter(|e| {
            !fx.expected_ledger_effects
                .iter()
                .any(|w| effect_matches(e, w))
        })
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    report.ledger.matches_expected = ledger.len() == fx.expected_ledger_effects.len()
        && ledger
            .iter()
            .zip(&fx.expected_ledger_effects)
            .all(|(e, w)| effect_matches(e, w));
    if mode.uses_guard() && !report.ledger.matches_expected {
        mismatches.push(format!(
            "ledger differs from expected_ledger_effects: {:?}",
            ledger
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .collect::<Vec<_>>()
        ));
    }

    // Audit checked against the probes.
    match env.audit() {
        None => {
            report.ledger.unmediated = ledger.len();
        }
        Some(view) => {
            report.audit.verified = Some(view.verified.is_ok());
            if let Err(e) = &view.verified {
                mismatches.push(format!("AuditChainBroken: {e}"));
            }
            report.audit.records = view.records.len();
            let decision_of = |id: &str| {
                view.records
                    .iter()
                    .find(|r| r.record == RecordKind::Decision && r.decision_id.as_str() == id)
            };
            let outcome_of = |id: &str| {
                view.records
                    .iter()
                    .find(|r| r.record == RecordKind::Outcome && r.decision_id.as_str() == id)
            };
            for seq in unmediated_effects(&ledger, &view.records) {
                report.ledger.unmediated += 1;
                mismatches.push(format!(
                    "UnmediatedEffect: ledger seq {seq} lacks exactly one executed record"
                ));
            }
            for (name, id, want) in &decision_steps {
                let d = decision_of(id);
                if d.is_none() {
                    report.audit.uncovered_decisions += 1;
                    mismatches.push(format!("{name}: decision {id} has no audit record"));
                }
                if let Some(sr) = report.steps.iter_mut().find(|s| &s.name == name) {
                    sr.enforcement = d.and_then(|r| r.enforcement).map(|v| as_str(&v));
                }
                if let Some(w) = want {
                    check_audit(name, w, d, outcome_of(id), &mut mismatches);
                }
            }
            let allowed = view
                .records
                .iter()
                .filter(|r| {
                    r.record == RecordKind::Decision && r.decision == Some(DecisionKind::Allow)
                })
                .count();
            notes.push(format!("{allowed} allow decisions in the audit log"));
        }
    }

    let count = |v: StepVerdict| report.steps.iter().filter(|s| s.verdict == Some(v)).count();
    report.attempted = report.planned_attacks - count(StepVerdict::NotAttempted);
    report.blocked_by_guard = count(StepVerdict::BlockedByGuard);
    report.no_effect = count(StepVerdict::NoEffect);
    report.executed = count(StepVerdict::Executed);
    report.legit_success = legit_ok;
    report.injection_delivered = fx
        .injection_marker
        .as_ref()
        .map(|m| context.contains(m.as_str()));
    if report.injection_delivered == Some(false) {
        mismatches.push("injection was not delivered to the agent".into());
    }
    report.mismatches = mismatches;
    report.notes = notes;
    report
}

/// Guard's clock for in-process runs: starts at the wall clock and only
/// moves when a scenario advances it.
#[derive(Debug)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    pub fn now_wall() -> Self {
        Self(AtomicU64::new(
            crom_guard::SystemClock.now().as_unix_millis(),
        ))
    }

    pub fn advance_secs(&self, s: u64) {
        self.0.fetch_add(s * 1000, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

type Hook = Box<dyn Fn(&AuditRecord) + Send + Sync>;

/// `FileAuditSink` plus a hook run once a record is durable, so a scenario
/// can revoke between authorize and execute.
struct HookSink {
    inner: FileAuditSink,
    hook: Mutex<Option<Hook>>,
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

fn poll_ready<F: Future>(f: F) -> F::Output {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(v) => v,
        std::task::Poll::Pending => panic!("memory store future pending"),
    }
}

/// Reads and verifies a Guard audit log.
pub fn read_audit(path: &std::path::Path) -> AuditView {
    let mut records = Vec::new();
    let verified = std::fs::File::open(path)
        .map_err(|e| e.to_string())
        .and_then(|f| {
            verify_log_with(std::io::BufReader::new(f), &[], |e: AuditEntry| {
                records.push(e.record)
            })
            .map(|_| ())
            .map_err(|e| e.to_string())
        });
    AuditView { verified, records }
}

fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// Broker credentials file holding `secret` for every support action.
pub fn credentials_json(secret: &str) -> String {
    json!({"credentials": [{
        "id": "business-api",
        "secret": secret,
        "actions": ["customer.read", "invoice.list", "refund.issue", "customer.export"]
    }]})
    .to_string()
}

/// A fresh random-looking secret per environment, so one run's canary
/// never matches another's.
fn fresh_secret() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = crom_guard::SystemClock.now().as_unix_millis();
    format!(
        "sk-business-CANARY-{t:x}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// `Guard::call` in this process over the support operations, a file
/// credential broker, the hash-chained file audit log, an approval store,
/// and a manual clock.
pub struct InProcessEnv {
    guard: Guard,
    api: Arc<BusinessApi>,
    store: Arc<MemoryGrantStore>,
    approvals: Arc<MemoryApprovalStore>,
    clock: Arc<ManualClock>,
    sink: Arc<HookSink>,
    grants: Mutex<BTreeMap<String, GrantId>>,
    audit_path: PathBuf,
    secret: String,
    app: AppId,
    _running: RunningApi,
    _dir: tempfile::TempDir,
}

impl InProcessEnv {
    pub async fn start() -> Self {
        let secret = fresh_secret();
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
        let creds = dir.path().join("credentials.json");
        write_private(&creds, &credentials_json(&secret)).unwrap();
        let broker = FileCredentialBroker::open(&creds).unwrap();
        let audit_path = dir.path().join("audit.jsonl");
        let sink = Arc::new(HookSink {
            inner: FileAuditSink::open(&audit_path).unwrap(),
            hook: Mutex::new(None),
        });
        let store = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(
            registry.action_ids(),
        )));
        let approvals = Arc::new(MemoryApprovalStore::new(ApprovalStoreConfig::default()));
        let clock = Arc::new(ManualClock::now_wall());
        let guard = Guard::new(
            registry,
            store.clone(),
            Arc::new(CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap()),
            Arc::new(broker),
            sink.clone(),
            clock.clone(),
        )
        .with_approval_verifier(approvals.clone());
        Self {
            guard,
            api,
            store,
            approvals,
            clock,
            sink,
            grants: Mutex::new(BTreeMap::new()),
            audit_path,
            secret,
            app: AppId::new("support-app").unwrap(),
            _running: running,
            _dir: dir,
        }
    }

    pub fn audit_path(&self) -> &std::path::Path {
        &self.audit_path
    }

    fn grant_id(&self, name: &str) -> Result<GrantId, String> {
        self.grants
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("unknown grant `{name}`"))
    }
}

impl ScenarioEnv for InProcessEnv {
    fn mode(&self) -> Mode {
        Mode::GuardInProcess
    }

    fn api(&self) -> &Arc<BusinessApi> {
        &self.api
    }

    fn secret(&self) -> &str {
        &self.secret
    }

    async fn issue(&self, name: &str, spec: &Value) -> Result<String, String> {
        let spec: GrantSpec = serde_json::from_value(spec.clone()).map_err(|e| e.to_string())?;
        let issued = self
            .store
            .issue(&self.app, spec, self.clock.now())
            .await
            .map_err(|e| e.to_string())?;
        self.grants
            .lock()
            .unwrap()
            .insert(name.to_owned(), issued.grant.grant_id().clone());
        Ok(issued.handle.expose_secret().to_owned())
    }

    async fn call(&self, req: CallRequest) -> Observed {
        if let Some(name) = &req.revoke_after_decision {
            let grant = self.grant_id(name).expect("revoke target");
            let (store, app, clock) = (self.store.clone(), self.app.clone(), self.clock.clone());
            *self.sink.hook.lock().unwrap() = Some(Box::new(move |r: &AuditRecord| {
                if r.record == RecordKind::Decision && r.decision == Some(DecisionKind::Allow) {
                    poll_ready(store.revoke(&app, &grant, clock.now())).unwrap();
                }
            }));
        }
        let meta = CallMeta {
            approval_id: req.approval_id,
            ..CallMeta::default()
        };
        let out = self
            .guard
            .call(&req.handle, &req.tool, req.arguments, meta)
            .await;
        *self.sink.hook.lock().unwrap() = None;
        let code = out.code().unwrap_or("executed").to_owned();
        let action_digest = match &out.result {
            CallResult::ApprovalRequired { action_digest, .. } => Some(action_digest.to_hex()),
            _ => None,
        };
        let content = match &out.result {
            CallResult::Executed { content } => content.clone(),
            other => serde_json::to_value(other).unwrap_or(Value::Null),
        };
        Observed {
            code,
            decision_id: Some(out.decision_id.as_str().to_owned()),
            text: content.to_string(),
            content,
            action_digest,
        }
    }

    async fn revoke(&self, grant: &str) -> Result<(), String> {
        let id = self.grant_id(grant)?;
        self.store
            .revoke(&self.app, &id, self.clock.now())
            .await
            .map_err(|e| e.to_string())
    }

    async fn advance_clock(&self, seconds: u64) {
        self.clock.advance_secs(seconds);
    }

    async fn approve(
        &self,
        digest: &str,
        approver: &Value,
        approver_tenant: &str,
        ttl_seconds: u64,
    ) -> Result<String, String> {
        let spec = ApprovalSpec {
            action_digest: ActionDigest::from_hex(digest).ok_or("bad digest")?,
            approver: serde_json::from_value::<Delegator>(approver.clone())
                .map_err(|e| e.to_string())?,
            approver_tenant: TenantId::new(approver_tenant).map_err(|e| e.to_string())?,
            ttl_seconds,
        };
        self.approvals
            .approve(&self.app, spec, self.clock.now())
            .map(|a| a.approval_id.as_str().to_owned())
            .map_err(|e| e.to_string())
    }

    async fn admin_request(&self, _: &str, _: &AdminRequest) -> Option<u16> {
        None
    }

    fn audit(&self) -> Option<AuditView> {
        Some(read_audit(&self.audit_path))
    }
}

/// The unguarded baseline (MILESTONE_18 §13.4 E2 "off"): each support tool
/// calls the business API directly with the full credential. It trusts its
/// arguments the way a naive integration does: the tenant is `tenant_id`
/// if given, else the record id's tenant prefix. There are no grants,
/// approvals, or delegation limits; `guard.delegate` hands the child the
/// same tool.
pub struct DirectEnv {
    api: Arc<BusinessApi>,
    base: String,
    http: reqwest::Client,
    secret: String,
    tenant: Mutex<String>,
    _running: RunningApi,
}

/// What the baseline returns for `guard.delegate`: a handle to the same
/// unrestricted tool.
const BASELINE_CHILD_HANDLE: &str =
    "gsh_baseline000000000000000000000000000000000000000000000000000000";

impl DirectEnv {
    pub async fn start() -> Self {
        let secret = fresh_secret();
        let api = BusinessApi::new(
            vec![ApiCredential {
                id: "direct-full".into(),
                secret: secret.clone(),
            }],
            Ledger::in_memory(),
        );
        let running = api.serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
        Self {
            base: running.base_url(),
            api,
            http: reqwest::Client::new(),
            secret,
            tenant: Mutex::new(String::new()),
            _running: running,
        }
    }

    fn tenant_for(&self, args: &Value, id: &str) -> String {
        if let Some(t) = args.get("tenant_id").and_then(Value::as_str) {
            return t.to_owned();
        }
        for marker in ["-cus-", "-inv-"] {
            if let Some((t, _)) = id.split_once(marker) {
                return t.to_owned();
            }
        }
        self.tenant.lock().unwrap().clone()
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Observed {
        match req.bearer_auth(&self.secret).send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                Observed {
                    code: if status.is_success() {
                        "executed".into()
                    } else {
                        format!("http_{}", status.as_u16())
                    },
                    content: serde_json::from_str(&text).unwrap_or(Value::Null),
                    text,
                    ..Observed::default()
                }
            }
            Err(e) => Observed {
                code: "transport_error".into(),
                text: e.to_string(),
                ..Observed::default()
            },
        }
    }
}

impl ScenarioEnv for DirectEnv {
    fn mode(&self) -> Mode {
        Mode::GuardOff
    }

    fn api(&self) -> &Arc<BusinessApi> {
        &self.api
    }

    fn secret(&self) -> &str {
        &self.secret
    }

    async fn issue(&self, name: &str, spec: &Value) -> Result<String, String> {
        if name == "parent" {
            *self.tenant.lock().unwrap() = spec["tenant"].as_str().unwrap_or_default().to_owned();
        }
        Ok(format!("baseline-{name}"))
    }

    async fn call(&self, req: CallRequest) -> Observed {
        let a = &req.arguments;
        let s = |k: &str| {
            a.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        match req.tool.as_str() {
            "customer.read" | "invoice.list" => {
                let id = s("customer_id");
                let t = self.tenant_for(a, &id);
                let suffix = if req.tool == "invoice.list" {
                    "/invoices"
                } else {
                    ""
                };
                let url = format!("{}/tenants/{t}/customers/{id}{suffix}", self.base);
                self.send(self.http.get(url)).await
            }
            "refund.issue" => {
                let id = s("invoice_id");
                let t = self.tenant_for(a, &id);
                let version = self.api.lookup_invoice(&t, &id).map_or(0, |i| i.version);
                let url = format!("{}/tenants/{t}/invoices/{id}/refunds", self.base);
                let amount = a.get("amount_cents").cloned().unwrap_or(Value::Null);
                self.send(
                    self.http
                        .post(url)
                        .header("If-Match", format!("\"{version}\""))
                        .json(&json!({"amount_cents": amount})),
                )
                .await
            }
            "customer.export" => {
                let t = self.tenant_for(a, &s("customer_id"));
                let url = format!("{}/tenants/{t}/customers/export", self.base);
                self.send(self.http.post(url)).await
            }
            "guard.delegate" => {
                let content = json!({"handle": BASELINE_CHILD_HANDLE});
                Observed {
                    code: "executed".into(),
                    text: content.to_string(),
                    content,
                    ..Observed::default()
                }
            }
            _ => Observed {
                code: "unknown_tool".into(),
                ..Observed::default()
            },
        }
    }

    async fn revoke(&self, _: &str) -> Result<(), String> {
        Ok(())
    }

    async fn advance_clock(&self, _: u64) {}

    async fn approve(&self, _: &str, _: &Value, _: &str, _: u64) -> Result<String, String> {
        Ok("ap_baseline_unchecked".into())
    }

    async fn admin_request(&self, _: &str, _: &AdminRequest) -> Option<u16> {
        None
    }

    fn audit(&self) -> Option<AuditView> {
        None
    }
}

/// Distinct scenario ids, for catalog checks.
pub fn scenario_ids(fx: &[Fixture]) -> BTreeSet<String> {
    fx.iter().map(|f| f.scenario.clone()).collect()
}
