//! Human-readable reports and machine-readable run summaries.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::artifacts::RunDir;
use crate::fixture::PatchInfo;
use crate::oracle::OracleReport;
use crate::runner::{EvalResult, EvalStatus};
use crate::score::EvalScore;
use crate::task::EvalTask;

/// Render the per-task `report.md`.
#[allow(clippy::too_many_arguments)]
pub fn render_task_report(
    task: &EvalTask,
    errored: bool,
    run_summary: &str,
    patch: &PatchInfo,
    oracle: &OracleReport,
    score: &EvalScore,
    status: EvalStatus,
) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {} — {}\n\n", task.id, task.title));
    s.push_str(&format!("- status: **{status:?}**\n"));
    s.push_str(&format!("- provider summary: {run_summary}\n"));
    s.push_str(&format!("- errored: {errored}\n"));
    if !task.tags.is_empty() {
        s.push_str(&format!("- tags: {}\n", task.tags.join(", ")));
    }
    s.push('\n');

    s.push_str("## Score\n\n");
    s.push_str(&format!("- **total: {:.3}**\n", score.total));
    s.push_str(&format!("- correctness: {:.3}\n", score.correctness));
    s.push_str(&format!("- patch_quality: {:.3}\n", score.patch_quality));
    s.push_str(&format!(
        "- process_quality: {:.3}\n",
        score.process_quality
    ));
    s.push_str(&format!("- efficiency: {:.3}\n\n", score.efficiency));
    if !score.notes.is_empty() {
        s.push_str("Notes:\n\n");
        for note in &score.notes {
            s.push_str(&format!("- {note}\n"));
        }
        s.push('\n');
    }

    s.push_str("## Oracle checks\n\n");
    s.push_str(&format!(
        "{} of {} checks passed.\n\n",
        oracle.checks_passed(),
        oracle.checks_total()
    ));
    if !oracle.commands.is_empty() {
        s.push_str("### Commands\n\n");
        for c in &oracle.commands {
            let mark = if c.success { "✓" } else { "✗" };
            s.push_str(&format!(
                "- {mark} `{}` (exit {:?}{})\n",
                c.command,
                c.exit_code,
                if c.timed_out { ", timed out" } else { "" }
            ));
        }
        s.push('\n');
    }
    if !oracle.assertions.is_empty() {
        s.push_str("### Assertions\n\n");
        for a in &oracle.assertions {
            let mark = if a.passed { "✓" } else { "✗" };
            s.push_str(&format!("- {mark} {} — {}\n", a.description, a.detail));
        }
        s.push('\n');
    }
    if !oracle.behavior.is_empty() {
        s.push_str("### Behavior\n\n");
        for b in &oracle.behavior {
            let mark = if b.passed { "✓" } else { "✗" };
            s.push_str(&format!("- {mark} {} — {}\n", b.description, b.detail));
        }
        s.push('\n');
    }

    s.push_str("## Patch\n\n");
    if patch.is_empty() {
        s.push_str("No changes were made.\n\n");
    } else {
        s.push_str(&format!(
            "{} file(s) changed, {} insertion(s), {} deletion(s).\n\n```\n{}\n```\n\n",
            patch.changed_files.len(),
            patch.insertions,
            patch.deletions,
            patch.stat.trim_end()
        ));
    }

    s.push_str("Artifacts: `events.jsonl`, `transcript.jsonl`, `final.patch`, `command-output/`, `workspace/`.\n");
    s
}

/// One task's line in a run summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub title: String,
    pub status: EvalStatus,
    pub score: f64,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub tool_calls: usize,
    pub turns_used: usize,
    pub duration_ms: u64,
    pub provider: String,
    // M15 additions.
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub failure_class: Option<crate::failure::FailureClass>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub adapter: String,
    #[serde(default)]
    pub isolation: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub quarantined: bool,
    #[serde(default)]
    pub behavior_warnings: usize,
}

/// A resolved-rate aggregation bucket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggBucket {
    pub key: String,
    pub total: usize,
    pub resolved: usize,
}

