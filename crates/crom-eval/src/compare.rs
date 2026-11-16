//! Paired run comparison with comparability checks and uncertainty (M17.4,
//! #294). Methodology: `docs/eval.md` ("Comparing runs") and
//! `docs/milestones/MILESTONE_17.md` §8.3. Tasks are paired by id; the
//! resolved-rate delta gets an exact McNemar test and a seeded bootstrap CI,
//! and the verdict stays "inconclusive" for n < 20 pairs, overlapping Wilson
//! intervals, p >= 0.05, or an unreliable run.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::index::IndexEntry;
use crate::result_schema::{Outcome, OutcomeCounts, ReproMetadata};
use crate::schema::TaskKind;
use crate::scoreboard::Attempt;
use crate::stats::{bootstrap_delta_ci, mcnemar_exact, pooled_delta, wilson_interval, Interval};

/// Version of the comparison JSON layout.
pub const COMPARE_SCHEMA_VERSION: u32 = 1;
/// Fewer paired tasks than this is always inconclusive.
pub const MIN_PAIRED_TASKS: usize = 20;
/// Significance level for the McNemar test.
pub const ALPHA: f64 = 0.05;
pub const BOOTSTRAP_RESAMPLES: usize = 2000;
pub const BOOTSTRAP_SEED: u64 = 0x5EED_C0DE;

/// A configuration factor that must match unless it is the experiment's subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Factor {
    /// Provider profile and model.
    Model,
    /// Task `task_version` and content hash.
    TaskVersion,
    ToolPolicy,
    Security,
    Isolation,
    /// Turn, time, mode, and attempt budgets.
    Budgets,
    /// Repair-loop settings.
    Repair,
    /// Prompt template version and prompt hash.
    Prompt,
    /// Reproducibility metadata missing (v1 results), so nothing can be checked.
    Metadata,
}

impl Factor {
    pub const ALL: [Factor; 9] = [
        Factor::Model,
        Factor::TaskVersion,
        Factor::ToolPolicy,
        Factor::Security,
        Factor::Isolation,
        Factor::Budgets,
        Factor::Repair,
        Factor::Prompt,
        Factor::Metadata,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Factor::Model => "model",
            Factor::TaskVersion => "task_version",
            Factor::ToolPolicy => "tool_policy",
            Factor::Security => "security",
            Factor::Isolation => "isolation",
            Factor::Budgets => "budgets",
            Factor::Repair => "repair",
            Factor::Prompt => "prompt",
            Factor::Metadata => "metadata",
        }
    }

    /// The comparable value of this factor, as JSON.
    fn value(&self, m: &ReproMetadata) -> serde_json::Value {
        use serde_json::json;
        match self {
            Factor::Model => json!({"profile": m.provider.profile, "model": m.provider.model}),
            Factor::TaskVersion => {
                json!({"version": m.task.content_version, "hash": m.task.content_hash})
            }
            Factor::ToolPolicy => serde_json::to_value(&m.tool_policy).unwrap_or_default(),
            Factor::Security => {
                // Measured overhead varies run to run; it is not configuration.
                let mut v = serde_json::to_value(&m.security).unwrap_or_default();
                if let Some(o) = v.as_object_mut() {
                    o.remove("agent_sandbox_overhead_ms");
                }
                v
            }
            Factor::Isolation => serde_json::to_value(&m.isolation).unwrap_or_default(),
            Factor::Budgets => json!({
                "max_turns": m.runtime.max_turns,
                "timeout_secs": m.runtime.timeout_secs,
                "mode": m.runtime.mode,
                "attempts": m.runtime.attempts,
            }),
            Factor::Repair => json!({
                "enabled": m.runtime.repair_enabled,
                "max_attempts": m.runtime.repair_max_attempts,
                "require_validation_after_mutation":
                    m.runtime.repair_require_validation_after_mutation,
            }),
            Factor::Prompt => json!({
                "template_version": m.prompt.template_version,
                "variant": m.prompt.variant,
                "hash": m.prompt.hash,
            }),
            Factor::Metadata => serde_json::Value::Null,
        }
    }
}

impl std::str::FromStr for Factor {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Factor::ALL
            .into_iter()
            .find(|f| f.as_str() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Factor::ALL.iter().map(Factor::as_str).collect();
                format!("unknown factor '{s}' ({})", names.join("|"))
            })
    }
}

/// Experiment presets: the factor each one is allowed to vary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Experiment {
    /// Commit A vs B: the harness or agent code changed, nothing else.
    Commit,
    /// Model A vs B.
    Model,
    /// Security configuration on vs off.
    Security,
    /// Repair loop on vs off (or different limits).
    Repair,
    /// Tool-policy variants.
    Policy,
    /// k repetitions of one configuration (noise floor).
    Reps,
}

impl Experiment {
    pub fn allowed(&self) -> &'static [Factor] {
        match self {
            Experiment::Commit | Experiment::Reps => &[],
            Experiment::Model => &[Factor::Model],
            Experiment::Security => &[Factor::Security],
            Experiment::Repair => &[Factor::Repair],
            Experiment::Policy => &[Factor::ToolPolicy],
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Experiment::Commit => "commit",
            Experiment::Model => "model",
            Experiment::Security => "security",
            Experiment::Repair => "repair",
            Experiment::Policy => "policy",
            Experiment::Reps => "reps",
        }
    }
}

