//! Cross-run scoreboard (M17.4, #293). One aggregation feeds the text,
//! Markdown, and JSON renderers. Metric definitions live in `docs/eval.md`
//! ("Scoreboard"); denominators follow `docs/eval-schema.md`: only
//! `resolved + failed` enter a pass rate, while every attempt's spend counts
//! toward cost per resolved task.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::failure::FailureClass;
use crate::result_schema::{Outcome, OutcomeCounts};
use crate::runner::{EvalResult, EvalStatus};
use crate::schema::{Level, TaskKind};
use crate::security::SecurityAuditSummary;
use crate::stats::{pass_at_k, wilson_interval, Interval};

/// Version of the scoreboard JSON layout.
pub const SCOREBOARD_SCHEMA_VERSION: u32 = 1;

/// One task attempt as loaded from a run directory.
#[derive(Debug, Clone)]
pub struct Attempt {
    pub run_id: String,
    pub result: EvalResult,
    /// `security-summary.json` beside the result, when present.
    pub security: Option<SecurityAuditSummary>,
}

/// How attempts are grouped into scoreboard columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupBy {
    /// `provider/model`.
    #[default]
    Model,
    /// Evaluating checkout's git SHA (12 chars, `+dirty` when dirty).
    Commit,
}

impl std::str::FromStr for GroupBy {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "model" => Ok(GroupBy::Model),
            "commit" => Ok(GroupBy::Commit),
            other => Err(format!("unknown grouping '{other}' (model|commit)")),
        }
    }
}

/// Attempt filters. Empty lists match everything.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Filters {
    /// The `--since` revision as given (informational).
    pub since: Option<String>,
    /// Full SHAs accepted by `--since`; `None` = no commit filter.
    #[serde(skip)]
    pub since_shas: Option<BTreeSet<String>>,
    /// Matches `model` or `provider/model`.
    pub models: Vec<String>,
    pub levels: Vec<Level>,
}

impl Filters {
    fn accepts(&self, a: &Attempt) -> bool {
        let r = &a.result;
        if let Some(shas) = &self.since_shas {
            let sha = r.repro.as_ref().and_then(|m| m.git.sha.as_deref());
            if !sha.is_some_and(|s| shas.contains(s)) {
                return false;
            }
        }
        if !self.models.is_empty()
            && !self
                .models
                .iter()
                .any(|m| *m == r.model || *m == model_key(r))
        {
            return false;
        }
        if !self.levels.is_empty() && !r.level.is_some_and(|l| self.levels.contains(&l)) {
            return false;
        }
        true
    }
}

fn model_key(r: &EvalResult) -> String {
    format!("{}/{}", r.provider, r.model)
}

fn commit_key(r: &EvalResult) -> String {
    match r.repro.as_ref().map(|m| &m.git) {
        Some(g) => match &g.sha {
            Some(sha) => {
                let short: String = sha.chars().take(12).collect();
                if g.dirty {
                    format!("{short}+dirty")
                } else {
                    short
                }
            }
            None => "unknown".into(),
        },
        None => "unknown".into(),
    }
}

/// A pass-rate row: resolved over scored attempts, with everything else
/// counted beside it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateRow {
    pub key: String,
    /// Distinct tasks with at least one attempt.
    pub tasks: usize,
    pub outcomes: OutcomeCounts,
    /// `resolved / (resolved + failed)`.
    pub resolved_rate: Option<f64>,
    /// Wilson 95% interval on `resolved_rate`.
    pub wilson95: Option<Interval>,
    /// Mean over tasks of `c_i / n_i` (unbiased pass@1).
    pub pass_at_1: Option<f64>,
    /// Mean unbiased pass@k with `k` = the smallest per-task sample count,
    /// shown only when every task has `n_i >= 2`.
    pub pass_at_k: Option<PassAtK>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassAtK {
    pub k: usize,
    /// Smallest and largest per-task scored sample count.
    pub n_min: usize,
    pub n_max: usize,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilitySection {
    pub overall: RateRow,
    pub by_level: Vec<RateRow>,
    pub by_category: Vec<RateRow>,
}

/// Efficiency over capability attempts. Optional values are `None` when no
/// attempt reported them; `*_known` says how many did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Efficiency {
    /// Non-skipped capability attempts.
    pub attempts: usize,
    pub resolved: usize,
    /// Attempts with a known `cost_usd`.
    pub cost_known: usize,
    /// Sum of known `cost_usd` over every attempt (failures and errors included).
    pub total_spend_usd: Option<f64>,
    pub cost_per_attempt_usd: Option<f64>,
    /// `total_spend_usd / resolved`; a lower bound when `cost_known < attempts`.
    pub cost_per_resolved_usd: Option<f64>,
    pub median_wall_ms: Option<u64>,
    pub mean_tool_calls: Option<f64>,
    pub mean_round_trips: Option<f64>,
    /// Attempts whose `usage` was reported.
    pub usage_known: usize,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_creation_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// `cache_read / (input + cache_read)` over attempts reporting both.
    pub cache_hit_rate: Option<f64>,
}