/// Machine-readable summary of a whole run (`summary.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub provider: String,
    pub total: usize,
    pub passed: usize,
    pub partial: usize,
    pub failed: usize,
    pub errored: usize,
    pub timed_out: usize,
    pub skipped: usize,
    pub mean_score: f64,
    pub tasks: Vec<TaskSummary>,
    // M15 additions.
    #[serde(default)]
    pub resolved: usize,
    #[serde(default)]
    pub quarantined: usize,
    #[serde(default)]
    pub failure_classes: std::collections::BTreeMap<String, usize>,
    #[serde(default)]
    pub by_category: Vec<AggBucket>,
    #[serde(default)]
    pub by_tag: Vec<AggBucket>,
    #[serde(default)]
    pub by_capability: Vec<AggBucket>,
    #[serde(default)]
    pub by_adapter: Vec<AggBucket>,
    #[serde(default)]
    pub by_isolation: Vec<AggBucket>,
    #[serde(default)]
    pub by_provider: Vec<AggBucket>,
    #[serde(default)]
    pub by_model: Vec<AggBucket>,
}

/// Aggregate resolved-rate by a key extractor (possibly multi-valued).
fn aggregate_by<'a, F>(tasks: &'a [TaskSummary], key_of: F) -> Vec<AggBucket>
where
    F: Fn(&'a TaskSummary) -> Vec<String>,
{
    use std::collections::BTreeMap;
    let mut map: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for t in tasks {
        if matches!(t.status, EvalStatus::Skipped) {
            continue;
        }
        for key in key_of(t) {
            let e = map.entry(key).or_insert((0, 0));
            e.0 += 1;
            if t.resolved {
                e.1 += 1;
            }
        }
    }
    map.into_iter()
        .map(|(key, (total, resolved))| AggBucket {
            key,
            total,
            resolved,
        })
        .collect()
}

impl RunSummary {
    pub fn from_results(run_id: &str, provider: &str, results: &[EvalResult]) -> Self {
        let mut summary = RunSummary {
            run_id: run_id.to_string(),
            provider: provider.to_string(),
            total: results.len(),
            passed: 0,
            partial: 0,
            failed: 0,
            errored: 0,
            timed_out: 0,
            skipped: 0,
            mean_score: 0.0,
            tasks: Vec::new(),
            resolved: 0,
            quarantined: 0,
            failure_classes: std::collections::BTreeMap::new(),
            by_category: Vec::new(),
            by_tag: Vec::new(),
            by_capability: Vec::new(),
            by_adapter: Vec::new(),
            by_isolation: Vec::new(),
            by_provider: Vec::new(),
            by_model: Vec::new(),
        };
        let mut score_sum = 0.0;
        for r in results {
            match r.status {
                EvalStatus::Passed => summary.passed += 1,
                EvalStatus::Partial => summary.partial += 1,
                EvalStatus::Failed => summary.failed += 1,
                EvalStatus::Errored => summary.errored += 1,
                EvalStatus::TimedOut => summary.timed_out += 1,
                EvalStatus::Skipped => summary.skipped += 1,
            }
            if r.resolved {
                summary.resolved += 1;
            }
            if r.quarantined {
                summary.quarantined += 1;
            }
            if let Some(fc) = r.failure_class {
                *summary
                    .failure_classes
                    .entry(fc.key().to_string())
                    .or_insert(0) += 1;
            }
            score_sum += r.score.total;
            summary.tasks.push(TaskSummary {
                task_id: r.task_id.clone(),
                title: r.title.clone(),
                status: r.status,
                score: r.score.total,
                checks_passed: r.checks_passed,
                checks_total: r.checks_total,
                tool_calls: r.tool_calls,
                turns_used: r.turns_used,
                duration_ms: r.duration_ms,
                provider: r.provider.clone(),
                resolved: r.resolved,
                failure_class: r.failure_class,
                category: r.category.clone(),
                tags: r.tags.clone(),
                capabilities: r.capabilities.clone(),
                adapter: r.adapter.clone(),
                isolation: r.isolation.clone(),
                model: r.model.clone(),
                quarantined: r.quarantined,
                behavior_warnings: r.behavior_warnings.len(),
            });
        }
        if !results.is_empty() {
            summary.mean_score = (score_sum / results.len() as f64 * 1000.0).round() / 1000.0;
        }
        // Aggregations (exclude skipped tasks).
        summary.by_category = aggregate_by(&summary.tasks, |t| {
            vec![t.category.clone().unwrap_or_else(|| "uncategorized".into())]
        });
        summary.by_tag = aggregate_by(&summary.tasks, |t| t.tags.clone());
        summary.by_capability = aggregate_by(&summary.tasks, |t| t.capabilities.clone());
        summary.by_adapter = aggregate_by(&summary.tasks, |t| vec![t.adapter.clone()]);
        summary.by_isolation = aggregate_by(&summary.tasks, |t| vec![t.isolation.clone()]);
        summary.by_provider = aggregate_by(&summary.tasks, |t| vec![t.provider.clone()]);
        summary.by_model = aggregate_by(&summary.tasks, |t| vec![t.model.clone()]);
        summary
    }

    /// Render a Markdown summary table.
    pub fn to_markdown(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("# Eval run `{}`\n\n", self.run_id));
        s.push_str(&format!("- provider: **{}**\n", self.provider));
        s.push_str(&format!(
            "- resolved: **{}/{}**\n",
            self.resolved, self.total
        ));
        s.push_str(&format!(
            "- {} tasks: {} passed, {} partial, {} failed, {} errored, {} timed out, {} skipped\n",
            self.total,
            self.passed,
            self.partial,
            self.failed,
            self.errored,
            self.timed_out,
            self.skipped
        ));
        if self.quarantined > 0 {
            s.push_str(&format!("- quarantined: {}\n", self.quarantined));
        }
        s.push_str(&format!("- mean score: {:.3}\n\n", self.mean_score));

        s.push_str("| task | status | resolved | score | checks | tools | turns | ms |\n");
        s.push_str("|------|--------|----------|-------|--------|-------|-------|----|\n");
        for t in &self.tasks {
            s.push_str(&format!(
                "| {} | {} | {} | {:.3} | {}/{} | {} | {} | {} |\n",
                t.task_id,
                t.status.symbol().trim(),
                if t.resolved { "yes" } else { "no" },
                t.score,
                t.checks_passed,
                t.checks_total,
                t.tool_calls,
                t.turns_used,
                t.duration_ms
            ));
        }
        s.push('\n');

        if !self.failure_classes.is_empty() {
            s.push_str("## Failure classes\n\n");
            for (k, v) in &self.failure_classes {
                s.push_str(&format!("- {k}: {v}\n"));
            }
            s.push('\n');
        }

        render_buckets(&mut s, "By category", &self.by_category);
        render_buckets(&mut s, "By capability", &self.by_capability);
        render_buckets(&mut s, "By tag", &self.by_tag);
        render_buckets(&mut s, "By adapter", &self.by_adapter);
        render_buckets(&mut s, "By isolation", &self.by_isolation);
        render_buckets(&mut s, "By provider", &self.by_provider);
        render_buckets(&mut s, "By model", &self.by_model);

        // Behavior summary: passed-but-risky, failed-without-validation, etc.
        let risky: Vec<&TaskSummary> = self
            .tasks
            .iter()
            .filter(|t| t.resolved && t.behavior_warnings > 0)
            .collect();
        if !risky.is_empty() {
            s.push_str("## Resolved but risky behavior\n\n");
            for t in risky {
                s.push_str(&format!(
                    "- {} ({} warning(s))\n",
                    t.task_id, t.behavior_warnings
                ));
            }
            s.push('\n');
        }
        s
    }
}