impl std::str::FromStr for Experiment {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "commit" => Experiment::Commit,
            "model" => Experiment::Model,
            "security" => Experiment::Security,
            "repair" => Experiment::Repair,
            "policy" => Experiment::Policy,
            "reps" => Experiment::Reps,
            other => {
                return Err(format!(
                    "unknown experiment '{other}' (commit|model|security|repair|policy|reps)"
                ))
            }
        })
    }
}

/// One factor that differs between baseline and candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mismatch {
    pub factor: Factor,
    /// Paired tasks where it differs.
    pub tasks: usize,
    pub example_task: String,
    pub baseline: serde_json::Value,
    pub candidate: serde_json::Value,
    /// The factor is the experiment's subject (or passed via `--allow`).
    pub allowed: bool,
}

/// Compare every factor between the first attempt of each paired task.
pub fn comparability(
    baseline: &BTreeMap<String, Vec<&Attempt>>,
    candidate: &BTreeMap<String, Vec<&Attempt>>,
    allowed: &[Factor],
) -> Vec<Mismatch> {
    let mut found: BTreeMap<Factor, Mismatch> = BTreeMap::new();
    for (task, b) in baseline {
        let Some(c) = candidate.get(task) else {
            continue;
        };
        let (Some(b), Some(c)) = (b.first(), c.first()) else {
            continue;
        };
        let factors: Vec<(Factor, serde_json::Value, serde_json::Value)> =
            match (&b.result.repro, &c.result.repro) {
                (Some(bm), Some(cm)) => Factor::ALL[..8]
                    .iter()
                    .map(|f| (*f, f.value(bm), f.value(cm)))
                    .filter(|(_, bv, cv)| bv != cv)
                    .collect(),
                (bm, cm) => vec![(
                    Factor::Metadata,
                    serde_json::Value::Bool(bm.is_some()),
                    serde_json::Value::Bool(cm.is_some()),
                )],
            };
        for (f, bv, cv) in factors {
            found
                .entry(f)
                .and_modify(|m| m.tasks += 1)
                .or_insert(Mismatch {
                    factor: f,
                    tasks: 1,
                    example_task: task.clone(),
                    baseline: bv,
                    candidate: cv,
                    allowed: allowed.contains(&f),
                });
        }
    }
    found.into_values().collect()
}

/// How a paired task's outcome moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    NewlySolved,
    NewlyFailing,
    /// Mixed outcomes across repetitions on either side.
    Flaky,
    StillPassing,
    StillFailing,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskPair {
    pub task_id: String,
    /// `(resolved, scored)` attempts.
    pub baseline: (usize, usize),
    pub candidate: (usize, usize),
    pub change: Change,
}

/// One side's totals over the paired tasks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SideStats {
    pub run: String,
    pub outcomes: OutcomeCounts,
    pub resolved_rate: Option<f64>,
    pub wilson95: Option<Interval>,
    pub unreliable: bool,
    /// Non-skipped attempts.
    pub attempts: usize,
    pub priced: usize,
    pub spend_usd: Option<f64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub mean_tool_calls: Option<f64>,
    pub mean_wall_ms: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McNemar {
    /// Pairs that went pass -> fail.
    pub b: usize,
    /// Pairs that went fail -> pass.
    pub c: usize,
    pub p_value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Improved,
    Regressed,
    Inconclusive,
}

/// Which task kinds enter the paired statistics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindScope {
    #[default]
    Capability,
    Harness,
    Security,
    All,
}

impl KindScope {
    fn admits(&self, kind: Option<TaskKind>) -> bool {
        match self {
            KindScope::All => true,
            KindScope::Capability => kind == Some(TaskKind::Capability),
            KindScope::Harness => kind == Some(TaskKind::Harness),
            KindScope::Security => kind == Some(TaskKind::Security),
        }
    }
}

impl std::str::FromStr for KindScope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "capability" => KindScope::Capability,
            "harness" => KindScope::Harness,
            "security" => KindScope::Security,
            "all" => KindScope::All,
            other => {
                return Err(format!(
                    "unknown kind '{other}' (capability|harness|security|all)"
                ))
            }
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedComparison {
    pub schema_version: u32,
    pub kind: KindScope,
    pub experiment: Option<Experiment>,
    pub mismatches: Vec<Mismatch>,
    /// Disallowed mismatches were overridden with `--force`.
    pub forced: bool,
    pub baseline: SideStats,
    pub candidate: SideStats,
    pub pairs: Vec<TaskPair>,
    pub unpaired_baseline: Vec<String>,
    pub unpaired_candidate: Vec<String>,
    /// Paired tasks with no scored attempt on at least one side.
    pub unscored: Vec<String>,
    /// Pooled candidate minus baseline resolved rate over scored pairs.
    pub delta: Option<f64>,
    pub bootstrap95: Option<Interval>,
    /// On per-task majority outcomes (resolved in more than half of scored attempts).
    pub mcnemar: McNemar,
    pub cost_delta_usd: Option<f64>,
    pub input_token_delta: Option<i64>,
    pub output_token_delta: Option<i64>,
    pub tool_calls_delta: Option<f64>,
    pub verdict: Verdict,
    pub verdict_reason: String,
}