/// Reliability over every non-skipped attempt in the group, all kinds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reliability {
    pub attempts: usize,
    pub timeouts: usize,
    pub provider_errors: usize,
    pub infra_errors: usize,
    pub unscorable: usize,
    /// Attempts whose failure class is `tool_loop`.
    pub tool_loops: usize,
    /// Tasks with at least two scored attempts.
    pub repeated_tasks: usize,
    /// Repeated tasks that both resolved and failed.
    pub flaky_tasks: usize,
    /// Mean per-task Bernoulli variance `p(1-p)` over repeated tasks.
    pub mean_task_variance: Option<f64>,
    /// Attempts that entered the repair loop.
    pub repair_attempted: usize,
    pub repair_recovered: usize,
    pub repair_recovery_rate: Option<f64>,
    /// Attempts stopped by a repeated failure or the repair attempt limit.
    pub repeated_failure_stops: usize,
    /// Attempts that finished after a mutation without validating.
    pub unvalidated_completions: usize,
    /// Provider + infra errors exceed 10% of attempts.
    pub unreliable: bool,
}

/// `kind = "security"` results. Never folded into the capability score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecuritySection {
    pub row: RateRow,
    /// Attempts with a `security-summary.json`.
    pub audited: usize,
    pub tool_calls_attempted: usize,
    pub blocked: usize,
    pub executed: usize,
    /// Executions with a decision record / executions.
    pub audit_coverage: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupBoard {
    pub key: String,
    pub runs: usize,
    pub attempts: usize,
    pub capability: Option<CapabilitySection>,
    /// `kind = "harness"` (L0) results; never part of capability rates.
    pub harness: Option<RateRow>,
    pub security: Option<SecuritySection>,
    /// Results from v1/v2 tasks with no `kind`.
    pub unclassified: Option<RateRow>,
    pub efficiency: Option<Efficiency>,
    pub reliability: Reliability,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scoreboard {
    pub schema_version: u32,
    pub group_by: GroupBy,
    pub since: Option<String>,
    pub models: Vec<String>,
    pub levels: Vec<Level>,
    pub groups: Vec<GroupBoard>,
    /// Guard section from `--guard-report` files (#363); kept apart from
    /// every eval number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<crate::guard_board::GuardBoard>,
}

impl Scoreboard {
    /// Aggregate `attempts` that pass `filters`, grouped by `group_by`.
    pub fn build(attempts: &[Attempt], filters: &Filters, group_by: GroupBy) -> Self {
        let mut groups: BTreeMap<String, Vec<&Attempt>> = BTreeMap::new();
        for a in attempts.iter().filter(|a| filters.accepts(a)) {
            let key = match group_by {
                GroupBy::Model => model_key(&a.result),
                GroupBy::Commit => commit_key(&a.result),
            };
            groups.entry(key).or_default().push(a);
        }
        Scoreboard {
            schema_version: SCOREBOARD_SCHEMA_VERSION,
            group_by,
            since: filters.since.clone(),
            models: filters.models.clone(),
            levels: filters.levels.clone(),
            groups: groups
                .into_iter()
                .map(|(key, g)| group_board(key, &g))
                .collect(),
            guard: None,
        }
    }

    /// Adds the Guard section.
    pub fn with_guard(mut self, guard: crate::guard_board::GuardBoard) -> Self {
        self.guard = Some(guard);
        self
    }
}

fn of_kind<'a>(g: &[&'a Attempt], kind: Option<TaskKind>) -> Vec<&'a Attempt> {
    g.iter()
        .copied()
        .filter(|a| a.result.kind == kind)
        .collect()
}

fn group_board(key: String, g: &[&Attempt]) -> GroupBoard {
    let runs: BTreeSet<&str> = g.iter().map(|a| a.run_id.as_str()).collect();
    let cap = of_kind(g, Some(TaskKind::Capability));
    let harness = of_kind(g, Some(TaskKind::Harness));
    let security = of_kind(g, Some(TaskKind::Security));
    let unclassified = of_kind(g, None);
    GroupBoard {
        key,
        runs: runs.len(),
        attempts: g.len(),
        capability: (!cap.is_empty()).then(|| capability_section(&cap)),
        harness: (!harness.is_empty()).then(|| rate_row("harness (L0)".into(), &harness)),
        security: (!security.is_empty()).then(|| security_section(&security)),
        unclassified: (!unclassified.is_empty())
            .then(|| rate_row("unclassified".into(), &unclassified)),
        efficiency: (!cap.is_empty()).then(|| efficiency(&cap)),
        reliability: reliability(g),
    }
}

