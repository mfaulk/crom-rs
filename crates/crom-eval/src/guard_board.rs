//! Guard section of `crom eval scoreboard` (#363, MILESTONE_18 §13.4, §18).
//! Built only from archived reports: `guard-scenarios/v1` (adversarial
//! scenarios, scored on the business ledger), `guard-experiments/v1`
//! (E1, E2, E4a, E5), `guard-demo/v1` (the reference app), and
//! `m17-adversarial-archive/v1` (an M17 adversarial report, e.g. the #340
//! base). Lanes are read by name, so a new scenario lane appears without code
//! changes. Security counts are never combined with capability or latency
//! into one score.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::scoreboard::{opt_f, Format, Table};
use crate::stats::{mcnemar_exact, wilson_interval, Interval};

pub const SCENARIOS_FORMAT: &str = "guard-scenarios/v1";
pub const EXPERIMENTS_FORMAT: &str = "guard-experiments/v1";
pub const DEMO_FORMAT: &str = "guard-demo/v1";
pub const M17_ARCHIVE_FORMAT: &str = "m17-adversarial-archive/v1";

/// One report file, as read.
#[derive(Debug, Clone)]
pub struct GuardInput {
    pub path: String,
    pub value: Value,
}

/// Reads each path; a directory contributes its `*.json` files in name order.
pub fn load_reports(paths: &[PathBuf]) -> Result<Vec<GuardInput>, String> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut inner: Vec<PathBuf> = std::fs::read_dir(p)
                .map_err(|e| format!("{}: {e}", p.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|f| f.extension().is_some_and(|x| x == "json"))
                .collect();
            inner.sort();
            files.extend(inner);
        } else {
            files.push(p.clone());
        }
    }
    files
        .into_iter()
        .map(|f| {
            let text = std::fs::read_to_string(&f).map_err(|e| format!("{}: {e}", f.display()))?;
            let value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", f.display()))?;
            Ok(GuardInput {
                path: display_path(&f),
                value,
            })
        })
        .collect()
}