impl PairedComparison {
    /// Mismatches not covered by the experiment or `--allow`.
    pub fn blocking(&self) -> Vec<&Mismatch> {
        self.mismatches.iter().filter(|m| !m.allowed).collect()
    }

    pub fn tasks_with(&self, change: Change) -> Vec<&str> {
        self.pairs
            .iter()
            .filter(|p| p.change == change)
            .map(|p| p.task_id.as_str())
            .collect()
    }
}

fn by_task(attempts: &[Attempt], scope: KindScope) -> BTreeMap<String, Vec<&Attempt>> {
    let mut m: BTreeMap<String, Vec<&Attempt>> = BTreeMap::new();
    for a in attempts.iter().filter(|a| scope.admits(a.result.kind)) {
        m.entry(a.result.task_id.clone()).or_default().push(a);
    }
    m
}

fn scored(attempts: &[&Attempt]) -> (usize, usize) {
    attempts
        .iter()
        .fold((0, 0), |(c, n), a| match a.result.outcome() {
            Outcome::Resolved => (c + 1, n + 1),
            Outcome::Failed => (c, n + 1),
            _ => (c, n),
        })
}

fn side_stats(run: &str, attempts: &[&Attempt]) -> SideStats {
    let outcomes = OutcomeCounts::from_outcomes(attempts.iter().map(|a| a.result.outcome()));
    let ran: Vec<&&Attempt> = attempts
        .iter()
        .filter(|a| a.result.outcome() != Outcome::Skipped)
        .collect();
    let costs: Vec<f64> = ran.iter().filter_map(|a| a.result.cost_usd).collect();
    let tokens = |f: fn(&crom_core::TokenUsage) -> Option<u64>| -> Option<u64> {
        let v: Vec<u64> = ran
            .iter()
            .filter_map(|a| a.result.usage.as_ref().and_then(f))
            .collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    let mean = |v: Vec<f64>| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    SideStats {
        run: run.to_string(),
        resolved_rate: outcomes.pass_rate(),
        wilson95: wilson_interval(outcomes.resolved, outcomes.scored()),
        unreliable: outcomes.unreliable(),
        outcomes,
        attempts: ran.len(),
        priced: costs.len(),
        spend_usd: (!costs.is_empty()).then(|| costs.iter().sum()),
        input_tokens: tokens(|u| u.input_tokens),
        output_tokens: tokens(|u| u.output_tokens),
        mean_tool_calls: mean(ran.iter().map(|a| a.result.tool_calls as f64).collect()),
        mean_wall_ms: mean(ran.iter().map(|a| a.result.duration_ms as f64).collect()),
    }
}

fn classify(b: (usize, usize), c: (usize, usize)) -> Change {
    let state = |(res, n): (usize, usize)| match res {
        0 => Some(false),
        r if r == n => Some(true),
        _ => None,
    };
    match (state(b), state(c)) {
        (Some(false), Some(true)) => Change::NewlySolved,
        (Some(true), Some(false)) => Change::NewlyFailing,
        (Some(true), Some(true)) => Change::StillPassing,
        (Some(false), Some(false)) => Change::StillFailing,
        _ => Change::Flaky,
    }
}

/// Inputs to [`compare_runs`].
#[derive(Debug, Clone, Default)]
pub struct CompareOptions {
    pub kind: KindScope,
    pub experiment: Option<Experiment>,
    /// Extra factors allowed to differ.
    pub allow: Vec<Factor>,
    pub force: bool,
}

/// Pair `baseline` and `candidate` attempts by task and compute the paired
/// statistics. Never refuses: callers check [`PairedComparison::blocking`].
pub fn compare_runs(
    baseline_run: &str,
    baseline: &[Attempt],
    candidate_run: &str,
    candidate: &[Attempt],
    opts: &CompareOptions,
) -> PairedComparison {
    let b_tasks = by_task(baseline, opts.kind);
    let c_tasks = by_task(candidate, opts.kind);
    let mut allowed: Vec<Factor> = opts.allow.clone();
    if let Some(e) = opts.experiment {
        allowed.extend_from_slice(e.allowed());
    }
    let mismatches = comparability(&b_tasks, &c_tasks, &allowed);

    let b_ids: BTreeSet<&String> = b_tasks.keys().collect();
    let c_ids: BTreeSet<&String> = c_tasks.keys().collect();
    let mut pairs = Vec::new();
    let mut unscored = Vec::new();
    let (mut b_paired, mut c_paired): (Vec<&Attempt>, Vec<&Attempt>) = (Vec::new(), Vec::new());
    for id in b_ids.intersection(&c_ids) {
        let (ba, ca) = (&b_tasks[*id], &c_tasks[*id]);
        b_paired.extend(ba.iter().copied());
        c_paired.extend(ca.iter().copied());
        let (bs, cs) = (scored(ba), scored(ca));
        if bs.1 == 0 || cs.1 == 0 {
            unscored.push((*id).clone());
            continue;
        }
        pairs.push(TaskPair {
            task_id: (*id).clone(),
            baseline: bs,
            candidate: cs,
            change: classify(bs, cs),
        });
    }
    let base = side_stats(baseline_run, &b_paired);
    let cand = side_stats(candidate_run, &c_paired);

    let samples: Vec<crate::stats::PairCounts> =
        pairs.iter().map(|p| (p.baseline, p.candidate)).collect();
    let delta = pooled_delta(&samples);
    let bootstrap95 = bootstrap_delta_ci(&samples, BOOTSTRAP_RESAMPLES, BOOTSTRAP_SEED);
    let majority = |(res, n): (usize, usize)| 2 * res > n;
    let b_count = pairs
        .iter()
        .filter(|p| majority(p.baseline) && !majority(p.candidate))
        .count();
    let c_count = pairs
        .iter()
        .filter(|p| !majority(p.baseline) && majority(p.candidate))
        .count();
    let mcnemar = McNemar {
        b: b_count,
        c: c_count,
        p_value: mcnemar_exact(b_count, c_count),
    };
    let (verdict, verdict_reason) =
        verdict(pairs.len(), &base, &cand, delta, &mcnemar, bootstrap95);
    let diff_u = |b: Option<u64>, c: Option<u64>| Some(c? as i64 - b? as i64);
    // Only a fully priced pair of sides yields a cost delta; partial sums would mislead.
    let fully_priced = |s: &SideStats| s.priced == s.attempts;
    let cost_delta_usd = (fully_priced(&base) && fully_priced(&cand))
        .then(|| base.spend_usd.zip(cand.spend_usd).map(|(b, c)| c - b))
        .flatten();
    let tool_calls_delta = base
        .mean_tool_calls
        .zip(cand.mean_tool_calls)
        .map(|(b, c)| c - b);
    PairedComparison {
        schema_version: COMPARE_SCHEMA_VERSION,
        kind: opts.kind,
        experiment: opts.experiment,
        forced: opts.force && mismatches.iter().any(|m| !m.allowed),
        mismatches,
        cost_delta_usd,
        input_token_delta: diff_u(base.input_tokens, cand.input_tokens),
        output_token_delta: diff_u(base.output_tokens, cand.output_tokens),
        tool_calls_delta,
        baseline: base,
        candidate: cand,
        pairs,
        unpaired_baseline: b_ids.difference(&c_ids).map(|s| (*s).clone()).collect(),
        unpaired_candidate: c_ids.difference(&b_ids).map(|s| (*s).clone()).collect(),
        unscored,
        delta,
        bootstrap95,
        mcnemar,
        verdict,
        verdict_reason,
    }
}

fn verdict(
    n: usize,
    base: &SideStats,
    cand: &SideStats,
    delta: Option<f64>,
    mcnemar: &McNemar,
    ci: Option<Interval>,
) -> (Verdict, String) {
    let inconclusive = |why: String| (Verdict::Inconclusive, why);
    if base.unreliable || cand.unreliable {
        return inconclusive(
            "a run is unreliable (provider + infra errors > 10% of attempts); do not compare"
                .into(),
        );
    }
    if n < MIN_PAIRED_TASKS {
        return inconclusive(format!(
            "n too small to conclude: {n} paired tasks < {MIN_PAIRED_TASKS}"
        ));
    }
    let (Some(delta), Some(ci), Some(bw), Some(cw)) = (delta, ci, base.wilson95, cand.wilson95)
    else {
        return inconclusive("nothing scored on one side".into());
    };
    if mcnemar.p_value >= ALPHA {
        return inconclusive(format!(
            "no significant paired difference (McNemar p = {:.4} >= {ALPHA})",
            mcnemar.p_value
        ));
    }
    if ci.low <= 0.0 && ci.high >= 0.0 {
        return inconclusive(format!(
            "bootstrap 95% CI [{:+.3}, {:+.3}] contains 0",
            ci.low, ci.high
        ));
    }
    if bw.low <= cw.high && cw.low <= bw.high {
        return inconclusive("Wilson 95% intervals of the two runs overlap".into());
    }
    let why = format!(
        "delta {delta:+.3}, McNemar p = {:.4}, bootstrap 95% CI [{:+.3}, {:+.3}], non-overlapping Wilson intervals",
        mcnemar.p_value, ci.low, ci.high
    );
    if delta > 0.0 {
        (Verdict::Improved, why)
    } else {
        (Verdict::Regressed, why)
    }
}

// ------------------------------------------------------- baseline lookup

/// The most recent indexed run evaluated at `sha`, else the most recent run
/// recorded on branch `reference`. Index order is append order.
pub fn resolve_baseline_run(
    entries: &[IndexEntry],
    sha: Option<&str>,
    reference: &str,
) -> Option<String> {
    let last = |pred: &dyn Fn(&IndexEntry) -> bool| {
        entries
            .iter()
            .rev()
            .find(|e| pred(e))
            .map(|e| e.run_id.clone())
    };
    sha.and_then(|s| last(&|e| e.git_sha.as_deref() == Some(s)))
        .or_else(|| last(&|e| e.branch.as_deref() == Some(reference)))
}

/// The most recently indexed run other than `exclude`.
pub fn latest_run_except(entries: &[IndexEntry], exclude: &str) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|e| e.run_id != exclude)
        .map(|e| e.run_id.clone())
}