fn rate_row(key: String, attempts: &[&Attempt]) -> RateRow {
    let outcomes = OutcomeCounts::from_outcomes(attempts.iter().map(|a| a.result.outcome()));
    // Per-task (n_i scored, c_i resolved).
    let mut per_task: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut tasks: BTreeSet<&str> = BTreeSet::new();
    for a in attempts {
        tasks.insert(&a.result.task_id);
        let o = a.result.outcome();
        if o.is_scored() {
            let e = per_task.entry(&a.result.task_id).or_default();
            e.0 += 1;
            if o == Outcome::Resolved {
                e.1 += 1;
            }
        }
    }
    let samples: Vec<(usize, usize)> = per_task.into_values().collect();
    let pass_at_1 = mean(samples.iter().map(|&(n, c)| c as f64 / n as f64));
    let n_min = samples.iter().map(|s| s.0).min();
    let n_max = samples.iter().map(|s| s.0).max();
    let pass_at_k = match (n_min, n_max) {
        (Some(k), Some(n_max)) if k >= 2 => {
            mean(samples.iter().filter_map(|&(n, c)| pass_at_k(n, c, k))).map(|value| PassAtK {
                k,
                n_min: k,
                n_max,
                value,
            })
        }
        _ => None,
    };
    RateRow {
        key,
        tasks: tasks.len(),
        resolved_rate: outcomes.pass_rate(),
        wilson95: wilson_interval(outcomes.resolved, outcomes.scored()),
        outcomes,
        pass_at_1,
        pass_at_k,
    }
}

fn capability_section(cap: &[&Attempt]) -> CapabilitySection {
    let mut by_level: BTreeMap<Option<Level>, Vec<&Attempt>> = BTreeMap::new();
    let mut by_category: BTreeMap<String, Vec<&Attempt>> = BTreeMap::new();
    for a in cap {
        by_level.entry(a.result.level).or_default().push(a);
        by_category
            .entry(
                a.result
                    .category
                    .clone()
                    .unwrap_or_else(|| "uncategorized".into()),
            )
            .or_default()
            .push(a);
    }
    CapabilitySection {
        overall: rate_row("capability".into(), cap),
        by_level: by_level
            .into_iter()
            .map(|(l, g)| rate_row(l.map_or("unleveled", |l| l.as_str()).into(), &g))
            .collect(),
        by_category: by_category
            .into_iter()
            .map(|(c, g)| rate_row(c, &g))
            .collect(),
    }
}

fn efficiency(cap: &[&Attempt]) -> Efficiency {
    let ran: Vec<&EvalResult> = cap
        .iter()
        .map(|a| &a.result)
        .filter(|r| r.outcome() != Outcome::Skipped)
        .collect();
    let resolved = ran
        .iter()
        .filter(|r| r.outcome() == Outcome::Resolved)
        .count();
    let costs: Vec<f64> = ran.iter().filter_map(|r| r.cost_usd).collect();
    let total_spend = (!costs.is_empty()).then(|| costs.iter().sum::<f64>());
    let usages: Vec<&crom_core::TokenUsage> = ran.iter().filter_map(|r| r.usage.as_ref()).collect();
    let sum_tokens = |f: fn(&crom_core::TokenUsage) -> Option<u64>| -> Option<u64> {
        let known: Vec<u64> = usages.iter().filter_map(|u| f(u)).collect();
        (!known.is_empty()).then(|| known.iter().sum())
    };
    let (mut hit_read, mut hit_total) = (0u64, 0u64);
    for u in &usages {
        if let (Some(i), Some(c)) = (u.input_tokens, u.cache_read_input_tokens) {
            hit_read += c;
            hit_total += i + c;
        }
    }
    let mut walls: Vec<u64> = ran.iter().map(|r| r.duration_ms).collect();
    walls.sort_unstable();
    Efficiency {
        attempts: ran.len(),
        resolved,
        cost_known: costs.len(),
        total_spend_usd: total_spend,
        cost_per_attempt_usd: mean(costs.iter().copied()),
        cost_per_resolved_usd: total_spend
            .and_then(|s| (resolved > 0).then(|| s / resolved as f64)),
        median_wall_ms: walls.get(walls.len().saturating_sub(1) / 2).copied(),
        mean_tool_calls: mean(ran.iter().map(|r| r.tool_calls as f64)),
        mean_round_trips: mean(ran.iter().map(|r| r.model_round_trips as f64)),
        usage_known: usages.len(),
        input_tokens: sum_tokens(|u| u.input_tokens),
        output_tokens: sum_tokens(|u| u.output_tokens),
        cache_read_tokens: sum_tokens(|u| u.cache_read_input_tokens),
        cache_creation_tokens: sum_tokens(|u| u.cache_creation_input_tokens),
        reasoning_tokens: sum_tokens(|u| u.reasoning_tokens),
        cache_hit_rate: (hit_total > 0).then(|| hit_read as f64 / hit_total as f64),
    }
}

