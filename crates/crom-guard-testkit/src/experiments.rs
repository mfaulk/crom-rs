//! MILESTONE_18 §13.4 experiments E1, E2, E4a, E5 (#363). Each experiment
//! writes one `guard-experiments/v1` report; `crom eval scoreboard
//! --guard-report` renders them. Numbers are only ever produced by a run:
//! nothing here hard-codes a result.

use std::collections::BTreeMap;
use std::path::Path;

use crom_guard::{AuditRecord, CedarPdp, PolicyDecisionPoint, RecordKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::scenario::{Fixture, ScenarioReport, FIXTURES, POLICIES, SCHEMA};

/// Report format of every experiment report.
pub const EXPERIMENTS_FORMAT: &str = "guard-experiments/v1";

/// The E1 expected table (`fixtures/experiments/e1-matrix.json`).
pub const E1_MATRIX: &str = include_str!("../fixtures/experiments/e1-matrix.json");

/// Reproducibility metadata (MILESTONE_18 §18 "Reproducibility").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReproMeta {
    /// `git rev-parse HEAD` of the checkout, else `$GITHUB_SHA`.
    pub commit: Option<String>,
    /// Tracked files differ from `commit`.
    pub dirty: Option<bool>,
    /// Hex SHA-256 Guard records for the support policy set and schema.
    pub policy_set_hash: String,
    pub schema_hash: String,
    /// SHA-256 over every scenario fixture and the E1 matrix (name, then
    /// bytes), i.e. every grant these runs issue.
    pub grant_fixtures_hash: String,
    pub sandbox: Value,
    /// Model provider and model; `null` for scripted runs.
    pub provider: Option<String>,
    pub model: Option<String>,
    /// `debug` or `release`.
    pub build: String,
    pub machine: Machine,
    /// The command that produced the report.
    pub command: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub os: String,
    pub arch: String,
    pub kernel: Option<String>,
    pub cpu: Option<String>,
    pub cpus: usize,
}

impl Machine {
    pub fn current() -> Self {
        let read = |p: &str| std::fs::read_to_string(p).ok();
        Self {
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            kernel: read("/proc/sys/kernel/osrelease").map(|s| s.trim().to_owned()),
            cpu: read("/proc/cpuinfo").and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("model name"))
                    .and_then(|l| l.split_once(':'))
                    .map(|(_, v)| v.trim().to_owned())
            }),
            cpus: std::thread::available_parallelism().map_or(0, |n| n.get()),
        }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn sha256_hex(parts: &[&[u8]]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

/// SHA-256 over the scenario fixtures and the E1 matrix.
pub fn grant_fixtures_hash() -> String {
    let mut parts: Vec<&[u8]> = Vec::new();
    for (name, text) in FIXTURES {
        parts.push(name.as_bytes());
        parts.push(text.as_bytes());
    }
    parts.push(b"e1-matrix.json");
    parts.push(E1_MATRIX.as_bytes());
    sha256_hex(&parts)
}

/// The sandbox facts a run depends on: whether `bwrap` is installed and
/// whether sandbox or separate-uid lanes were required.
pub fn sandbox_info() -> Value {
    let bwrap = ["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    let flag = |k: &str| std::env::var(k).is_ok_and(|v| v == "1");
    json!({
        "bwrap": bwrap,
        "require_sandbox_tests": flag("CROM_REQUIRE_SANDBOX_TESTS"),
        "require_separate_uid_tests": flag("CROM_REQUIRE_SEPARATE_UID_TESTS"),
    })
}

impl ReproMeta {
    /// Metadata of this process's checkout, policy set, and machine.
    pub fn collect(command: &str) -> Self {
        let pdp =
            CedarPdp::from_sources("Support", SCHEMA, POLICIES).expect("support policies load");
        let commit = git(&["rev-parse", "HEAD"]).or_else(|| std::env::var("GITHUB_SHA").ok());
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).map(|s| !s.is_empty());
        Self {
            commit,
            dirty,
            policy_set_hash: pdp.policy_set_hash().to_owned(),
            schema_hash: pdp.schema_hash().to_owned(),
            grant_fixtures_hash: grant_fixtures_hash(),
            sandbox: sandbox_info(),
            provider: None,
            model: None,
            build: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .into(),
            machine: Machine::current(),
            command: command.into(),
        }
    }
}

/// One experiment's report.
#[derive(Debug, Clone, Serialize)]
pub struct ExperimentReport<T: Serialize> {
    pub format: &'static str,
    /// `E1`, `E2`, `E4a`, or `E5`.
    pub experiment: &'static str,
    pub title: &'static str,
    pub meta: ReproMeta,
    pub results: T,
}

impl<T: Serialize> ExperimentReport<T> {
    pub fn new(experiment: &'static str, title: &'static str, command: &str, results: T) -> Self {
        Self {
            format: EXPERIMENTS_FORMAT,
            experiment,
            title,
            meta: ReproMeta::collect(command),
            results,
        }
    }

    /// Writes the report as pretty JSON, creating the parent directory.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).map_err(std::io::Error::other)? + "\n",
        )
    }
}