// --------------------------------------------------------------- render

fn rate(s: &SideStats) -> String {
    match (s.resolved_rate, s.wilson95) {
        (Some(r), Some(w)) => format!(
            "{}/{} = {r:.3} [{:.3}, {:.3}]",
            s.outcomes.resolved,
            s.outcomes.scored(),
            w.low,
            w.high
        ),
        _ => "n/a".into(),
    }
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("unknown".into(), |x| x.to_string())
}

impl PairedComparison {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default() + "\n"
    }

    pub fn to_text(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "Paired comparison: {} -> {} (kind: {}, experiment: {})\n\n",
            self.baseline.run,
            self.candidate.run,
            serde_json::to_value(self.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
            self.experiment.map_or("none", |e| e.as_str())
        ));
        if self.mismatches.is_empty() {
            s.push_str("Comparability: all checked factors match.\n\n");
        } else {
            if self.forced {
                s.push_str(
                    "!!! WARNING: runs are NOT comparable; --force given, results below are not a valid comparison !!!\n",
                );
            }
            s.push_str("Metadata diff:\n");
            for m in &self.mismatches {
                s.push_str(&format!(
                    "  {} {} ({} task(s), e.g. {}): {} != {}\n",
                    if m.allowed {
                        "[expected]"
                    } else {
                        "[MISMATCH]"
                    },
                    m.factor.as_str(),
                    m.tasks,
                    m.example_task,
                    m.baseline,
                    m.candidate
                ));
            }
            s.push('\n');
        }
        s.push_str(&format!(
            "Verdict: {} ({})\n\n",
            match self.verdict {
                Verdict::Improved => "IMPROVED",
                Verdict::Regressed => "REGRESSED",
                Verdict::Inconclusive => "INCONCLUSIVE",
            },
            self.verdict_reason
        ));
        s.push_str(&format!("Paired tasks: {}\n", self.pairs.len()));
        if self.pairs.is_empty() && self.kind != KindScope::All {
            s.push_str("  (no scored pairs of this kind; try --kind all)\n");
        }
        s.push_str(&format!("  baseline:  {}\n", rate(&self.baseline)));
        s.push_str(&format!("  candidate: {}\n", rate(&self.candidate)));
        s.push_str(&format!(
            "  delta: {}  bootstrap 95% CI: {}\n",
            self.delta.map_or("n/a".into(), |d| format!("{d:+.3}")),
            self.bootstrap95
                .map_or("n/a".into(), |i| format!("[{:+.3}, {:+.3}]", i.low, i.high))
        ));
        s.push_str(&format!(
            "  McNemar exact: pass->fail {}, fail->pass {}, p = {:.4}\n\n",
            self.mcnemar.b, self.mcnemar.c, self.mcnemar.p_value
        ));
        for (label, change) in [
            ("Newly solved", Change::NewlySolved),
            ("Newly failing", Change::NewlyFailing),
            ("Flaky", Change::Flaky),
        ] {
            let ids = self.tasks_with(change);
            s.push_str(&format!("{label}: {}\n", ids.len()));
            for p in self.pairs.iter().filter(|p| p.change == change) {
                s.push_str(&format!(
                    "  {}  {}/{} -> {}/{}\n",
                    p.task_id, p.baseline.0, p.baseline.1, p.candidate.0, p.candidate.1
                ));
            }
        }
        s.push_str(&format!(
            "Still passing: {}, still failing: {}\n",
            self.tasks_with(Change::StillPassing).len(),
            self.tasks_with(Change::StillFailing).len()
        ));
        for (label, ids) in [
            ("Only in baseline", &self.unpaired_baseline),
            ("Only in candidate", &self.unpaired_candidate),
            ("Unscored on a side", &self.unscored),
        ] {
            if !ids.is_empty() {
                s.push_str(&format!("{label}: {}\n", ids.join(", ")));
            }
        }
        let (b, c) = (&self.baseline, &self.candidate);
        s.push_str("\nCost and usage (paired tasks, all non-skipped attempts):\n");
        s.push_str(&format!(
            "  spend USD: {} ({} priced) -> {} ({} priced), delta {}\n",
            opt(b.spend_usd.map(|v| format!("{v:.4}"))),
            b.priced,
            opt(c.spend_usd.map(|v| format!("{v:.4}"))),
            c.priced,
            opt(self.cost_delta_usd.map(|v| format!("{v:+.4}")))
        ));
        s.push_str(&format!(
            "  input tokens: {} -> {} ({}); output tokens: {} -> {} ({})\n",
            opt(b.input_tokens),
            opt(c.input_tokens),
            opt(self.input_token_delta.map(|v| format!("{v:+}"))),
            opt(b.output_tokens),
            opt(c.output_tokens),
            opt(self.output_token_delta.map(|v| format!("{v:+}")))
        ));
        s.push_str(&format!(
            "  mean tool calls: {} -> {} ({})\n",
            opt(b.mean_tool_calls.map(|v| format!("{v:.2}"))),
            opt(c.mean_tool_calls.map(|v| format!("{v:.2}"))),
            opt(self.tool_calls_delta.map(|v| format!("{v:+.2}")))
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::result_schema::*;
    use crate::runner::test_support::result_with;

    fn repro(task: &str) -> ReproMetadata {
        ReproMetadata {
            git: GitMeta::default(),
            task: TaskMeta {
                id: task.into(),
                schema_version: 3,
                content_version: Some("1.0.0".into()),
                content_hash: Some("sha256:aa".into()),
            },
            provider: ProviderMeta {
                profile: "deepseek".into(),
                model: "deepseek-chat".into(),
                base_url_host: None,
            },
            tool_policy: ToolPolicyMeta {
                allowed_tools: vec![],
                builtin_tools: serde_json::json!({"bash": true}),
            },
            security: SecurityMeta {
                side_effects: serde_json::json!("default"),
                approvals: "auto".into(),
                agent_sandbox: "off".into(),
                agent_sandbox_overhead_ms: None,
            },
            isolation: IsolationMeta {
                backend: "local".into(),
                network: false,
                cpu_limit: None,
                memory_limit: None,
                container_image: None,
            },
            runtime: RuntimeMeta {
                max_turns: 20,
                timeout_secs: 300,
                mode: "act".into(),
                attempts: 1,
                repair_enabled: false,
                repair_max_attempts: 3,
                repair_require_validation_after_mutation: false,
            },
            prompt: PromptMeta {
                template_version: PROMPT_TEMPLATE_VERSION.into(),
                variant: None,
                hash: "sha256:pp".into(),
            },
            repetition: 0,
            seed: 0,
            host: HostInfo::default(),
            crom_version: "0.0.0".into(),
            pricing_source_version: None,
        }
    }

    fn att(task: &str, resolved: bool) -> Attempt {
        let mut r = result_with(task, resolved, None);
        r.kind = Some(TaskKind::Capability);
        r.outcome = Some(if resolved {
            Outcome::Resolved
        } else {
            Outcome::Failed
        });
        r.repro = Some(repro(task));
        r.cost_usd = Some(0.01);
        Attempt {
            run_id: "r".into(),
            result: r,
            security: None,
        }
    }

    /// Build one side from per-task outcome lists (one entry per repetition).
    fn side(tasks: &[(&str, &[bool])]) -> Vec<Attempt> {
        tasks
            .iter()
            .flat_map(|(id, outs)| outs.iter().map(move |&o| att(id, o)))
            .collect()
    }

    fn ids(n: usize, prefix: &str) -> Vec<String> {
        (0..n).map(|i| format!("{prefix}{i:02}")).collect()
    }

    fn run(base: &[Attempt], cand: &[Attempt], opts: &CompareOptions) -> PairedComparison {
        compare_runs("base", base, "cand", cand, opts)
    }

    /// Identical configurations produce no mismatches.
    #[test]
    fn identical_metadata_is_comparable() {
        let b = side(&[("a", &[true])]);
        let c = side(&[("a", &[false])]);
        assert!(run(&b, &c, &CompareOptions::default())
            .mismatches
            .is_empty());
    }

    /// A model change blocks a plain comparison but is expected under the model experiment preset.
    #[test]
    fn model_change_needs_model_experiment() {
        let b = side(&[("a", &[true])]);
        let mut c = side(&[("a", &[true])]);
        c[0].result.repro.as_mut().unwrap().provider.model = "gpt-4.1-mini".into();
        let plain = run(&b, &c, &CompareOptions::default());
        assert_eq!(plain.blocking().len(), 1);
        assert_eq!(plain.blocking()[0].factor, Factor::Model);
        let exp = run(
            &b,
            &c,
            &CompareOptions {
                experiment: Some(Experiment::Model),
                ..Default::default()
            },
        );
        assert!(exp.blocking().is_empty());
        assert!(exp.mismatches[0].allowed);
    }

    /// Budgets, task versions, prompt, isolation, and tool policy differences are each detected; sandbox overhead is ignored.
    #[test]
    fn each_factor_is_detected() {
        let b = side(&[("a", &[true]), ("b", &[true])]);
        let mut c = side(&[("a", &[true]), ("b", &[true])]);
        {
            let m = c[0].result.repro.as_mut().unwrap();
            m.runtime.max_turns = 40;
            m.task.content_version = Some("1.1.0".into());
            m.prompt.hash = "sha256:other".into();
            m.isolation.backend = "podman".into();
            m.tool_policy.allowed_tools = vec!["Read".into()];
            m.security.agent_sandbox_overhead_ms = Some(3.5);
        }
        c[1].result.repro.as_mut().unwrap().runtime.repair_enabled = true;
        let r = run(&b, &c, &CompareOptions::default());
        let factors: Vec<Factor> = r.mismatches.iter().map(|m| m.factor).collect();
        assert_eq!(
            factors,
            [
                Factor::TaskVersion,
                Factor::ToolPolicy,
                Factor::Isolation,
                Factor::Budgets,
                Factor::Repair,
                Factor::Prompt
            ]
        );
        assert_eq!(r.mismatches[0].example_task, "a");
        let allowed = run(
            &b,
            &c,
            &CompareOptions {
                experiment: Some(Experiment::Repair),
                allow: vec![Factor::TaskVersion],
                ..Default::default()
            },
        );
        assert_eq!(allowed.blocking().len(), 4);
    }

    /// Results without reproducibility metadata cannot be checked and are flagged.
    #[test]
    fn missing_metadata_is_a_mismatch() {
        let b = side(&[("a", &[true])]);
        let mut c = side(&[("a", &[true])]);
        c[0].result.repro = None;
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.blocking()[0].factor, Factor::Metadata);
    }

    /// 24 paired tasks, 6 -> 18 resolved with no pass->fail: hand-computed McNemar p = 2/4096, non-overlapping Wilson intervals, verdict improved.
    #[test]
    fn clear_improvement_is_detected() {
        let names = ids(24, "t");
        let b: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 6))
            .collect();
        let c: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 18))
            .collect();
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!((r.mcnemar.b, r.mcnemar.c), (0, 12));
        assert!((r.mcnemar.p_value - 2.0 / 4096.0).abs() < 1e-12);
        assert!((r.delta.unwrap() - 0.5).abs() < 1e-12);
        assert_eq!(r.tasks_with(Change::NewlySolved).len(), 12);
        assert_eq!(r.verdict, Verdict::Improved, "{}", r.verdict_reason);
        let ci = r.bootstrap95.unwrap();
        assert!(ci.low > 0.0 && ci.high <= 1.0, "{ci:?}");
        assert_eq!(r.cost_delta_usd, Some(0.0));
        assert!(r.to_text().contains("Verdict: IMPROVED"));
    }

    /// The mirror image is a regression.
    #[test]
    fn clear_regression_is_detected() {
        let names = ids(24, "t");
        let b: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 18))
            .collect();
        let c: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 6))
            .collect();
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.verdict, Verdict::Regressed, "{}", r.verdict_reason);
        assert_eq!(r.tasks_with(Change::NewlyFailing).len(), 12);
    }

    /// Fewer than 20 paired tasks is reported as too small to conclude even when every task flips.
    #[test]
    fn small_n_is_inconclusive() {
        let names = ids(10, "t");
        let b: Vec<Attempt> = names.iter().map(|id| att(id, false)).collect();
        let c: Vec<Attempt> = names.iter().map(|id| att(id, true)).collect();
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.verdict, Verdict::Inconclusive);
        assert!(
            r.verdict_reason.contains("n too small")
                && r.verdict_reason.contains("10 paired tasks"),
            "{}",
            r.verdict_reason
        );
        assert!(r.to_text().contains("INCONCLUSIVE (n too small"));
    }

    /// 20 pairs with b = 1, c = 6 gives the hand-computed p = 0.125 and stays inconclusive.
    #[test]
    fn small_effect_is_inconclusive() {
        let names = ids(20, "t");
        // Baseline passes t00..t09; candidate drops t00 and gains t10..t15.
        let b: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 10))
            .collect();
        let c: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, (1..16).contains(&i)))
            .collect();
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!((r.mcnemar.b, r.mcnemar.c), (1, 6));
        assert!((r.mcnemar.p_value - 0.125).abs() < 1e-12);
        assert_eq!(r.verdict, Verdict::Inconclusive);
        assert!(r.verdict_reason.contains("McNemar p = 0.1250"));
    }

    /// Repetitions classify mixed tasks as flaky; one-sided and unscored tasks are listed apart from the pairs.
    #[test]
    fn flaky_unpaired_and_unscored() {
        let b = side(&[("f", &[true, false]), ("only_b", &[true]), ("u", &[true])]);
        let mut c = side(&[("f", &[true, true]), ("only_c", &[false]), ("u", &[false])]);
        c[3].result.outcome = Some(Outcome::InfraError);
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.tasks_with(Change::Flaky), ["f"]);
        assert_eq!(r.unpaired_baseline, ["only_b"]);
        assert_eq!(r.unpaired_candidate, ["only_c"]);
        assert_eq!(r.unscored, ["u"]);
        // Majority: f is 1/2 (not a majority) -> 2/2, so it counts as fail -> pass.
        assert_eq!((r.mcnemar.b, r.mcnemar.c), (0, 1));
    }

    /// Cost delta is unknown unless both sides are fully priced; token deltas need usage on both sides.
    #[test]
    fn cost_and_token_deltas_are_unknown_aware() {
        let b = side(&[("a", &[true])]);
        let mut c = side(&[("a", &[true])]);
        c[0].result.cost_usd = None;
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.cost_delta_usd, None);
        assert_eq!(r.input_token_delta, None);
        let mut c2 = side(&[("a", &[true])]);
        c2[0].result.cost_usd = Some(0.05);
        c2[0].result.usage = Some(crom_core::TokenUsage {
            input_tokens: Some(10),
            ..Default::default()
        });
        let mut b2 = side(&[("a", &[true])]);
        b2[0].result.usage = Some(crom_core::TokenUsage {
            input_tokens: Some(4),
            ..Default::default()
        });
        let r = run(&b2, &c2, &CompareOptions::default());
        assert!((r.cost_delta_usd.unwrap() - 0.04).abs() < 1e-12);
        assert_eq!(r.input_token_delta, Some(6));
        assert!(r.to_text().contains("delta +0.0400"));
    }

    /// Harness tasks are excluded under the default capability scope and included with --kind all.
    #[test]
    fn kind_scope_filters_pairs() {
        let mut b = side(&[("h", &[true]), ("c", &[true])]);
        let mut c = side(&[("h", &[true]), ("c", &[false])]);
        b[0].result.kind = Some(TaskKind::Harness);
        c[0].result.kind = Some(TaskKind::Harness);
        assert_eq!(run(&b, &c, &CompareOptions::default()).pairs.len(), 1);
        let all = CompareOptions {
            kind: KindScope::All,
            ..Default::default()
        };
        assert_eq!(run(&b, &c, &all).pairs.len(), 2);
    }

    /// An unreliable side forces an inconclusive verdict.
    #[test]
    fn unreliable_run_is_not_compared() {
        let names = ids(24, "t");
        let b: Vec<Attempt> = names.iter().map(|id| att(id, false)).collect();
        let mut c: Vec<Attempt> = names.iter().map(|id| att(id, true)).collect();
        for a in c.iter_mut().take(5) {
            a.result.outcome = Some(Outcome::ProviderError);
        }
        let r = run(&b, &c, &CompareOptions::default());
        assert_eq!(r.verdict, Verdict::Inconclusive);
        assert!(r.verdict_reason.contains("unreliable"));
    }

    /// Text and JSON renderings of the 24-task improvement fixture match their checked-in snapshots.
    #[test]
    fn snapshots_text_json() {
        let names = ids(24, "t");
        let b: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 6))
            .collect();
        let mut c: Vec<Attempt> = names
            .iter()
            .enumerate()
            .map(|(i, id)| att(id, i < 18))
            .collect();
        c[0].result.repro.as_mut().unwrap().provider.model = "gpt-4.1-mini".into();
        let r = compare_runs(
            "base",
            &b,
            "cand",
            &c,
            &CompareOptions {
                experiment: Some(Experiment::Model),
                ..Default::default()
            },
        );
        assert_snapshot("compare-improved.txt", &r.to_text());
        assert_snapshot("compare-improved.json", &r.to_json());
        // JSON parses back; floats are compared via the snapshot, not bit-for-bit.
        let back: PairedComparison = serde_json::from_str(&r.to_json()).unwrap();
        assert_eq!((back.verdict, &back.pairs), (r.verdict, &r.pairs));
    }

    fn assert_snapshot(name: &str, actual: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/snapshots")
            .join(name);
        if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
            std::fs::write(&path, actual).unwrap();
        }
        let expected = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("missing snapshot {name}; rerun with UPDATE_SNAPSHOTS=1"));
        assert_eq!(actual, expected, "snapshot {name} differs");
    }

    fn entry(run: &str, sha: &str, branch: &str) -> IndexEntry {
        serde_json::from_value(serde_json::json!({
            "run_id": run, "timestamp": "t", "git_sha": sha, "branch": branch,
            "dirty_worktree": false, "suite": null, "task_id": "a", "adapter": "internal",
            "isolation": "local", "provider": "fake", "model": "fake", "resolved": true,
            "failure_class": null, "cost_usd": null, "wall_time_ms": 0, "tool_call_count": 0,
            "patch_lines_added": 0, "patch_lines_removed": 0, "category": null, "tags": [],
            "capabilities": [], "repetition": 0, "quarantined": false
        }))
        .unwrap()
    }

    /// The baseline for --since is the latest run at the ref's SHA, else the latest run on that branch.
    #[test]
    fn resolves_baseline_from_index() {
        let idx = vec![
            entry("r1", "aaa", "main"),
            entry("r2", "bbb", "main"),
            entry("r3", "aaa", "main"),
            entry("r4", "ccc", "feature"),
        ];
        assert_eq!(
            resolve_baseline_run(&idx, Some("aaa"), "main").as_deref(),
            Some("r3")
        );
        assert_eq!(
            resolve_baseline_run(&idx, Some("zzz"), "main").as_deref(),
            Some("r3")
        );
        assert_eq!(resolve_baseline_run(&idx, None, "nope"), None);
        assert_eq!(latest_run_except(&idx, "r3").as_deref(), Some("r4"));
        assert_eq!(latest_run_except(&idx, "r4").as_deref(), Some("r3"));
    }
}
