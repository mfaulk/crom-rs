use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crom_core::events::{EnforcementLayer, SecurityDecision, SecurityOutcome};
use crom_sdk::SdkEvent;
use serde_json::{json, Value};

use crate::agent::AgentRun;

/// The business API's effect ledger (JSONL), the independent oracle.
pub fn read_ledger(path: &Path) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("ledger line"))
        .collect()
}

pub fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    read_ledger(path)
}

/// One matrix step: what was attempted, by whom, what was expected, and
/// what Crom, Guard and the ledger show.
#[derive(Debug, Clone)]
pub struct Row {
    pub row: &'static str,
    pub step: String,
    pub actor: &'static str,
    pub expected: String,
    pub observed: String,
    pub effects_expected: Vec<String>,
    pub effects_observed: Vec<String>,
    /// Every new ledger entry carries this step's Guard decision id.
    pub effects_attributed: bool,
}

impl Row {
    pub fn pass(&self) -> bool {
        self.expected == self.observed
            && self.effects_expected == self.effects_observed
            && self.effects_attributed
    }

    fn effects_text(effects: &[String]) -> String {
        if effects.is_empty() {
            "-".into()
        } else {
            effects.join(",")
        }
    }

    /// Deterministic line for the matrix snapshot (no ids or timings).
    pub fn line(&self) -> String {
        format!(
            "{:<4} {:<28} {:<11} {:<54} expected={:<27} observed={:<27} ledger={}",
            if self.pass() { "PASS" } else { "FAIL" },
            self.row,
            self.actor,
            self.step,
            self.expected,
            self.observed,
            Self::effects_text(&self.effects_observed),
        )
    }

    pub fn to_json(&self) -> Value {
        json!({
            "row": self.row,
            "step": self.step,
            "actor": self.actor,
            "expected": self.expected,
            "observed": self.observed,
            "ledger_expected": self.effects_expected,
            "ledger_observed": self.effects_observed,
            "ledger_attributed": self.effects_attributed,
            "pass": self.pass(),
        })
    }
}

/// What happened to one Crom tool call: Guard's verdict when Guard decided,
/// otherwise the Crom layer that stopped it.
pub fn observe_call(
    run: &AgentRun,
    tool_use_id: &str,
    approvals: usize,
) -> (String, Option<String>) {
    let decision: Option<&SecurityDecision> = run.events.iter().find_map(|e| match e {
        SdkEvent::SecurityDecision(d) if d.tool_use_id == tool_use_id => Some(d),
        _ => None,
    });
    let completed_error = run.events.iter().find_map(|e| match e {
        SdkEvent::ToolCallCompleted(c) if c.tool_use_id == tool_use_id => Some(c.is_error),
        _ => None,
    });
    let base = match decision {
        None => "not_called".to_string(),
        Some(d) if d.enforcement == EnforcementLayer::Guard => match d.outcome {
            SecurityOutcome::Executed => "executed".into(),
            _ => d.reason.clone().unwrap_or_else(|| "blocked".into()),
        },
        Some(d) if d.outcome == SecurityOutcome::Blocked => "crom_denied".into(),
        Some(_) => match (run.results.get(tool_use_id), completed_error) {
            (Some(out), _) if out.failed() => "tool_error".into(),
            (Some(_), _) | (None, Some(false)) => "tool_ok".into(),
            (None, Some(true)) => "tool_error".into(),
            (None, None) => "no_result".into(),
        },
    };
    let observed = if approvals > 0 && base != "approval_required" {
        format!("approval_required->{base}")
    } else {
        base
    };
    (observed, decision.and_then(|d| d.guard_decision_id.clone()))
}

/// A Crom run that failed before calling any tool (Guard refused its handle
/// at tool discovery), labelled with Guard's session error code for that
/// handle, which Crom does not surface.
pub fn observe_refused_run(run: &AgentRun, guard_code: &str) -> String {
    let called = run
        .events
        .iter()
        .any(|e| matches!(e, SdkEvent::ToolCallStarted(_)));
    if called || run.status != crom_sdk::AgentRunStatus::Failed {
        return "ran".into();
    }
    format!("refused({guard_code})")
}

pub fn effect_kinds(entries: &[Value]) -> Vec<String> {
    entries
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or("?").to_owned())
        .collect()
}

pub fn attributed(entries: &[Value], decision_id: Option<&str>) -> bool {
    entries
        .iter()
        .all(|e| decision_id.is_some() && e["guard_decision_id"].as_str() == decision_id)
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

impl Check {
    pub fn new(name: impl Into<String>, pass: bool, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            pass,
            detail: detail.into(),
        }
    }
}

/// Claim 5 (MILESTONE_18 §18): every ledger effect has exactly one Guard
/// `executed` outcome record with its decision id.
pub fn check_effects_audited(ledger: &[Value], audit: &[Value]) -> Check {
    let mut bad = Vec::new();
    for e in ledger {
        let id = e["guard_decision_id"].as_str();
        let n = audit
            .iter()
            .filter(|r| {
                id.is_some()
                    && r["decision_id"].as_str() == id
                    && r["record"] == "outcome"
                    && r["outcome"] == "executed"
            })
            .count();
        if n != 1 {
            bad.push(format!("{}:{n}", e["kind"].as_str().unwrap_or("?")));
        }
    }
    Check::new(
        "every ledger effect has exactly one Guard executed record",
        bad.is_empty(),
        if bad.is_empty() {
            format!("{} effects", ledger.len())
        } else {
            format!("unmatched: {}", bad.join(" "))
        },
    )
}