fn display_path(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

// ------------------------------------------------------------- inputs

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InMeta {
    commit: Option<String>,
    dirty: Option<bool>,
    policy_set_hash: Option<String>,
    schema_hash: Option<String>,
    grant_fixtures_hash: Option<String>,
    sandbox: Value,
    provider: Option<String>,
    model: Option<String>,
    build: Option<String>,
    machine: Value,
    command: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InScenario {
    scenario: String,
    title: String,
    mode: String,
    status: String,
    planned_attacks: usize,
    attempted: usize,
    blocked_by_guard: usize,
    blocked_by_sandbox: usize,
    blocked_by_preflight: usize,
    no_effect: usize,
    executed: usize,
    legit_success: bool,
    injection_delivered: Option<bool>,
    ledger: InLedger,
    audit: InAudit,
    mismatches: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InLedger {
    entries: usize,
    matches_expected: bool,
    unmediated: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InAudit {
    verified: Option<bool>,
    uncovered_decisions: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InCell {
    id: String,
    lane: String,
    row: String,
    label: String,
    role: String,
    tenant: String,
    scope: String,
    resource: String,
    expected: String,
    observed: String,
    ledger_matches: bool,
    unmediated: usize,
    audit_verified: Option<bool>,
    agrees: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InOnOff {
    scenario: String,
    lane: String,
    guard: bool,
    attempted: usize,
    unauthorized_effects: usize,
    legit_success: bool,
    wall_ms: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InReplay {
    provider: String,
    model: String,
    source: String,
    requests_served: usize,
    decisions: Vec<Value>,
    ledger_effects: usize,
    audit_verified: bool,
    unmediated: usize,
    equal_to_reference: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InLatency {
    operation: String,
    path: String,
    result: String,
    stats: LatencyCells,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct InM17Scenario {
    number: u32,
    id: String,
    sandbox: String,
    attempted: usize,
    blocked_by_policy: usize,
    blocked_by_sandbox: usize,
    executed: usize,
    legit_success: bool,
}

// ------------------------------------------------------------ outputs

/// `k / n` with its Wilson 95% interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    pub k: usize,
    pub n: usize,
    pub rate: Option<f64>,
    pub wilson95: Option<Interval>,
}

impl Rate {
    fn new(k: usize, n: usize) -> Self {
        Self {
            k,
            n,
            rate: (n > 0).then(|| k as f64 / n as f64),
            wilson95: wilson_interval(k, n),
        }
    }

    fn cell(&self) -> String {
        match self.wilson95 {
            Some(w) => format!(
                "{}/{} ({:.3}, [{:.3}, {:.3}])",
                self.k,
                self.n,
                self.rate.unwrap_or_default(),
                w.low,
                w.high
            ),
            None => format!("{}/{}", self.k, self.n),
        }
    }
}

/// Where a section's numbers came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub path: String,
    pub format: String,
    pub experiment: Option<String>,
    pub commit: Option<String>,
    pub dirty: Option<bool>,
    pub policy_set_hash: Option<String>,
    pub schema_hash: Option<String>,
    pub grant_fixtures_hash: Option<String>,
    pub sandbox: Value,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub build: Option<String>,
    pub machine: Value,
    pub command: Option<String>,
}

/// One scenario lane, summed over its scenarios.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaneSummary {
    pub lane: String,
    /// Guard mediated the agent's calls (every lane but `guard_off`).
    pub guard: bool,
    pub scenarios_ran: usize,
    pub scenarios_skipped: usize,
    pub planned: usize,
    pub attempted: usize,
    pub blocked_by_guard: usize,
    pub blocked_by_sandbox: usize,
    pub blocked_by_preflight: usize,
    pub no_effect: usize,
    /// Unauthorized effects: attacks a ledger or context probe saw.
    pub executed: usize,
    pub legit: Rate,
    pub injection_delivered: Rate,
    pub ledger_entries: usize,
    pub unmediated: usize,
    /// Ledger entries with exactly one Guard `executed` record / entries;
    /// `None` without Guard or without entries.
    pub audit_coverage: Option<f64>,
    /// Runs whose audit log verified / runs with an audit log.
    pub audit_verified: Rate,
    pub uncovered_decisions: usize,
    pub mismatches: usize,
}

/// One scenario across lanes: `executed/attempted`, or `skipped`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioRow {
    pub scenario: String,
    pub title: String,
    pub lanes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E1Lane {
    pub lane: String,
    pub agreement: Rate,
    pub ledger_matches: usize,
    pub unmediated: usize,
    pub audit_verified: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E1Cell {
    pub id: String,
    pub row: String,
    pub role: String,
    pub tenant: String,
    pub scope: String,
    pub resource: String,
    pub call: String,
    pub expected: String,
    /// Observed result per lane.
    pub observed: BTreeMap<String, String>,
    pub agrees: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E1Summary {
    pub cells: usize,
    pub lanes: Vec<E1Lane>,
    pub table: Vec<E1Cell>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E2Lane {
    pub lane: String,
    pub guard: bool,
    pub scenarios: usize,
    pub attempted: usize,
    pub unauthorized_effects: usize,
    /// Scenarios with at least one unauthorized effect.
    pub scenarios_with_effect: Rate,
    pub legit: Rate,
    pub median_wall_ms: Option<f64>,
}

/// Exact McNemar on scenarios paired across two lanes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paired {
    pub on: String,
    pub off: String,
    pub pairs: usize,
    /// Discordant pairs: true only with Guard on / true only with Guard off.
    pub only_on: usize,
    pub only_off: usize,
    pub p_value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E2Summary {
    pub lanes: Vec<E2Lane>,
    /// "At least one unauthorized effect", Guard on vs off.
    pub effect_mcnemar: Option<Paired>,
    /// Legitimate subtask completed, Guard on vs off.
    pub legit_mcnemar: Option<Paired>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E4aProvider {
    pub provider: String,
    pub model: String,
    pub source: String,
    pub requests_served: usize,
    pub decisions: usize,
    pub ledger_effects: usize,
    pub audit_verified: bool,
    pub unmediated: usize,
    pub equal_to_reference: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E4aSummary {
    pub reference_decisions: usize,
    pub providers: Vec<E4aProvider>,
    pub all_equal: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LatencyCells {
    pub n: usize,
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E5Row {
    pub operation: String,
    pub path: String,
    pub result: String,
    pub stats: LatencyCells,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct E5Summary {
    pub iterations: usize,
    pub audit_sink: String,
    pub rows: Vec<E5Row>,
}

/// One reference-app run (`examples/guard-support`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemoSummary {
    pub path: String,
    pub agent: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub rows: Rate,
    pub checks: Rate,
    pub ledger_effects: usize,
    pub pass: bool,
}

/// One M17 adversarial lane (`sandbox` mode), e.g. at the #340 base.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct M17Lane {
    pub label: String,
    pub commit: Option<String>,
    pub sandbox: String,
    pub scenarios: usize,
    pub attempted: usize,
    pub blocked_by_policy: usize,
    pub blocked_by_sandbox: usize,
    pub executed: usize,
    pub legit: Rate,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GuardBoard {
    pub sources: Vec<Source>,
    pub lanes: Vec<LaneSummary>,
    pub scenarios: Vec<ScenarioRow>,
    pub e1: Option<E1Summary>,
    pub e2: Option<E2Summary>,
    pub e4a: Option<E4aSummary>,
    pub e5: Option<E5Summary>,
    pub demos: Vec<DemoSummary>,
    pub m17: Vec<M17Lane>,
}

fn parse<T: for<'de> Deserialize<'de>>(v: &Value, path: &str) -> Result<T, String> {
    serde_json::from_value(v.clone()).map_err(|e| format!("{path}: {e}"))
}

fn source(input: &GuardInput, format: &str, experiment: Option<&str>) -> Source {
    let m: InMeta = serde_json::from_value(input.value["meta"].clone()).unwrap_or_default();
    Source {
        path: input.path.clone(),
        format: format.into(),
        experiment: experiment.map(str::to_owned),
        commit: m
            .commit
            .or_else(|| input.value["commit"].as_str().map(str::to_owned)),
        dirty: m.dirty,
        policy_set_hash: m.policy_set_hash,
        schema_hash: m.schema_hash,
        grant_fixtures_hash: m.grant_fixtures_hash,
        sandbox: m.sandbox,
        provider: m
            .provider
            .or_else(|| input.value["provider"].as_str().map(str::to_owned)),
        model: m
            .model
            .or_else(|| input.value["model"].as_str().map(str::to_owned)),
        build: m.build,
        machine: m.machine,
        command: m
            .command
            .or_else(|| input.value["command"].as_str().map(str::to_owned)),
    }
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

impl GuardBoard {
    /// Aggregates `inputs`. Fails on an unknown format or a second report
    /// for the same experiment.
    pub fn build(inputs: &[GuardInput]) -> Result<Self, String> {
        let mut b = GuardBoard::default();
        let mut scenarios: Vec<InScenario> = Vec::new();
        for input in inputs {
            let v = &input.value;
            let format = v["format"].as_str().unwrap_or_default();
            match format {
                SCENARIOS_FORMAT => {
                    b.sources.push(source(input, format, None));
                    scenarios.extend(parse::<Vec<InScenario>>(&v["scenarios"], &input.path)?);
                }
                EXPERIMENTS_FORMAT => {
                    let exp = v["experiment"].as_str().unwrap_or_default();
                    b.sources.push(source(input, format, Some(exp)));
                    b.experiment(exp, &v["results"], &input.path)?;
                }
                DEMO_FORMAT => {
                    b.sources.push(source(input, format, None));
                    b.demos.push(demo(input));
                }
                M17_ARCHIVE_FORMAT => {
                    b.sources.push(source(input, format, None));
                    let label = v["label"].as_str().unwrap_or("M17").to_owned();
                    let rows: Vec<InM17Scenario> = parse(&v["report"]["scenarios"], &input.path)?;
                    b.m17.extend(m17_lanes(&label, v["commit"].as_str(), &rows));
                }
                other => {
                    return Err(format!(
                        "{}: unknown report format {other:?} (expected {SCENARIOS_FORMAT}, \
                         {EXPERIMENTS_FORMAT}, {DEMO_FORMAT}, or {M17_ARCHIVE_FORMAT})",
                        input.path
                    ))
                }
            }
        }
        b.scenario_sections(&scenarios);
        Ok(b)
    }

    fn experiment(&mut self, exp: &str, results: &Value, path: &str) -> Result<(), String> {
        let dup = || format!("{path}: a second {exp} report");
        match exp {
            "E1" => {
                if self.e1.is_some() {
                    return Err(dup());
                }
                let cells: Vec<InCell> = parse(&results["results"], path)?;
                self.e1 = Some(e1(results["cells"].as_u64().unwrap_or(0) as usize, &cells));
            }
            "E2" => {
                if self.e2.is_some() {
                    return Err(dup());
                }
                self.e2 = Some(e2(&parse::<Vec<InOnOff>>(&results["runs"], path)?));
            }
            "E4a" => {
                if self.e4a.is_some() {
                    return Err(dup());
                }
                let providers: Vec<InReplay> = parse(&results["providers"], path)?;
                self.e4a = Some(E4aSummary {
                    reference_decisions: results["reference"].as_array().map_or(0, Vec::len),
                    all_equal: !providers.is_empty()
                        && providers.iter().all(|p| p.equal_to_reference),
                    providers: providers
                        .into_iter()
                        .map(|p| E4aProvider {
                            provider: p.provider,
                            model: p.model,
                            source: p.source,
                            requests_served: p.requests_served,
                            decisions: p.decisions.len(),
                            ledger_effects: p.ledger_effects,
                            audit_verified: p.audit_verified,
                            unmediated: p.unmediated,
                            equal_to_reference: p.equal_to_reference,
                        })
                        .collect(),
                });
            }
            "E5" => {
                if self.e5.is_some() {
                    return Err(dup());
                }
                let rows: Vec<InLatency> = parse(&results["rows"], path)?;
                self.e5 = Some(E5Summary {
                    iterations: results["iterations"].as_u64().unwrap_or(0) as usize,
                    audit_sink: results["audit_sink"].as_str().unwrap_or_default().into(),
                    rows: rows
                        .into_iter()
                        .map(|r| E5Row {
                            operation: r.operation,
                            path: r.path,
                            result: r.result,
                            stats: r.stats,
                        })
                        .collect(),
                });
            }
            other => return Err(format!("{path}: unknown experiment {other:?}")),
        }
        Ok(())
    }

    fn scenario_sections(&mut self, all: &[InScenario]) {
        // Lanes in first-seen order.
        let mut order: Vec<String> = Vec::new();
        for s in all {
            if !order.contains(&s.mode) {
                order.push(s.mode.clone());
            }
        }
        for lane in &order {
            let rs: Vec<&InScenario> = all.iter().filter(|s| &s.mode == lane).collect();
            let ran: Vec<&&InScenario> = rs.iter().filter(|s| s.status == "ran").collect();
            let sum = |f: fn(&InScenario) -> usize| ran.iter().map(|s| f(s)).sum::<usize>();
            let guard = lane != "guard_off";
            let entries = sum(|s| s.ledger.entries);
            let unmediated = sum(|s| s.ledger.unmediated);
            let audited: Vec<&&&InScenario> =
                ran.iter().filter(|s| s.audit.verified.is_some()).collect();
            let injections: Vec<bool> = ran.iter().filter_map(|s| s.injection_delivered).collect();
            self.lanes.push(LaneSummary {
                lane: lane.clone(),
                guard,
                scenarios_ran: ran.len(),
                scenarios_skipped: rs.len() - ran.len(),
                planned: sum(|s| s.planned_attacks),
                attempted: sum(|s| s.attempted),
                blocked_by_guard: sum(|s| s.blocked_by_guard),
                blocked_by_sandbox: sum(|s| s.blocked_by_sandbox),
                blocked_by_preflight: sum(|s| s.blocked_by_preflight),
                no_effect: sum(|s| s.no_effect),
                executed: sum(|s| s.executed),
                legit: Rate::new(ran.iter().filter(|s| s.legit_success).count(), ran.len()),
                injection_delivered: Rate::new(
                    injections.iter().filter(|d| **d).count(),
                    injections.len(),
                ),
                ledger_entries: entries,
                unmediated,
                audit_coverage: (guard && entries > 0 && !audited.is_empty())
                    .then(|| (entries - unmediated.min(entries)) as f64 / entries as f64),
                audit_verified: Rate::new(
                    audited
                        .iter()
                        .filter(|s| s.audit.verified == Some(true))
                        .count(),
                    audited.len(),
                ),
                uncovered_decisions: sum(|s| s.audit.uncovered_decisions),
                mismatches: sum(|s| s.mismatches.len()),
            });
        }
        let mut rows: BTreeMap<(usize, String), ScenarioRow> = BTreeMap::new();
        for s in all {
            // G2 before G10: sort on the numeric suffix, then the id.
            let n: usize = s
                .scenario
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .parse()
                .unwrap_or(usize::MAX);
            let row = rows
                .entry((n, s.scenario.clone()))
                .or_insert_with(|| ScenarioRow {
                    scenario: s.scenario.clone(),
                    title: s.title.clone(),
                    lanes: BTreeMap::new(),
                });
            let cell = if s.status == "ran" {
                format!("{}/{}", s.executed, s.attempted)
            } else {
                "skipped".into()
            };
            row.lanes.insert(s.mode.clone(), cell);
        }
        self.scenarios = rows.into_values().collect();
    }
}

fn e1(cells: usize, rs: &[InCell]) -> E1Summary {
    let mut lanes: Vec<String> = Vec::new();
    for r in rs {
        if !lanes.contains(&r.lane) {
            lanes.push(r.lane.clone());
        }
    }
    let lane_rows = lanes
        .iter()
        .map(|l| {
            let mine: Vec<&InCell> = rs.iter().filter(|r| &r.lane == l).collect();
            E1Lane {
                lane: l.clone(),
                agreement: Rate::new(mine.iter().filter(|r| r.agrees).count(), mine.len()),
                ledger_matches: mine.iter().filter(|r| r.ledger_matches).count(),
                unmediated: mine.iter().map(|r| r.unmediated).sum(),
                audit_verified: mine
                    .iter()
                    .filter(|r| r.audit_verified == Some(true))
                    .count(),
            }
        })
        .collect();
    let mut table: Vec<E1Cell> = Vec::new();
    for r in rs {
        match table.iter_mut().find(|c| c.id == r.id) {
            Some(c) => {
                c.observed.insert(r.lane.clone(), r.observed.clone());
                c.agrees &= r.agrees;
            }
            None => table.push(E1Cell {
                id: r.id.clone(),
                row: r.row.clone(),
                role: r.role.clone(),
                tenant: r.tenant.clone(),
                scope: r.scope.clone(),
                resource: r.resource.clone(),
                call: r.label.clone(),
                expected: r.expected.clone(),
                observed: BTreeMap::from([(r.lane.clone(), r.observed.clone())]),
                agrees: r.agrees,
            }),
        }
    }
    E1Summary {
        cells,
        lanes: lane_rows,
        table,
    }
}

fn e2(runs: &[InOnOff]) -> E2Summary {
    let mut lanes: Vec<(String, bool)> = Vec::new();
    for r in runs {
        if !lanes.iter().any(|(l, _)| l == &r.lane) {
            lanes.push((r.lane.clone(), r.guard));
        }
    }
    let lane_rows: Vec<E2Lane> = lanes
        .iter()
        .map(|(l, guard)| {
            let mine: Vec<&InOnOff> = runs.iter().filter(|r| &r.lane == l).collect();
            E2Lane {
                lane: l.clone(),
                guard: *guard,
                scenarios: mine.len(),
                attempted: mine.iter().map(|r| r.attempted).sum(),
                unauthorized_effects: mine.iter().map(|r| r.unauthorized_effects).sum(),
                scenarios_with_effect: Rate::new(
                    mine.iter().filter(|r| r.unauthorized_effects > 0).count(),
                    mine.len(),
                ),
                legit: Rate::new(mine.iter().filter(|r| r.legit_success).count(), mine.len()),
                median_wall_ms: median(mine.iter().map(|r| r.wall_ms).collect()),
            }
        })
        .collect();
    let on = lanes.iter().find(|(_, g)| *g).map(|(l, _)| l.clone());
    let off = lanes.iter().find(|(_, g)| !*g).map(|(l, _)| l.clone());
    let paired = |f: fn(&InOnOff) -> bool| -> Option<Paired> {
        let (on, off) = (on.clone()?, off.clone()?);
        let (mut pairs, mut only_on, mut only_off) = (0, 0, 0);
        for a in runs.iter().filter(|r| r.lane == on) {
            if let Some(b) = runs
                .iter()
                .find(|r| r.lane == off && r.scenario == a.scenario)
            {
                pairs += 1;
                match (f(a), f(b)) {
                    (true, false) => only_on += 1,
                    (false, true) => only_off += 1,
                    _ => {}
                }
            }
        }
        Some(Paired {
            on,
            off,
            pairs,
            only_on,
            only_off,
            p_value: mcnemar_exact(only_on, only_off),
        })
    };
    E2Summary {
        lanes: lane_rows,
        effect_mcnemar: paired(|r| r.unauthorized_effects > 0),
        legit_mcnemar: paired(|r| r.legit_success),
    }
}

fn demo(input: &GuardInput) -> DemoSummary {
    let v = &input.value;
    let n = |k: &str| v[k].as_u64().unwrap_or(0) as usize;
    let checks = v["checks"].as_array().cloned().unwrap_or_default();
    DemoSummary {
        path: input.path.clone(),
        agent: v["agent"].as_str().unwrap_or_default().into(),
        provider: v["meta"]["provider"]
            .as_str()
            .or(v["provider"].as_str())
            .map(str::to_owned),
        model: v["meta"]["model"]
            .as_str()
            .or(v["model"].as_str())
            .map(str::to_owned),
        rows: Rate::new(n("passed"), n("passed") + n("failed")),
        checks: Rate::new(
            checks.iter().filter(|c| c["pass"] == true).count(),
            checks.len(),
        ),
        ledger_effects: n("ledger_effects"),
        pass: v["pass"] == true,
    }
}

fn m17_lanes(label: &str, commit: Option<&str>, rows: &[InM17Scenario]) -> Vec<M17Lane> {
    let mut modes: Vec<&str> = Vec::new();
    for r in rows {
        if !modes.contains(&r.sandbox.as_str()) {
            modes.push(&r.sandbox);
        }
    }
    modes
        .into_iter()
        .map(|m| {
            let mine: Vec<&InM17Scenario> = rows.iter().filter(|r| r.sandbox == m).collect();
            let ids: BTreeSet<(u32, &str)> =
                mine.iter().map(|r| (r.number, r.id.as_str())).collect();
            M17Lane {
                label: label.into(),
                commit: commit.map(str::to_owned),
                sandbox: m.into(),
                scenarios: ids.len(),
                attempted: mine.iter().map(|r| r.attempted).sum(),
                blocked_by_policy: mine.iter().map(|r| r.blocked_by_policy).sum(),
                blocked_by_sandbox: mine.iter().map(|r| r.blocked_by_sandbox).sum(),
                executed: mine.iter().map(|r| r.executed).sum(),
                legit: Rate::new(mine.iter().filter(|r| r.legit_success).count(), mine.len()),
            }
        })
        .collect()
}

// ------------------------------------------------------- determinism check

impl GuardBoard {
    /// Facts that must not change between runs of the same commit:
    /// everything except provenance, wall times, and latency. Scenario
    /// lanes that were skipped are left out, so environments with fewer
    /// lanes compare on the lanes they ran.
    pub fn deterministic_facts(&self, raw: &[GuardInput]) -> BTreeMap<String, String> {
        let mut f = BTreeMap::new();
        for input in raw {
            if input.value["format"] == SCENARIOS_FORMAT {
                let rows: Vec<InScenario> =
                    serde_json::from_value(input.value["scenarios"].clone()).unwrap_or_default();
                for s in rows.iter().filter(|s| s.status == "ran") {
                    f.insert(
                        format!("scenario {} {}", s.scenario, s.mode),
                        format!(
                            "attempted={} guard={} sandbox={} preflight={} no_effect={} executed={} legit={} ledger={}/{} unmediated={} audit={:?} mismatches={}",
                            s.attempted,
                            s.blocked_by_guard,
                            s.blocked_by_sandbox,
                            s.blocked_by_preflight,
                            s.no_effect,
                            s.executed,
                            s.legit_success,
                            s.ledger.entries,
                            s.ledger.matches_expected,
                            s.ledger.unmediated,
                            s.audit.verified,
                            s.mismatches.len()
                        ),
                    );
                }
            }
        }
        if let Some(e) = &self.e1 {
            f.insert("E1 cells".into(), e.cells.to_string());
            for c in &e.table {
                for (lane, obs) in &c.observed {
                    f.insert(
                        format!("E1 {} {lane}", c.id),
                        format!("expected={} observed={obs} agrees={}", c.expected, c.agrees),
                    );
                }
            }
        }
        if let Some(e) = &self.e2 {
            for l in &e.lanes {
                f.insert(
                    format!("E2 {}", l.lane),
                    format!(
                        "scenarios={} attempted={} effects={} legit={}/{}",
                        l.scenarios, l.attempted, l.unauthorized_effects, l.legit.k, l.legit.n
                    ),
                );
            }
        }
        if let Some(e) = &self.e4a {
            f.insert("E4a reference".into(), e.reference_decisions.to_string());
            for p in &e.providers {
                f.insert(
                    format!("E4a {}", p.provider),
                    format!(
                        "served={} decisions={} effects={} equal={}",
                        p.requests_served, p.decisions, p.ledger_effects, p.equal_to_reference
                    ),
                );
            }
        }
        f
    }
}

/// Differences between the deterministic facts of `fresh` and `archived`.
/// A scenario lane present on only one side is not a difference (an
/// environment may lack a lane); every other missing fact is.
pub fn compare_facts(
    fresh: &BTreeMap<String, String>,
    archived: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (k, a) in archived {
        match fresh.get(k) {
            Some(v) if v != a => out.push(format!("{k}: archived `{a}`, fresh `{v}`")),
            None if !k.starts_with("scenario ") => {
                out.push(format!("{k}: missing from the fresh run"))
            }
            _ => {}
        }
    }
    for k in fresh.keys() {
        if !archived.contains_key(k) && !k.starts_with("scenario ") {
            out.push(format!("{k}: not in the archive"));
        }
    }
    out
}

// ------------------------------------------------------------ rendering

fn yes(b: bool) -> String {
    if b { "yes" } else { "NO" }.into()
}

fn short(s: &Option<String>, n: usize) -> String {
    s.as_deref()
        .map_or("-".into(), |v| v.chars().take(n).collect())
}

impl GuardBoard {
    /// Renders every section that has data. `heading` is the Markdown level
    /// of the section title (`#` or `##`).
    pub fn render(&self, fmt: Format, top_level: bool, s: &mut String) {
        let md = fmt == Format::Markdown;
        let h = if top_level { "#" } else { "##" };
        s.push_str(&if md {
            format!("{h} Guard scoreboard\n\n")
        } else {
            "== Guard scoreboard ==\n\n".into()
        });
        s.push_str(
            "Generated from archived reports by `crom eval scoreboard --guard-report`. \
             Security, task completion, and latency are separate tables; nothing is \
             combined into one score. Rates are k/n with Wilson 95% intervals.\n\n",
        );
        if !self.lanes.is_empty() {
            Table {
                title: "Security: adversarial scenarios by lane (ledger-scored)".into(),
                header: vec![
                    "lane",
                    "guard",
                    "ran",
                    "skipped",
                    "attempted",
                    "blocked (guard)",
                    "blocked (sandbox)",
                    "blocked (preflight)",
                    "no effect",
                    "executed",
                    "legit",
                    "ledger entries",
                    "unmediated",
                    "audit coverage",
                    "audit verify",
                    "mismatches",
                ],
                rows: self
                    .lanes
                    .iter()
                    .map(|l| {
                        vec![
                            l.lane.clone(),
                            if l.guard { "on" } else { "off" }.into(),
                            l.scenarios_ran.to_string(),
                            l.scenarios_skipped.to_string(),
                            l.attempted.to_string(),
                            l.blocked_by_guard.to_string(),
                            l.blocked_by_sandbox.to_string(),
                            l.blocked_by_preflight.to_string(),
                            l.no_effect.to_string(),
                            l.executed.to_string(),
                            l.legit.cell(),
                            l.ledger_entries.to_string(),
                            l.unmediated.to_string(),
                            opt_f(l.audit_coverage, 3),
                            if l.audit_verified.n == 0 {
                                "n/a".into()
                            } else {
                                format!("{}/{}", l.audit_verified.k, l.audit_verified.n)
                            },
                            l.mismatches.to_string(),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
            let lanes: Vec<&str> = self.lanes.iter().map(|l| l.lane.as_str()).collect();
            let mut header = vec!["scenario", "title"];
            header.extend(lanes.iter().copied());
            Table {
                title: "Security: executed/attempted per scenario and lane".into(),
                header,
                rows: self
                    .scenarios
                    .iter()
                    .map(|r| {
                        let mut row = vec![r.scenario.clone(), r.title.clone()];
                        row.extend(
                            lanes
                                .iter()
                                .map(|l| r.lanes.get(*l).cloned().unwrap_or_else(|| "-".into())),
                        );
                        row
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if !self.m17.is_empty() {
            Table {
                title: "Comparison: M17 adversarial suite (Crom sandbox and policy only, no Guard)"
                    .into(),
                header: vec![
                    "report",
                    "commit",
                    "sandbox",
                    "scenarios",
                    "attempted",
                    "blocked (policy)",
                    "blocked (sandbox)",
                    "executed",
                    "legit",
                ],
                rows: self
                    .m17
                    .iter()
                    .map(|l| {
                        vec![
                            l.label.clone(),
                            short(&l.commit, 7),
                            l.sandbox.clone(),
                            l.scenarios.to_string(),
                            l.attempted.to_string(),
                            l.blocked_by_policy.to_string(),
                            l.blocked_by_sandbox.to_string(),
                            l.executed.to_string(),
                            format!("{}/{}", l.legit.k, l.legit.n),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if let Some(e) = &self.e1 {
            Table {
                title: format!(
                    "E1 authorization matrix: agreement with the expected table ({} cells)",
                    e.cells
                ),
                header: vec![
                    "lane",
                    "agree",
                    "ledger matches",
                    "unmediated",
                    "audit verified",
                ],
                rows: e
                    .lanes
                    .iter()
                    .map(|l| {
                        vec![
                            l.lane.clone(),
                            l.agreement.cell(),
                            format!("{}/{}", l.ledger_matches, l.agreement.n),
                            l.unmediated.to_string(),
                            format!("{}/{}", l.audit_verified, l.agreement.n),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
            let lanes: Vec<String> = e.lanes.iter().map(|l| l.lane.clone()).collect();
            let mut header = vec![
                "cell", "role", "tenant", "scope", "resource", "call", "expected",
            ];
            header.extend(lanes.iter().map(String::as_str));
            header.push("agrees");
            Table {
                title: "E1 expected table and observed results".into(),
                header,
                rows: e
                    .table
                    .iter()
                    .map(|c| {
                        let mut row = vec![
                            c.id.clone(),
                            c.role.clone(),
                            c.tenant.clone(),
                            c.scope.clone(),
                            c.resource.clone(),
                            c.call.clone(),
                            c.expected.clone(),
                        ];
                        row.extend(
                            lanes
                                .iter()
                                .map(|l| c.observed.get(l).cloned().unwrap_or_else(|| "-".into())),
                        );
                        row.push(yes(c.agrees));
                        row
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if let Some(e) = &self.e2 {
            Table {
                title:
                    "E2 Guard on vs off (direct-credential baseline): security and task completion"
                        .into(),
                header: vec![
                    "lane",
                    "guard",
                    "scenarios",
                    "attacks attempted",
                    "unauthorized effects",
                    "scenarios with an effect",
                    "legitimate task completed",
                    "median wall ms",
                ],
                rows: e
                    .lanes
                    .iter()
                    .map(|l| {
                        vec![
                            l.lane.clone(),
                            if l.guard { "on" } else { "off" }.into(),
                            l.scenarios.to_string(),
                            l.attempted.to_string(),
                            l.unauthorized_effects.to_string(),
                            l.scenarios_with_effect.cell(),
                            l.legit.cell(),
                            opt_f(l.median_wall_ms, 2),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
            let rows: Vec<Vec<String>> = [
                ("at least one unauthorized effect", &e.effect_mcnemar),
                ("legitimate task completed", &e.legit_mcnemar),
            ]
            .into_iter()
            .filter_map(|(what, p)| {
                p.as_ref().map(|p| {
                    vec![
                        what.into(),
                        format!("{} vs {}", p.on, p.off),
                        p.pairs.to_string(),
                        p.only_on.to_string(),
                        p.only_off.to_string(),
                        if p.p_value >= 0.001 {
                            format!("{:.3}", p.p_value)
                        } else {
                            format!("{:.2e}", p.p_value)
                        },
                    ]
                })
            })
            .collect();
            if !rows.is_empty() {
                Table {
                    title: "E2 paired by scenario (exact McNemar)".into(),
                    header: vec!["outcome", "lanes", "pairs", "only on", "only off", "p"],
                    rows,
                }
                .render(fmt, s);
            }
        }
        if let Some(e) = &self.e4a {
            Table {
                title: format!(
                    "E4a provider independence: recorded tool calls replayed through Crom ({} reference decisions; all equal: {})",
                    e.reference_decisions,
                    yes(e.all_equal)
                ),
                header: vec![
                    "provider",
                    "model",
                    "transcript",
                    "requests served",
                    "decisions",
                    "ledger effects",
                    "unmediated",
                    "audit verify",
                    "equal to reference",
                ],
                rows: e
                    .providers
                    .iter()
                    .map(|p| {
                        vec![
                            p.provider.clone(),
                            p.model.clone(),
                            p.source.clone(),
                            p.requests_served.to_string(),
                            p.decisions.to_string(),
                            p.ledger_effects.to_string(),
                            p.unmediated.to_string(),
                            yes(p.audit_verified),
                            yes(p.equal_to_reference),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if !self.demos.is_empty() {
            Table {
                title: "Reference application runs (examples/guard-support)".into(),
                header: vec![
                    "report",
                    "agent",
                    "provider/model",
                    "matrix rows",
                    "evidence checks",
                    "ledger effects",
                    "pass",
                ],
                rows: self
                    .demos
                    .iter()
                    .map(|d| {
                        vec![
                            d.path.clone(),
                            d.agent.clone(),
                            match (&d.provider, &d.model) {
                                (Some(p), Some(m)) => format!("{p}/{m}"),
                                (Some(p), None) => p.clone(),
                                _ => "scripted".into(),
                            },
                            format!("{}/{}", d.rows.k, d.rows.n),
                            format!("{}/{}", d.checks.k, d.checks.n),
                            d.ledger_effects.to_string(),
                            yes(d.pass),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if let Some(e) = &self.e5 {
            Table {
                title: format!(
                    "E5 overhead: latency per warm call in microseconds (n = {} per row; audit sink: {})",
                    e.iterations, e.audit_sink
                ),
                header: vec!["operation", "path", "result", "n", "p50 us", "p95 us", "p99 us"],
                rows: e
                    .rows
                    .iter()
                    .map(|r| {
                        vec![
                            r.operation.clone(),
                            r.path.clone(),
                            r.result.clone(),
                            r.stats.n.to_string(),
                            format!("{:.1}", r.stats.p50_us),
                            format!("{:.1}", r.stats.p95_us),
                            format!("{:.1}", r.stats.p99_us),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
        if !self.sources.is_empty() {
            Table {
                title: "Sources and reproducibility".into(),
                header: vec![
                    "report",
                    "format",
                    "commit",
                    "dirty",
                    "policy set",
                    "schema",
                    "grant fixtures",
                    "build",
                    "machine",
                    "sandbox",
                    "provider/model",
                    "command",
                ],
                rows: self
                    .sources
                    .iter()
                    .map(|src| {
                        let machine = match (
                            src.machine["cpu"].as_str(),
                            src.machine["cpus"].as_u64(),
                            src.machine["kernel"].as_str(),
                        ) {
                            (Some(cpu), Some(n), k) => {
                                format!("{cpu} ({n} cpus, {})", k.unwrap_or("?"))
                            }
                            _ => "-".into(),
                        };
                        let sandbox = if src.sandbox.is_null() {
                            "-".into()
                        } else {
                            src.sandbox.to_string().replace('|', "/")
                        };
                        vec![
                            src.path.clone(),
                            match &src.experiment {
                                Some(e) => format!("{} {e}", src.format),
                                None => src.format.clone(),
                            },
                            short(&src.commit, 12),
                            src.dirty.map_or("-".into(), |d| d.to_string()),
                            short(&src.policy_set_hash, 12),
                            short(&src.schema_hash, 12),
                            short(&src.grant_fixtures_hash, 12),
                            src.build.clone().unwrap_or_else(|| "-".into()),
                            machine,
                            sandbox,
                            match (&src.provider, &src.model) {
                                (Some(p), Some(m)) => format!("{p}/{m}"),
                                (Some(p), None) => p.clone(),
                                _ => "-".into(),
                            },
                            src.command.as_deref().map_or("-".into(), |c| {
                                if md {
                                    format!("`{c}`")
                                } else {
                                    c.into()
                                }
                            }),
                        ]
                    })
                    .collect(),
            }
            .render(fmt, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn scen(
        id: &str,
        mode: &str,
        status: &str,
        attempted: usize,
        executed: usize,
        legit: bool,
    ) -> Value {
        let guard = mode != "guard_off";
        json!({
            "scenario": id, "title": format!("title {id}"), "mode": mode, "status": status,
            "planned_attacks": attempted, "attempted": attempted,
            "blocked_by_guard": if guard { attempted - executed } else { 0 },
            "blocked_by_sandbox": 0, "blocked_by_preflight": 0,
            "no_effect": if guard { 0 } else { attempted - executed },
            "executed": executed, "legit_success": legit,
            "injection_delivered": if id == "G15" { json!(true) } else { Value::Null },
            "ledger": {"entries": 2, "matches_expected": guard, "unmediated": if guard { 0 } else { 2 }},
            "audit": {"verified": if guard { json!(true) } else { Value::Null }, "records": 4, "uncovered_decisions": 0},
            "mismatches": [], "steps": []
        })
    }

    fn input(path: &str, v: Value) -> GuardInput {
        GuardInput {
            path: path.into(),
            value: v,
        }
    }

    fn meta(command: &str) -> Value {
        json!({"commit": "0123456789abcdef", "dirty": false, "policy_set_hash": "ab".repeat(32),
               "schema_hash": "cd".repeat(32), "grant_fixtures_hash": "ef".repeat(32),
               "sandbox": {"bwrap": "/usr/bin/bwrap"}, "provider": null, "model": null, "build": "debug",
               "machine": {"cpu": "Test CPU", "cpus": 4, "kernel": "6.0"}, "command": command})
    }

    /// Synthetic reports covering every format: two scenario lanes plus a new lane name, E1, E2, E4a, E5, a demo run, and an M17 archive.
    pub(crate) fn synthetic() -> Vec<GuardInput> {
        vec![
            input(
                "r/scenarios.json",
                json!({
                    "format": SCENARIOS_FORMAT, "meta": meta("cargo test scenarios"),
                    "scenarios": [
                        scen("G1", "guard_in_process", "ran", 3, 0, true),
                        scen("G1", "guard_off", "ran", 3, 2, true),
                        scen("G10", "guard_in_process", "ran", 2, 0, true),
                        scen("G10", "guard_off", "ran", 2, 2, false),
                        scen("G15", "guard_in_process", "ran", 1, 0, true),
                        scen("G15", "guard_off", "ran", 1, 1, true),
                        scen("G1", "crom", "ran", 3, 0, true),
                        scen("G10", "crom", "skipped", 2, 0, false),
                    ]
                }),
            ),
            input(
                "r/e1.json",
                json!({
                    "format": EXPERIMENTS_FORMAT, "experiment": "E1", "meta": meta("cargo test e1_"),
                    "results": {"cells": 2, "results": [
                        {"id": "E1-01", "lane": "guard_in_process", "row": "r", "label": "customer.read a", "role": "op",
                         "tenant": "acme", "scope": "full", "resource": "granted", "expected": "executed",
                         "observed": "executed", "ledger_matches": true, "unmediated": 0, "audit_verified": true, "agrees": true},
                        {"id": "E1-01", "lane": "guard_server", "row": "r", "label": "customer.read a", "role": "op",
                         "tenant": "acme", "scope": "full", "resource": "granted", "expected": "executed",
                         "observed": "executed", "ledger_matches": true, "unmediated": 0, "audit_verified": true, "agrees": true},
                        {"id": "E1-02", "lane": "guard_in_process", "row": "r", "label": "refund b", "role": "op",
                         "tenant": "acme", "scope": "full", "resource": "other", "expected": "policy_deny",
                         "observed": "policy_deny", "ledger_matches": true, "unmediated": 0, "audit_verified": true, "agrees": true},
                        {"id": "E1-02", "lane": "guard_server", "row": "r", "label": "refund b", "role": "op",
                         "tenant": "acme", "scope": "full", "resource": "other", "expected": "policy_deny",
                         "observed": "executed", "ledger_matches": false, "unmediated": 0, "audit_verified": true, "agrees": false}
                    ]}
                }),
            ),
            input(
                "r/e2.json",
                json!({
                    "format": EXPERIMENTS_FORMAT, "experiment": "E2", "meta": meta("cargo test e2_"),
                    "results": {"runs": [
                        {"scenario": "G1", "lane": "guard_in_process", "guard": true, "attempted": 3, "unauthorized_effects": 0, "legit_success": true, "wall_ms": 4.0},
                        {"scenario": "G1", "lane": "guard_off", "guard": false, "attempted": 3, "unauthorized_effects": 2, "legit_success": true, "wall_ms": 1.0},
                        {"scenario": "G2", "lane": "guard_in_process", "guard": true, "attempted": 1, "unauthorized_effects": 0, "legit_success": true, "wall_ms": 6.0},
                        {"scenario": "G2", "lane": "guard_off", "guard": false, "attempted": 1, "unauthorized_effects": 1, "legit_success": false, "wall_ms": 2.0}
                    ]}
                }),
            ),
            input(
                "r/e4a.json",
                json!({
                    "format": EXPERIMENTS_FORMAT, "experiment": "E4a", "meta": meta("cargo test e4a_"),
                    "results": {"reference": [{}, {}], "all_equal": true, "providers": [
                        {"provider": "anthropic", "model": "m1", "source": "synthetic", "requests_served": 3,
                         "decisions": [{}, {}], "ledger_effects": 1, "audit_verified": true, "unmediated": 0, "equal_to_reference": true},
                        {"provider": "gemini", "model": "m2", "source": "synthetic", "requests_served": 3,
                         "decisions": [{}, {}], "ledger_effects": 1, "audit_verified": true, "unmediated": 0, "equal_to_reference": true}
                    ]}
                }),
            ),
            input(
                "r/e5.json",
                json!({
                    "format": EXPERIMENTS_FORMAT, "experiment": "E5", "meta": meta("cargo test --release e5_"),
                    "results": {"iterations": 10, "warmup": 2, "audit_sink": "file", "rows": [
                        {"operation": "customer.read", "path": "direct", "result": "executed", "warmup": 2,
                         "stats": {"n": 10, "p50_us": 1.5, "p95_us": 2.0, "p99_us": 3.25, "mean_us": 1.6, "max_us": 3.3}},
                        {"operation": "customer.read", "path": "guard_in_process", "result": "executed", "warmup": 2,
                         "stats": {"n": 10, "p50_us": 10.0, "p95_us": 12.0, "p99_us": 15.0, "mean_us": 10.6, "max_us": 15.0}}
                    ]}
                }),
            ),
            input(
                "r/demo.json",
                json!({
                    "format": DEMO_FORMAT, "agent": "scripted", "commit": "0123456789abcdef",
                    "command": "cargo run --manifest-path examples/guard-support/Cargo.toml",
                    "rows": [], "checks": [{"check": "a", "pass": true}, {"check": "b", "pass": true}],
                    "ledger_effects": 4, "passed": 21, "failed": 0, "pass": true
                }),
            ),
            input(
                "r/m17.json",
                json!({
                    "format": M17_ARCHIVE_FORMAT, "label": "M17 base (#340)", "commit": "82fb30a1e625",
                    "source": "CI artifact", "report": {"scenarios": [
                        {"number": 1, "id": "a", "sandbox": "required", "attempted": 1, "blocked_by_policy": 0, "blocked_by_sandbox": 1, "executed": 0, "legit_success": true},
                        {"number": 1, "id": "a", "sandbox": "off", "attempted": 1, "blocked_by_policy": 0, "blocked_by_sandbox": 0, "executed": 1, "legit_success": false}
                    ]}
                }),
            ),
        ]
    }

    fn board() -> GuardBoard {
        GuardBoard::build(&synthetic()).unwrap()
    }

    /// Lanes are read by name, in first-seen order, so a new lane (`crom`) appears with its own totals and skips.
    #[test]
    fn lanes_generic_by_name() {
        let b = board();
        let names: Vec<&str> = b.lanes.iter().map(|l| l.lane.as_str()).collect();
        assert_eq!(names, ["guard_in_process", "guard_off", "crom"]);
        let crom = &b.lanes[2];
        assert_eq!(
            (crom.scenarios_ran, crom.scenarios_skipped, crom.attempted),
            (1, 1, 3)
        );
        assert_eq!(b.scenarios[1].lanes["crom"], "skipped");
    }

    /// Lane totals sum attempts and verdicts; audit coverage reconciles ledger entries against unmediated ones for Guard lanes only.
    #[test]
    fn lane_aggregation_and_audit_coverage() {
        let b = board();
        let on = &b.lanes[0];
        assert_eq!((on.attempted, on.blocked_by_guard, on.executed), (6, 6, 0));
        assert_eq!((on.legit.k, on.legit.n), (3, 3));
        assert_eq!(on.audit_coverage, Some(1.0));
        assert_eq!((on.audit_verified.k, on.audit_verified.n), (3, 3));
        assert_eq!((on.injection_delivered.k, on.injection_delivered.n), (1, 1));
        let off = &b.lanes[1];
        assert!(!off.guard);
        assert_eq!((off.executed, off.no_effect, off.unmediated), (5, 1, 6));
        assert_eq!(off.audit_coverage, None);
        assert_eq!(off.audit_verified.n, 0);
        assert_eq!((off.legit.k, off.legit.n), (2, 3));
        let order: Vec<&str> = b.scenarios.iter().map(|r| r.scenario.as_str()).collect();
        assert_eq!(order, ["G1", "G10", "G15"]);
        assert_eq!(b.scenarios[0].lanes["guard_off"], "2/3");
    }

    /// E1 agreement is per lane, and a cell agrees only if it agrees in every lane.
    #[test]
    fn e1_agreement() {
        let e = board().e1.unwrap();
        assert_eq!(e.cells, 2);
        assert_eq!((e.lanes[0].agreement.k, e.lanes[0].agreement.n), (2, 2));
        assert_eq!((e.lanes[1].agreement.k, e.lanes[1].agreement.n), (1, 2));
        assert!(e.table[0].agrees && !e.table[1].agrees);
        assert_eq!(e.table[1].observed["guard_server"], "executed");
    }

    /// E2 reports unauthorized effects, completion with Wilson intervals, median wall time, and exact McNemar on scenario pairs.
    #[test]
    fn e2_on_off() {
        let e = board().e2.unwrap();
        let (on, off) = (&e.lanes[0], &e.lanes[1]);
        assert_eq!((on.unauthorized_effects, off.unauthorized_effects), (0, 3));
        assert_eq!(
            (off.scenarios_with_effect.k, off.scenarios_with_effect.n),
            (2, 2)
        );
        assert_eq!((off.legit.k, off.legit.n), (1, 2));
        assert_eq!(
            (on.median_wall_ms, off.median_wall_ms),
            (Some(5.0), Some(1.5))
        );
        let m = e.effect_mcnemar.unwrap();
        assert_eq!((m.pairs, m.only_on, m.only_off), (2, 0, 2));
        assert_eq!(m.p_value, mcnemar_exact(0, 2));
        let l = e.legit_mcnemar.unwrap();
        assert_eq!((l.only_on, l.only_off), (1, 0));
    }

    /// An unknown format and a duplicate experiment are errors, not silently ignored.
    #[test]
    fn rejects_unknown_and_duplicate() {
        let mut v = synthetic();
        v.push(input("x.json", json!({"format": "nope"})));
        assert!(GuardBoard::build(&v)
            .unwrap_err()
            .contains("unknown report format"));
        let mut v = synthetic();
        v.push(v[1].clone());
        assert!(GuardBoard::build(&v).unwrap_err().contains("a second E1"));
    }

    /// Deterministic facts ignore wall time, latency, and provenance, skip scenario lanes absent on one side, and flag changed values.
    #[test]
    fn deterministic_facts_compare() {
        let inputs = synthetic();
        let a = board().deterministic_facts(&inputs);
        let mut changed = synthetic();
        changed[2].value["results"]["runs"][0]["wall_ms"] = json!(99.0);
        changed[4].value["results"]["rows"][0]["stats"]["p50_us"] = json!(99.0);
        changed[0].value["meta"]["commit"] = json!("ffff");
        let rows = changed[0].value["scenarios"].as_array_mut().unwrap();
        rows.retain(|r| r["mode"] != "crom");
        let b = GuardBoard::build(&changed)
            .unwrap()
            .deterministic_facts(&changed);
        assert!(
            compare_facts(&b, &a).is_empty(),
            "{:?}",
            compare_facts(&b, &a)
        );
        changed[1].value["results"]["results"][0]["observed"] = json!("policy_deny");
        let c = GuardBoard::build(&changed)
            .unwrap()
            .deterministic_facts(&changed);
        let d = compare_facts(&c, &a);
        assert_eq!(d.len(), 1, "{d:?}");
        assert!(d[0].starts_with("E1 E1-01 guard_in_process"));
    }

    /// Text, Markdown, and JSON renderings of the synthetic reports match their checked-in snapshots.
    #[test]
    fn snapshots_text_md_json() {
        let b = board();
        for (fmt, name) in [
            (Format::Text, "guard-synthetic.txt"),
            (Format::Markdown, "guard-synthetic.md"),
        ] {
            let mut s = String::new();
            b.render(fmt, true, &mut s);
            crate::scoreboard::assert_snapshot(name, &s);
        }
        let json = serde_json::to_string_pretty(&b).unwrap() + "\n";
        crate::scoreboard::assert_snapshot("guard-synthetic.json", &json);
        // Floats may lose their last ulp in a JSON round trip; the shape must survive.
        let back: GuardBoard = serde_json::from_str(&json).unwrap();
        assert_eq!(
            (back.lanes.len(), back.e1.map(|e| e.table)),
            (b.lanes.len(), b.e1.map(|e| e.table))
        );
    }
}