/// Render a resolved-rate bucket table when non-empty.
fn render_buckets(s: &mut String, title: &str, buckets: &[AggBucket]) {
    if buckets.is_empty() {
        return;
    }
    s.push_str(&format!("## {title}\n\n"));
    s.push_str("| key | resolved | total | rate |\n");
    s.push_str("|-----|----------|-------|------|\n");
    for b in buckets {
        let rate = if b.total > 0 {
            b.resolved as f64 / b.total as f64
        } else {
            0.0
        };
        s.push_str(&format!(
            "| {} | {} | {} | {:.2} |\n",
            b.key, b.resolved, b.total, rate
        ));
    }
    s.push('\n');
}

/// Write `summary.json` and `summary.md` for a run.
pub fn write_summaries(
    run_dir: &RunDir,
    provider: &str,
    results: &[EvalResult],
) -> std::io::Result<RunSummary> {
    let summary = RunSummary::from_results(&run_dir.run_id, provider, results);
    let json = serde_json::to_string_pretty(&summary).map_err(std::io::Error::other)?;
    std::fs::write(run_dir.summary_json_path(), json)?;
    std::fs::write(run_dir.summary_md_path(), summary.to_markdown())?;
    Ok(summary)
}

/// Load a `summary.json` from a run directory.
pub fn load_summary(run_dir: &Path) -> std::io::Result<RunSummary> {
    let path = if run_dir.is_dir() {
        run_dir.join("summary.json")
    } else {
        run_dir.to_path_buf()
    };
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(std::io::Error::other)
}