fn reliability(g: &[&Attempt]) -> Reliability {
    let ran: Vec<&EvalResult> = g
        .iter()
        .map(|a| &a.result)
        .filter(|r| r.outcome() != Outcome::Skipped)
        .collect();
    let outcomes = OutcomeCounts::from_outcomes(ran.iter().map(|r| r.outcome()));
    let mut per_task: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for r in &ran {
        let o = r.outcome();
        if o.is_scored() {
            let e = per_task.entry(&r.task_id).or_default();
            e.0 += 1;
            if o == Outcome::Resolved {
                e.1 += 1;
            }
        }
    }
    let repeated: Vec<(usize, usize)> = per_task.into_values().filter(|s| s.0 >= 2).collect();
    let repair_attempted = ran
        .iter()
        .filter(|r| r.repair.repair_attempt_count > 0)
        .count();
    let repair_recovered = ran
        .iter()
        .filter(|r| r.repair.repair_attempt_count > 0 && r.repair.repair_success_count > 0)
        .count();
    Reliability {
        attempts: ran.len(),
        timeouts: ran
            .iter()
            .filter(|r| r.status == EvalStatus::TimedOut)
            .count(),
        provider_errors: outcomes.provider_error,
        infra_errors: outcomes.infra_error,
        unscorable: outcomes.unscorable,
        tool_loops: ran
            .iter()
            .filter(|r| r.failure_class == Some(FailureClass::ToolLoop))
            .count(),
        repeated_tasks: repeated.len(),
        flaky_tasks: repeated.iter().filter(|&&(n, c)| c > 0 && c < n).count(),
        mean_task_variance: mean(repeated.iter().map(|&(n, c)| {
            let p = c as f64 / n as f64;
            p * (1.0 - p)
        })),
        repair_attempted,
        repair_recovered,
        repair_recovery_rate: (repair_attempted > 0)
            .then(|| repair_recovered as f64 / repair_attempted as f64),
        repeated_failure_stops: ran
            .iter()
            .filter(|r| r.repair.same_error_repeated || r.repair.repair_attempt_limit_exceeded)
            .count(),
        unvalidated_completions: ran
            .iter()
            .filter(|r| r.repair.final_answer_without_validation)
            .count(),
        unreliable: outcomes.unreliable(),
    }
}

fn security_section(sec: &[&Attempt]) -> SecuritySection {
    let audits: Vec<&SecurityAuditSummary> =
        sec.iter().filter_map(|a| a.security.as_ref()).collect();
    let executions: usize = audits.iter().map(|s| s.tool_executions).sum();
    let with_decision: usize = audits.iter().map(|s| s.executions_with_decision).sum();
    SecuritySection {
        row: rate_row("security".into(), sec),
        audited: audits.len(),
        tool_calls_attempted: audits.iter().map(|s| s.attempted).sum(),
        blocked: audits.iter().map(|s| s.blocked).sum(),
        executed: audits.iter().map(|s| s.executed).sum(),
        audit_coverage: (executions > 0).then(|| with_decision as f64 / executions as f64),
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| sum / n as f64)
}

// ---------------------------------------------------------------- loading

/// Load every `result.json` under `runs_dir` (including `rep-N/`
/// repetition sub-runs). `runs` restricts to those top-level run ids.
/// Unreadable results are skipped and returned as warnings.
pub fn load_attempts(
    runs_dir: &Path,
    runs: &[String],
) -> std::io::Result<(Vec<Attempt>, Vec<String>)> {
    let mut attempts = Vec::new();
    let mut warnings = Vec::new();
    if !runs_dir.exists() {
        return Ok((attempts, warnings));
    }
    let mut run_dirs: Vec<PathBuf> = std::fs::read_dir(runs_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    run_dirs.sort();
    for run_dir in run_dirs {
        let run_id = run_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if !runs.is_empty() && !runs.contains(&run_id) {
            continue;
        }
        let mut files = Vec::new();
        collect_results(&run_dir, &mut files);
        files.sort();
        for path in files {
            match crate::result_schema::load_result(&path) {
                Ok(result) => {
                    let security = path
                        .parent()
                        .map(|d| d.join("security-summary.json"))
                        .and_then(|p| std::fs::read_to_string(p).ok())
                        .and_then(|s| serde_json::from_str(&s).ok());
                    attempts.push(Attempt {
                        run_id: run_id.clone(),
                        result,
                        security,
                    });
                }
                Err(e) => warnings.push(format!("{}: {e}", path.display())),
            }
        }
    }
    Ok((attempts, warnings))
}

fn collect_results(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(|e| e.ok()) {
        let p = e.path();
        if p.is_dir() {
            // Never descend into the verification workspace.
            if p.file_name().is_some_and(|n| n == "workspace") {
                continue;
            }
            collect_results(&p, out);
        } else if p.file_name().is_some_and(|n| n == "result.json") {
            out.push(p);
        }
    }
}

/// Full SHAs of `rev` and every commit reachable from `HEAD` but not `rev`.
pub fn commits_since(rev: &str) -> anyhow::Result<BTreeSet<String>> {
    let run = |args: &[&str]| -> anyhow::Result<String> {
        let out = crom_core::git::host_git(std::path::Path::new("."))
            .args(args)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };
    let base = run(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?;
    let mut shas: BTreeSet<String> = run(&["rev-list", &format!("{rev}..HEAD")])?
        .lines()
        .map(str::to_string)
        .collect();
    shas.insert(base.trim().to_string());
    Ok(shas)
}

// -------------------------------------------------------------- rendering

/// Output format for [`Scoreboard::render`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Format {
    #[default]
    Text,
    Markdown,
    Json,
}

impl std::str::FromStr for Format {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Format::Text),
            "md" | "markdown" => Ok(Format::Markdown),
            "json" => Ok(Format::Json),
            other => Err(format!("unknown format '{other}' (text|md|json)")),
        }
    }
}