/// Counts of Guard audit records by decision and outcome.
pub fn audit_summary(audit: &[Value]) -> Value {
    let mut decisions = std::collections::BTreeMap::<String, u64>::new();
    let mut outcomes = std::collections::BTreeMap::<String, u64>::new();
    for r in audit {
        if let Some(d) = r["decision"].as_str() {
            *decisions.entry(d.to_owned()).or_default() += 1;
        }
        if r["record"] == "outcome" {
            if let Some(o) = r["outcome"].as_str() {
                *outcomes.entry(o.to_owned()).or_default() += 1;
            }
        }
    }
    json!({"records": audit.len(), "decisions": decisions, "outcomes": outcomes})
}

pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(files_under(&p));
        } else {
            out.push(p);
        }
    }
    out
}

/// Names of the files under `paths` that contain any of `needles`. The
/// needles themselves are never printed.
pub fn files_containing(paths: &[PathBuf], needles: &[String]) -> Vec<String> {
    let mut hits = Vec::new();
    for p in paths {
        let files = if p.is_dir() {
            files_under(p)
        } else {
            vec![p.clone()]
        };
        for f in files {
            let Ok(bytes) = std::fs::read(&f) else {
                continue;
            };
            let text = String::from_utf8_lossy(&bytes);
            if needles
                .iter()
                .any(|n| !n.is_empty() && text.contains(n.as_str()))
            {
                hits.push(f.display().to_string());
            }
        }
    }
    hits
}

/// Canaries of every record outside the ticket's grant (other tenants and
/// other acme customers); none may reach the agent.
pub fn out_of_scope_canaries(allowed_records: &[&str]) -> Vec<String> {
    let data = crom_guard_testkit::seed();
    let customers = data.customers.iter().map(|c| (&c.tenant, &c.id));
    let invoices = data.invoices.iter().map(|i| (&i.tenant, &i.id));
    customers
        .chain(invoices)
        .filter(|(_, id)| !allowed_records.contains(&id.as_str()))
        .map(|(tenant, id)| crom_guard_testkit::canary(tenant, id))
        .collect()
}

pub fn print_matrix(rows: &[Row]) {
    println!("\nMILESTONE_18 §14.2 demonstration matrix");
    for r in rows {
        println!("{}", r.line());
    }
}

pub fn print_checks(checks: &[Check]) {
    println!("\nEvidence checks");
    for c in checks {
        println!(
            "{}  {} ({})",
            if c.pass { "PASS" } else { "FAIL" },
            c.name,
            c.detail
        );
    }
}

pub fn checks_json(checks: &[Check]) -> Value {
    Value::Array(
        checks
            .iter()
            .map(|c| json!({"check": c.name, "pass": c.pass, "detail": c.detail}))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(expected: &str, observed: &str, effects: &[&str]) -> Row {
        Row {
            row: "r",
            step: "s".into(),
            actor: "crom",
            expected: expected.into(),
            observed: observed.into(),
            effects_expected: effects.iter().map(|s| s.to_string()).collect(),
            effects_observed: effects.iter().map(|s| s.to_string()).collect(),
            effects_attributed: true,
        }
    }

    /// A row passes only when the outcome, the ledger effects and their attribution all match.
    #[test]
    fn row_passes_only_on_full_match() {
        assert!(row("executed", "executed", &["refund"]).pass());
        assert!(!row("executed", "policy_deny", &[]).pass());
        let mut r = row("executed", "executed", &["refund"]);
        r.effects_observed.push("refund".into());
        assert!(!r.pass());
        let mut r = row("executed", "executed", &["refund"]);
        r.effects_attributed = false;
        assert!(!r.pass());
    }

    /// An effect must carry the step's Guard decision id; with no decision id nothing is attributed.
    #[test]
    fn attribution_requires_matching_decision_id() {
        let e = vec![json!({"kind": "refund", "guard_decision_id": "d1"})];
        assert!(attributed(&e, Some("d1")));
        assert!(!attributed(&e, Some("d2")));
        assert!(!attributed(&e, None));
        assert!(attributed(&[], None));
    }

    /// The audit check fails for an effect with no executed record or with two.
    #[test]
    fn effects_audited_needs_exactly_one_executed_record() {
        let ledger = vec![json!({"kind": "refund", "guard_decision_id": "d1"})];
        let rec = json!({"decision_id": "d1", "record": "outcome", "outcome": "executed"});
        assert!(check_effects_audited(&ledger, std::slice::from_ref(&rec)).pass);
        assert!(!check_effects_audited(&ledger, &[]).pass);
        assert!(!check_effects_audited(&ledger, &[rec.clone(), rec]).pass);
    }

    /// Canaries of the granted records are excluded; every other record's canary is included.
    #[test]
    fn out_of_scope_canaries_exclude_granted_records() {
        let c = out_of_scope_canaries(&["acme-cus-000", "acme-inv-000"]);
        assert!(!c.contains(&crom_guard_testkit::canary("acme", "acme-cus-000")));
        assert!(c.contains(&crom_guard_testkit::canary("acme", "acme-cus-001")));
        assert!(c.contains(&crom_guard_testkit::canary("globex", "globex-inv-000")));
    }

    /// The leak scan reports the file that contains a needle and never the needle.
    #[test]
    fn files_containing_reports_paths_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.log"), "x sk-secret y").unwrap();
        std::fs::write(dir.path().join("b.log"), "clean").unwrap();
        let hits = files_containing(&[dir.path().to_path_buf()], &["sk-secret".into()]);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].ends_with("a.log"));
    }
}