/// Compare two run summaries and render a Markdown diff of pass/fail, score,
/// tool counts, and runtime.
pub fn compare(baseline: &RunSummary, candidate: &RunSummary) -> String {
    use std::collections::BTreeMap;
    let base: BTreeMap<&str, &TaskSummary> = baseline
        .tasks
        .iter()
        .map(|t| (t.task_id.as_str(), t))
        .collect();
    let cand: BTreeMap<&str, &TaskSummary> = candidate
        .tasks
        .iter()
        .map(|t| (t.task_id.as_str(), t))
        .collect();

    let mut all_ids: Vec<&str> = base.keys().chain(cand.keys()).copied().collect();
    all_ids.sort();
    all_ids.dedup();

    let mut s = String::new();
    s.push_str(&format!(
        "# Compare `{}` → `{}`\n\n",
        baseline.run_id, candidate.run_id
    ));
    s.push_str(&format!(
        "- baseline mean score: {:.3} ({} passed)\n",
        baseline.mean_score, baseline.passed
    ));
    s.push_str(&format!(
        "- candidate mean score: {:.3} ({} passed)\n\n",
        candidate.mean_score, candidate.passed
    ));
    s.push_str("| task | baseline | candidate | Δscore | Δtools | Δms |\n");
    s.push_str("|------|----------|-----------|--------|--------|-----|\n");
    for id in all_ids {
        let b = base.get(id);
        let c = cand.get(id);
        let b_status = b
            .map(|t| t.status.symbol().trim().to_string())
            .unwrap_or_else(|| "—".into());
        let c_status = c
            .map(|t| t.status.symbol().trim().to_string())
            .unwrap_or_else(|| "—".into());
        let dscore = c.map(|t| t.score).unwrap_or(0.0) - b.map(|t| t.score).unwrap_or(0.0);
        let dtools = c.map(|t| t.tool_calls as i64).unwrap_or(0)
            - b.map(|t| t.tool_calls as i64).unwrap_or(0);
        let dms = c.map(|t| t.duration_ms as i64).unwrap_or(0)
            - b.map(|t| t.duration_ms as i64).unwrap_or(0);
        let flag = if b.map(|t| t.status) != c.map(|t| t.status) {
            " ⚠"
        } else {
            ""
        };
        s.push_str(&format!(
            "| {id}{flag} | {b_status} | {c_status} | {dscore:+.3} | {dtools:+} | {dms:+} |\n"
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::EvalScore;

    fn result(id: &str, status: EvalStatus, score: f64) -> EvalResult {
        let resolved = status == EvalStatus::Passed;
        let mut r = crate::runner::test_support::result_with(id, resolved, None);
        r.status = status;
        r.score = EvalScore {
            total: score,
            correctness: score,
            patch_quality: score,
            process_quality: score,
            efficiency: score,
            notes: vec![],
        };
        r.suites = vec![];
        r.category = None;
        r.tags = vec![];
        r.checks_passed = if resolved { 1 } else { 0 };
        r.checks_total = 1;
        r.tool_calls = 2;
        r
    }

    #[test]
    fn summary_counts_and_markdown() {
        let results = vec![
            result("a", EvalStatus::Passed, 1.0),
            result("b", EvalStatus::Failed, 0.0),
        ];
        let summary = RunSummary::from_results("run-1", "fake", &results);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert!((summary.mean_score - 0.5).abs() < 1e-9);
        let md = summary.to_markdown();
        assert!(md.contains("run-1"));
        assert!(md.contains("| a |"));
    }

    #[test]
    fn compare_flags_regression() {
        let base =
            RunSummary::from_results("base", "fake", &[result("a", EvalStatus::Passed, 1.0)]);
        let cand =
            RunSummary::from_results("cand", "fake", &[result("a", EvalStatus::Failed, 0.0)]);
        let diff = compare(&base, &cand);
        assert!(diff.contains("⚠"));
        assert!(diff.contains("-1.000"));
    }
}