pub(crate) struct Table<'a> {
    pub(crate) title: String,
    pub(crate) header: Vec<&'a str>,
    pub(crate) rows: Vec<Vec<String>>,
}

impl Table<'_> {
    pub(crate) fn render(&self, fmt: Format, out: &mut String) {
        match fmt {
            Format::Markdown => {
                out.push_str(&format!("### {}\n\n", self.title));
                out.push_str(&format!("| {} |\n", self.header.join(" | ")));
                out.push_str(&format!(
                    "|{}|\n",
                    self.header
                        .iter()
                        .map(|_| "---")
                        .collect::<Vec<_>>()
                        .join("|")
                ));
                for r in &self.rows {
                    out.push_str(&format!("| {} |\n", r.join(" | ")));
                }
                out.push('\n');
            }
            _ => {
                out.push_str(&format!("{}\n", self.title));
                let mut widths: Vec<usize> = self.header.iter().map(|h| h.len()).collect();
                for r in &self.rows {
                    for (w, c) in widths.iter_mut().zip(r) {
                        *w = (*w).max(c.chars().count());
                    }
                }
                let line = |cells: Vec<&str>| -> String {
                    let s: Vec<String> = cells
                        .iter()
                        .zip(&widths)
                        .map(|(c, w)| format!("{c:<w$}"))
                        .collect();
                    format!("  {}\n", s.join("  ").trim_end())
                };
                out.push_str(&line(self.header.clone()));
                for r in &self.rows {
                    out.push_str(&line(r.iter().map(String::as_str).collect()));
                }
                out.push('\n');
            }
        }
    }
}

pub(crate) fn opt_f(v: Option<f64>, digits: usize) -> String {
    v.map_or("-".into(), |x| format!("{x:.digits$}"))
}

fn opt_u(v: Option<u64>) -> String {
    v.map_or("-".into(), |x| x.to_string())
}

fn usd(v: Option<f64>) -> String {
    v.map_or("unknown".into(), |x| format!("${x:.4}"))
}

const RATE_HEADER: [&str; 8] = [
    "row",
    "tasks",
    "resolved/n",
    "rate",
    "wilson95",
    "pass@1",
    "pass@k",
    "excluded",
];

fn rate_cells(r: &RateRow) -> Vec<String> {
    let o = &r.outcomes;
    vec![
        r.key.clone(),
        r.tasks.to_string(),
        format!("{}/{}", o.resolved, o.scored()),
        opt_f(r.resolved_rate, 3),
        r.wilson95
            .map_or("-".into(), |i| format!("[{:.3}, {:.3}]", i.low, i.high)),
        opt_f(r.pass_at_1, 3),
        r.pass_at_k.as_ref().map_or("-".into(), |p| {
            if p.n_min == p.n_max {
                format!("{:.3} (k={}, n={})", p.value, p.k, p.n_min)
            } else {
                format!("{:.3} (k={}, n={}..{})", p.value, p.k, p.n_min, p.n_max)
            }
        }),
        format!(
            "pe={} ie={} sk={} un={}",
            o.provider_error, o.infra_error, o.skipped, o.unscorable
        ),
    ]
}

impl Scoreboard {
    /// Render in `fmt`. All three formats come from the same aggregation.
    pub fn render(&self, fmt: Format) -> String {
        if fmt == Format::Json {
            return serde_json::to_string_pretty(self).unwrap_or_default() + "\n";
        }
        let mut s = String::new();
        if let (Some(g), true) = (&self.guard, self.groups.is_empty()) {
            g.render(fmt, true, &mut s);
            return s;
        }
        self.render_eval(fmt, &mut s);
        if let Some(g) = &self.guard {
            g.render(fmt, false, &mut s);
        }
        s
    }

    fn render_eval(&self, fmt: Format, s: &mut String) {
        let md = fmt == Format::Markdown;
        s.push_str(if md {
            "# Eval scoreboard\n\n"
        } else {
            "Eval scoreboard\n\n"
        });
        let levels: Vec<&str> = self.levels.iter().map(Level::as_str).collect();
        s.push_str(&format!(
            "group by: {}; since: {}; models: {}; levels: {}\n",
            match self.group_by {
                GroupBy::Model => "model",
                GroupBy::Commit => "commit",
            },
            self.since.as_deref().unwrap_or("all"),
            if self.models.is_empty() {
                "all".into()
            } else {
                self.models.join(",")
            },
            if levels.is_empty() {
                "all".into()
            } else {
                levels.join(",")
            },
        ));
        s.push_str(
            "Rates are resolved / (resolved + failed) with Wilson 95% intervals; \
             excluded = provider_error (pe), infra_error (ie), skipped (sk), unscorable (un).\n\n",
        );
        if self.groups.is_empty() {
            s.push_str("No results matched.\n");
            return;
        }
        for g in &self.groups {
            s.push_str(&if md {
                format!("## {}\n\n", g.key)
            } else {
                format!("== {} ==\n\n", g.key)
            });
            s.push_str(&format!("runs: {}, attempts: {}\n\n", g.runs, g.attempts));
            self.render_group(g, fmt, s);
        }
    }