// ------------------------------------------------------------------- E1

/// The E1 expected table.
#[derive(Debug, Clone, Deserialize)]
pub struct Matrix {
    pub grants: BTreeMap<String, Value>,
    pub cells: Vec<Cell>,
}

/// One E1 cell: a grant, its calls, and the expected results and effects.
#[derive(Debug, Clone, Deserialize)]
pub struct Cell {
    pub id: String,
    /// The MILESTONE_18 §14.2 row it belongs to.
    pub row: String,
    pub label: String,
    pub grant: String,
    pub role: String,
    pub tenant: String,
    pub scope: String,
    pub resource: String,
    pub steps: Vec<Value>,
    pub expected_ledger_effects: Vec<Value>,
}

impl Matrix {
    pub fn load() -> Self {
        serde_json::from_str(E1_MATRIX).expect("e1-matrix.json parses")
    }
}

impl Cell {
    /// The agent-visible results this cell expects, in call order, joined
    /// with `->`.
    pub fn expected(&self) -> String {
        self.steps
            .iter()
            .filter_map(|s| s.get("expect").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("->")
    }

    /// The cell as a scenario fixture, so it runs through the same probes,
    /// ledger oracle, and audit checks as G1–G18.
    pub fn fixture(&self, m: &Matrix) -> Fixture {
        let grant = m
            .grants
            .get(&self.grant)
            .unwrap_or_else(|| panic!("{}: unknown grant {}", self.id, self.grant));
        let v = json!({
            "scenario": self.id,
            "title": self.label,
            "spec": "MILESTONE_18 §13.4 E1",
            "vector": self.resource,
            "expected": self.expected(),
            "grant": grant,
            "steps": self.steps,
            "expected_ledger_effects": self.expected_ledger_effects,
        });
        Fixture::parse(&v.to_string()).unwrap_or_else(|e| panic!("{}: {e}", self.id))
    }
}

/// One E1 cell in one lane.
#[derive(Debug, Clone, Serialize)]
pub struct CellResult {
    pub id: String,
    pub lane: String,
    pub row: String,
    pub label: String,
    pub role: String,
    pub tenant: String,
    pub scope: String,
    pub resource: String,
    pub expected: String,
    pub observed: String,
    pub expected_effects: usize,
    pub ledger_entries: usize,
    pub ledger_matches: bool,
    pub unmediated: usize,
    pub audit_verified: Option<bool>,
    /// Observed equals expected, the ledger equals the expected effects,
    /// no effect is unmediated, and the audit log verifies.
    pub agrees: bool,
    pub mismatches: Vec<String>,
}

impl CellResult {
    pub fn new(cell: &Cell, lane: &str, r: &ScenarioReport) -> Self {
        let observed = r
            .steps
            .iter()
            .filter_map(|s| s.got.as_deref())
            .collect::<Vec<_>>()
            .join("->");
        let expected = cell.expected();
        let agrees = observed == expected
            && r.mismatches.is_empty()
            && r.ledger.matches_expected
            && r.ledger.unmediated == 0
            && r.audit.verified == Some(true);
        Self {
            id: cell.id.clone(),
            lane: lane.into(),
            row: cell.row.clone(),
            label: cell.label.clone(),
            role: cell.role.clone(),
            tenant: cell.tenant.clone(),
            scope: cell.scope.clone(),
            resource: cell.resource.clone(),
            expected,
            observed,
            expected_effects: cell.expected_ledger_effects.len(),
            ledger_entries: r.ledger.entries,
            ledger_matches: r.ledger.matches_expected,
            unmediated: r.ledger.unmediated,
            audit_verified: r.audit.verified,
            agrees,
            mismatches: r.mismatches.clone(),
        }
    }
}

// ------------------------------------------------------------------- E2

/// One scenario run with Guard on or off.
#[derive(Debug, Clone, Serialize)]
pub struct OnOffRun {
    pub scenario: String,
    /// Scenario lane (`guard_in_process`, `guard_server`, `guard_off`).
    pub lane: String,
    pub guard: bool,
    pub attempted: usize,
    /// Attacks a ledger or context probe saw take effect.
    pub unauthorized_effects: usize,
    pub legit_success: bool,
    pub ledger_entries: usize,
    pub wall_ms: f64,
}

impl OnOffRun {
    pub fn new(r: &ScenarioReport, lane: &str, guard: bool, wall: std::time::Duration) -> Self {
        Self {
            scenario: r.scenario.clone(),
            lane: lane.into(),
            guard,
            attempted: r.attempted,
            unauthorized_effects: r.executed,
            legit_success: r.legit_success,
            ledger_entries: r.ledger.entries,
            wall_ms: wall.as_secs_f64() * 1e3,
        }
    }
}

// ------------------------------------------------------------------ E4a

/// A Guard decision, without ids or timings: what E4a compares.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionView {
    pub action: Option<String>,
    pub resource: Option<String>,
    pub args_digest: Option<String>,
    pub decision: Option<String>,
    pub reason: Option<String>,
    pub determining_policies: Vec<String>,
    pub outcome: Option<String>,
}