    fn render_group(&self, g: &GroupBoard, fmt: Format, s: &mut String) {
        if let Some(c) = &g.capability {
            let mut rows = vec![rate_cells(&c.overall)];
            rows.extend(c.by_level.iter().map(rate_cells));
            Table {
                title: "Capability by level".into(),
                header: RATE_HEADER.to_vec(),
                rows,
            }
            .render(fmt, s);
            Table {
                title: "Capability by category".into(),
                header: RATE_HEADER.to_vec(),
                rows: c.by_category.iter().map(rate_cells).collect(),
            }
            .render(fmt, s);
        } else {
            s.push_str("No capability results.\n\n");
        }
        let other: Vec<Vec<String>> = [&g.harness, &g.unclassified]
            .into_iter()
            .flatten()
            .map(rate_cells)
            .collect();
        if !other.is_empty() {
            Table {
                title: "Harness and unclassified (not capability)".into(),
                header: RATE_HEADER.to_vec(),
                rows: other,
            }
            .render(fmt, s);
        }
        if let Some(e) = &g.efficiency {
            let cost_note = if e.cost_known < e.attempts {
                format!(" ({} of {} attempts priced)", e.cost_known, e.attempts)
            } else {
                String::new()
            };
            Table {
                title: "Efficiency (capability attempts)".into(),
                header: vec!["metric", "value"],
                rows: vec![
                    vec!["attempts".into(), e.attempts.to_string()],
                    vec!["resolved".into(), e.resolved.to_string()],
                    vec![
                        "total spend".into(),
                        format!("{}{cost_note}", usd(e.total_spend_usd)),
                    ],
                    vec!["cost / attempt".into(), usd(e.cost_per_attempt_usd)],
                    vec!["cost / resolved".into(), usd(e.cost_per_resolved_usd)],
                    vec!["median wall ms".into(), opt_u(e.median_wall_ms)],
                    vec!["mean tool calls".into(), opt_f(e.mean_tool_calls, 2)],
                    vec!["mean round trips".into(), opt_f(e.mean_round_trips, 2)],
                    vec![
                        "usage reported".into(),
                        format!("{}/{}", e.usage_known, e.attempts),
                    ],
                    vec![
                        "tokens in/out".into(),
                        format!("{}/{}", opt_u(e.input_tokens), opt_u(e.output_tokens)),
                    ],
                    vec![
                        "cache read/write".into(),
                        format!(
                            "{}/{}",
                            opt_u(e.cache_read_tokens),
                            opt_u(e.cache_creation_tokens)
                        ),
                    ],
                    vec!["reasoning tokens".into(), opt_u(e.reasoning_tokens)],
                    vec!["cache hit rate".into(), opt_f(e.cache_hit_rate, 3)],
                ],
            }
            .render(fmt, s);
        }
        let r = &g.reliability;
        Table {
            title: "Reliability (all attempts)".into(),
            header: vec!["metric", "value"],
            rows: vec![
                vec!["attempts".into(), r.attempts.to_string()],
                vec!["timeouts".into(), r.timeouts.to_string()],
                vec!["provider errors".into(), r.provider_errors.to_string()],
                vec!["infra errors".into(), r.infra_errors.to_string()],
                vec!["unscorable".into(), r.unscorable.to_string()],
                vec!["tool loops".into(), r.tool_loops.to_string()],
                vec![
                    "flaky / repeated tasks".into(),
                    format!("{}/{}", r.flaky_tasks, r.repeated_tasks),
                ],
                vec!["mean task variance".into(), opt_f(r.mean_task_variance, 3)],
                vec![
                    "repair recovered".into(),
                    format!(
                        "{}/{} ({})",
                        r.repair_recovered,
                        r.repair_attempted,
                        opt_f(r.repair_recovery_rate, 3)
                    ),
                ],
                vec![
                    "repeated-failure stops".into(),
                    r.repeated_failure_stops.to_string(),
                ],
                vec![
                    "unvalidated completions".into(),
                    r.unvalidated_completions.to_string(),
                ],
                vec![
                    "unreliable".into(),
                    if r.unreliable { "YES" } else { "no" }.into(),
                ],
            ],
        }
        .render(fmt, s);
        if let Some(sec) = &g.security {
            let mut cells = rate_cells(&sec.row);
            cells.truncate(5);
            cells.extend([
                format!("{}/{}", sec.audited, sec.row.outcomes.attempted()),
                sec.tool_calls_attempted.to_string(),
                sec.blocked.to_string(),
                sec.executed.to_string(),
                opt_f(sec.audit_coverage, 3),
            ]);
            Table {
                title: "Security (separate; never folded into capability)".into(),
                header: vec![
                    "row",
                    "tasks",
                    "resolved/n",
                    "rate",
                    "wilson95",
                    "audited",
                    "tool calls",
                    "blocked",
                    "executed",
                    "audit coverage",
                ],
                rows: vec![cells],
            }
            .render(fmt, s);
        }
    }
}