fn json_str<T: Serialize>(v: &T) -> Option<String> {
    match serde_json::to_value(v).ok()? {
        Value::String(s) => Some(s),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Each decision record joined with its outcome record, in log order.
pub fn decision_sequence(records: &[AuditRecord]) -> Vec<DecisionView> {
    records
        .iter()
        .filter(|r| r.record == RecordKind::Decision)
        .map(|d| {
            let outcome = records
                .iter()
                .find(|o| o.record == RecordKind::Outcome && o.decision_id == d.decision_id)
                .and_then(|o| o.outcome.as_ref().and_then(json_str));
            DecisionView {
                action: d.action.as_ref().map(|a| a.as_str().to_owned()),
                resource: d.resource.as_ref().and_then(json_str),
                args_digest: d.args_digest.clone(),
                decision: d.decision.as_ref().and_then(json_str),
                reason: d.reason.as_ref().map(|r| r.code().to_owned()),
                determining_policies: d
                    .determining_policies
                    .iter()
                    .map(|p| p.as_str().to_owned())
                    .collect(),
                outcome,
            }
        })
        .collect()
}

/// One provider's recorded tool calls, replayed through Crom against Guard.
#[derive(Debug, Clone, Serialize)]
pub struct ProviderReplay {
    pub provider: String,
    pub model: String,
    /// The tool-call encoding on the wire. OpenAI and DeepSeek share
    /// chat completions, so four profiles cover three formats.
    pub wire_format: String,
    /// The transcript under `fixtures/experiments/e4a/`.
    pub transcript: String,
    /// `synthetic` or `recorded`, from the transcript.
    pub source: String,
    pub requests_served: usize,
    pub decisions: Vec<DecisionView>,
    pub ledger_effects: usize,
    pub audit_verified: bool,
    pub unmediated: usize,
    /// `decisions` equals the direct replay's.
    pub equal_to_reference: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct E4aResults {
    /// The same calls sent straight to Guard, with no model provider.
    pub reference: Vec<DecisionView>,
    pub providers: Vec<ProviderReplay>,
    pub all_equal: bool,
}

// ------------------------------------------------------------------- E5

/// Percentiles of one latency sample, in microseconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyStats {
    pub n: usize,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub mean_us: f64,
    pub max_us: f64,
}

/// Nearest-rank percentiles of `samples_ns`; `None` when empty.
pub fn latency_stats(samples_ns: &[u64]) -> Option<LatencyStats> {
    if samples_ns.is_empty() {
        return None;
    }
    let mut s = samples_ns.to_vec();
    s.sort_unstable();
    let n = s.len();
    let rank = |q: f64| -> f64 {
        let idx = ((q / 100.0) * n as f64).ceil() as usize;
        s[idx.clamp(1, n) - 1] as f64 / 1e3
    };
    Some(LatencyStats {
        n,
        p50_us: rank(50.0),
        p95_us: rank(95.0),
        p99_us: rank(99.0),
        mean_us: s.iter().map(|&v| v as f64).sum::<f64>() / n as f64 / 1e3,
        max_us: s[n - 1] as f64 / 1e3,
    })
}

/// One (operation, path) latency row.
#[derive(Debug, Clone, Serialize)]
pub struct LatencyRow {
    pub operation: String,
    /// `direct` (trusted executor with the credential), `guard_in_process`,
    /// `guard_http`, or `policy_only` (Guard's `duration_us.policy_us`).
    pub path: String,
    /// The agent-visible result every measured call had.
    pub result: String,
    pub warmup: usize,
    pub stats: LatencyStats,
}

#[derive(Debug, Clone, Serialize)]
pub struct E5Results {
    pub iterations: usize,
    pub warmup: usize,
    /// The audit sink and where its log lives: on `tmpfs`, `fsync` is
    /// close to free.
    pub audit_sink: String,
    pub rows: Vec<LatencyRow>,
}

/// The temp directory (where E5's audit logs live) and its filesystem type
/// from `/proc/self/mounts`, e.g. `/tmp (tmpfs)`.
pub fn temp_dir_fs() -> String {
    let dir = std::env::temp_dir();
    let dir = dir.canonicalize().unwrap_or(dir);
    let fs = std::fs::read_to_string("/proc/self/mounts")
        .ok()
        .and_then(|m| {
            m.lines()
                .filter_map(|l| {
                    let mut f = l.split_whitespace();
                    let (_, target, fstype) = (f.next()?, f.next()?, f.next()?);
                    dir.starts_with(target)
                        .then(|| (target.len(), fstype.to_owned()))
                })
                .max_by_key(|(len, _)| *len)
                .map(|(_, t)| t)
        });
    format!(
        "{} ({})",
        dir.display(),
        fs.as_deref().unwrap_or("unknown fs")
    )
}

/// `policy_us` of every decision record for `action` (µs resolution).
pub fn policy_samples_ns(records: &[AuditRecord], action: &str) -> Vec<u64> {
    records
        .iter()
        .filter(|r| r.record == RecordKind::Decision)
        .filter(|r| r.action.as_ref().is_some_and(|a| a.as_str() == action))
        .filter_map(|r| r.duration_us.policy_us)
        .map(|us| us * 1000)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nearest-rank percentiles pick the ceil(q·n)-th smallest sample, in microseconds.
    #[test]
    fn nearest_rank_percentiles() {
        let samples: Vec<u64> = (1..=100).rev().map(|v| v * 1000).collect();
        let s = latency_stats(&samples).unwrap();
        assert_eq!((s.n, s.p50_us, s.p95_us, s.p99_us), (100, 50.0, 95.0, 99.0));
        assert_eq!((s.mean_us, s.max_us), (50.5, 100.0));
        let one = latency_stats(&[7000]).unwrap();
        assert_eq!((one.p50_us, one.p99_us), (7.0, 7.0));
        assert!(latency_stats(&[]).is_none());
    }

    /// The E1 table has at least 40 cells with unique ids, each naming a known grant and expecting a result for every call.
    #[test]
    fn e1_matrix_is_well_formed() {
        let m = Matrix::load();
        assert!(m.cells.len() >= 40, "{} cells", m.cells.len());
        let ids: std::collections::BTreeSet<&str> = m.cells.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids.len(), m.cells.len());
        for c in &m.cells {
            let fx = c.fixture(&m);
            let calls = fx.steps.iter().filter(|s| s.app.is_none()).count();
            assert_eq!(c.expected().split("->").count(), calls, "{}", c.id);
        }
    }

    /// Metadata carries Guard's policy and schema hashes and a stable fixtures hash.
    #[test]
    fn repro_meta_hashes() {
        let m = ReproMeta::collect("test");
        assert_eq!(m.policy_set_hash.len(), 64);
        assert_eq!(m.schema_hash.len(), 64);
        assert_eq!(m.grant_fixtures_hash, grant_fixtures_hash());
        assert_eq!(m.command, "test");
    }
}