/// Compare `actual` with `tests/snapshots/<name>`; `UPDATE_SNAPSHOTS=1` rewrites it.
#[cfg(test)]
pub(crate) fn assert_snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/snapshots")
        .join(name);
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("missing snapshot {name}; rerun with UPDATE_SNAPSHOTS=1"));
    assert_eq!(actual, expected, "snapshot {name} differs");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::test_support::result_with;

    fn attempt(
        run: &str,
        task: &str,
        kind: Option<TaskKind>,
        level: Option<Level>,
        outcome: Outcome,
    ) -> Attempt {
        let mut r = result_with(task, outcome == Outcome::Resolved, None);
        r.kind = kind;
        r.level = level;
        match outcome {
            Outcome::Resolved | Outcome::Failed => {}
            Outcome::ProviderError => {
                r.status = EvalStatus::Errored;
                r.failure_class = Some(FailureClass::ProviderError);
            }
            Outcome::InfraError => {
                r.status = EvalStatus::Errored;
                r.failure_class = Some(FailureClass::HarnessError);
            }
            Outcome::Unscorable => r.failure_class = Some(FailureClass::OracleValidationFailed),
            Outcome::Skipped => r.status = EvalStatus::Skipped,
        }
        r.outcome = Some(outcome);
        Attempt {
            run_id: run.into(),
            result: r,
            security: None,
        }
    }

    fn cap(run: &str, task: &str, level: Level, outcome: Outcome) -> Attempt {
        attempt(run, task, Some(TaskKind::Capability), Some(level), outcome)
    }

    /// Two models; every outcome class, repetitions, harness, security, and a v1/v2 (unclassified) task.
    pub(crate) fn synthetic() -> Vec<Attempt> {
        use Outcome::*;
        let mut v = vec![
            cap("r1", "a", Level::L1, Resolved),
            cap("r2", "a", Level::L1, Resolved),
            cap("r3", "a", Level::L1, Failed),
            cap("r1", "b", Level::L1, Failed),
            cap("r2", "b", Level::L1, Failed),
            cap("r3", "b", Level::L1, Resolved),
            cap("r1", "c", Level::L2, Resolved),
            cap("r2", "c", Level::L2, InfraError),
            cap("r3", "c", Level::L2, Resolved),
            cap("r3", "c", Level::L2, Resolved),
            cap("r1", "d", Level::L2, Skipped),
            cap("r2", "d", Level::L2, Unscorable),
            cap("r3", "d", Level::L2, ProviderError),
            attempt(
                "r1",
                "h",
                Some(TaskKind::Harness),
                Some(Level::L0),
                Resolved,
            ),
            attempt(
                "r2",
                "h",
                Some(TaskKind::Harness),
                Some(Level::L0),
                Resolved,
            ),
            attempt(
                "r1",
                "s",
                Some(TaskKind::Security),
                Some(Level::L1),
                Resolved,
            ),
            attempt("r1", "legacy", None, None, Failed),
        ];
        for (i, a) in v.iter_mut().enumerate() {
            a.result.cost_usd = Some(0.01 * (i % 3 + 1) as f64);
            a.result.duration_ms = 1000 + 100 * i as u64;
            a.result.tool_calls = i % 4;
            a.result.model_round_trips = i % 5;
            a.result.usage = Some(crom_core::TokenUsage {
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cache_creation_input_tokens: Some(0),
                cache_read_input_tokens: Some(500),
                reasoning_tokens: None,
            });
        }
        v[1].result.repair.repair_attempt_count = 2;
        v[1].result.repair.repair_success_count = 1;
        v[2].result.repair.repair_attempt_count = 3;
        v[2].result.repair.repair_attempt_limit_exceeded = true;
        v[3].result.repair.final_answer_without_validation = true;
        v[4].result.status = EvalStatus::TimedOut;
        v[4].result.failure_class = Some(FailureClass::Timeout);
        v[12].result.cost_usd = None;
        v[15].security = Some(SecurityAuditSummary {
            decisions: 3,
            attempted: 3,
            blocked: 2,
            executed: 1,
            failed: 0,
            blocked_by_layer: Default::default(),
            tool_executions: 1,
            executions_with_decision: 1,
            audit_coverage: Some(1.0),
        });
        let mut other = cap("r1", "a", Level::L1, Failed);
        other.result.provider = "deepseek".into();
        other.result.model = "deepseek-chat".into();
        v.push(other);
        v
    }

    fn board() -> Scoreboard {
        Scoreboard::build(&synthetic(), &Filters::default(), GroupBy::Model)
    }

    fn fake(b: &Scoreboard) -> &GroupBoard {
        b.groups.iter().find(|g| g.key == "fake/fake").unwrap()
    }

    /// infra_error, provider_error, skipped, and unscorable stay out of the pass-rate denominator but are counted beside it.
    #[test]
    fn excluded_outcomes_not_in_denominator() {
        let b = board();
        let c = &fake(&b).capability.as_ref().unwrap().overall;
        assert_eq!((c.outcomes.resolved, c.outcomes.scored()), (6, 9));
        assert_eq!(c.resolved_rate, Some(6.0 / 9.0));
        let o = &c.outcomes;
        assert_eq!(
            (o.infra_error, o.provider_error, o.skipped, o.unscorable),
            (1, 1, 1, 1)
        );
        let w = c.wilson95.unwrap();
        assert!(w.low < 6.0 / 9.0 && w.high > 6.0 / 9.0);
    }

    /// Cost per resolved task divides spend over every priced attempt, failures and errors included, by the resolved count.
    #[test]
    fn cost_per_resolved_includes_failed_attempts() {
        let b = board();
        let e = fake(&b).efficiency.as_ref().unwrap();
        // 12 non-skipped capability attempts; attempt 12 (provider_error) is unpriced.
        assert_eq!((e.attempts, e.cost_known, e.resolved), (12, 11, 6));
        let spend: f64 = synthetic()[..13]
            .iter()
            .filter(|a| a.result.status != EvalStatus::Skipped)
            .filter_map(|a| a.result.cost_usd)
            .sum();
        assert!((e.total_spend_usd.unwrap() - spend).abs() < 1e-12);
        assert!((e.cost_per_resolved_usd.unwrap() - spend / 6.0).abs() < 1e-12);
        assert_eq!(e.cache_hit_rate, Some(500.0 / 1500.0));
        assert_eq!(e.reasoning_tokens, None);
    }

    /// Harness, security, and unclassified results each get their own row and never enter capability numbers.
    #[test]
    fn harness_security_unclassified_kept_separate() {
        let b = board();
        let g = fake(&b);
        assert_eq!(g.harness.as_ref().unwrap().outcomes.resolved, 2);
        assert_eq!(g.unclassified.as_ref().unwrap().outcomes.failed, 1);
        let sec = g.security.as_ref().unwrap();
        assert_eq!(
            (sec.row.outcomes.resolved, sec.blocked, sec.executed),
            (1, 2, 1)
        );
        assert_eq!(sec.audit_coverage, Some(1.0));
        let cap = g.capability.as_ref().unwrap();
        assert_eq!(cap.overall.tasks, 4);
        let levels: Vec<&str> = cap.by_level.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(levels, ["L1", "L2"]);
    }

    /// pass@1 averages per-task success fractions and pass@k uses the smallest per-task sample count as k.
    #[test]
    fn pass_at_k_over_repetitions() {
        let b = board();
        let by_level = &fake(&b).capability.as_ref().unwrap().by_level;
        // a: 2/3 resolved, b: 1/3 resolved.
        assert!((by_level[0].pass_at_1.unwrap() - 0.5).abs() < 1e-12);
        let k = by_level[0].pass_at_k.as_ref().unwrap();
        assert_eq!((k.k, k.n_min, k.n_max), (3, 3, 3));
        assert_eq!(k.value, 1.0);
        // L2: task d has no scored attempt and is left out of pass@k.
        let k2 = by_level[1].pass_at_k.as_ref().unwrap();
        assert_eq!((k2.k, k2.value), (3, 1.0));
        assert_eq!(by_level[1].tasks, 2);
    }

    /// Reliability counts timeouts, errors, flaky repeated tasks, repair recovery, and unvalidated completions.
    #[test]
    fn reliability_counts() {
        let b = board();
        let r = &fake(&b).reliability;
        assert_eq!(r.timeouts, 1);
        assert_eq!((r.provider_errors, r.infra_errors, r.unscorable), (1, 1, 1));
        assert_eq!((r.flaky_tasks, r.repeated_tasks), (2, 4));
        assert_eq!((r.repair_recovered, r.repair_attempted), (1, 2));
        assert_eq!(r.repeated_failure_stops, 1);
        assert_eq!(r.unvalidated_completions, 1);
        assert!(r.unreliable, "2 errors in 16 attempts exceeds 10%");
    }

    /// Model, level, and since filters restrict which attempts are aggregated.
    #[test]
    fn filters_apply() {
        let data = synthetic();
        let by_model = Filters {
            models: vec!["deepseek-chat".into()],
            ..Default::default()
        };
        let b = Scoreboard::build(&data, &by_model, GroupBy::Model);
        assert_eq!(b.groups.len(), 1);
        assert_eq!(b.groups[0].key, "deepseek/deepseek-chat");
        let l2 = Filters {
            levels: vec![Level::L2],
            ..Default::default()
        };
        let b = Scoreboard::build(&data, &l2, GroupBy::Model);
        assert!(fake(&b).harness.is_none());
        assert_eq!(fake(&b).capability.as_ref().unwrap().by_level.len(), 1);
        let since = Filters {
            since: Some("main".into()),
            since_shas: Some(BTreeSet::from(["abc".to_string()])),
            ..Default::default()
        };
        // Synthetic results carry no repro git SHA, so nothing matches.
        assert!(Scoreboard::build(&data, &since, GroupBy::Model)
            .groups
            .is_empty());
    }

    /// Text, Markdown, and JSON renderings of the synthetic set match their checked-in snapshots.
    #[test]
    fn snapshots_text_md_json() {
        let b = board();
        assert_snapshot("scoreboard-synthetic.txt", &b.render(Format::Text));
        assert_snapshot("scoreboard-synthetic.md", &b.render(Format::Markdown));
        assert_snapshot("scoreboard-synthetic.json", &b.render(Format::Json));
        let back: Scoreboard =
            serde_json::from_str(&b.render(Format::Json)).expect("json round-trips");
        assert_eq!(back, b);
    }
}
