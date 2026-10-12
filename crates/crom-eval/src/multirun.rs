//! Multi-run statistics (M15F).
//!
//! Repeated runs of the same task set (`--repetitions k`) are aggregated into
//! pass@1 / pass@k, per-task pass rates, variance flags, and a failure-class
//! distribution. The aggregation is deterministic given its input results.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::index::median;
use crate::runner::EvalResult;

/// Per-task aggregation across repetitions.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRepetitionStats {
    pub task_id: String,
    pub runs: usize,
    pub resolved_count: usize,
    pub pass_rate: f64,
    /// True when some but not all repetitions resolved.
    pub flaky_or_nondeterministic: bool,
    pub failure_classes: BTreeMap<String, usize>,
}

/// Aggregate statistics over a multi-run set.
#[derive(Debug, Clone, Serialize)]
pub struct MultiRunStats {
    pub repetitions: usize,
    pub total_tasks: usize,
    /// Mean single-sample resolved rate (probability one attempt resolves).
    pub pass_at_1: f64,
    /// Probability at least one of k attempts resolves, averaged over tasks.
    pub pass_at_k: f64,
    pub mean_resolved_rate: f64,
    pub median_resolved_rate: f64,
    pub failure_class_distribution: BTreeMap<String, usize>,
    pub median_cost_usd: Option<f64>,
    pub median_wall_ms: u64,
    pub median_tool_calls: u64,
    pub per_task: Vec<TaskRepetitionStats>,
}

/// Aggregate a flat list of results (across all repetitions) by task.
pub fn aggregate(results: &[EvalResult], repetitions: usize) -> MultiRunStats {
    let mut by_task: BTreeMap<&str, Vec<&EvalResult>> = BTreeMap::new();
    for r in results {
        by_task.entry(&r.task_id).or_default().push(r);
    }

    let mut per_task = Vec::new();
    let mut pass_rates = Vec::new();
    let mut pass_at_k_hits = 0usize;
    let mut global_failures: BTreeMap<String, usize> = BTreeMap::new();

    for (task_id, runs) in &by_task {
        let n = runs.len();
        let resolved_count = runs.iter().filter(|r| r.resolved).count();
        let pass_rate = if n > 0 {
            resolved_count as f64 / n as f64
        } else {
            0.0
        };
        pass_rates.push(pass_rate);
        if resolved_count >= 1 {
            pass_at_k_hits += 1;
        }
        let mut failure_classes: BTreeMap<String, usize> = BTreeMap::new();
        for r in runs {
            if let Some(fc) = r.failure_class {
                *failure_classes.entry(fc.key().to_string()).or_insert(0) += 1;
                *global_failures.entry(fc.key().to_string()).or_insert(0) += 1;
            }
        }
        per_task.push(TaskRepetitionStats {
            task_id: task_id.to_string(),
            runs: n,
            resolved_count,
            pass_rate,
            flaky_or_nondeterministic: resolved_count > 0 && resolved_count < n,
            failure_classes,
        });
    }

    let total_tasks = by_task.len();
    let mean = if pass_rates.is_empty() {
        0.0
    } else {
        pass_rates.iter().sum::<f64>() / pass_rates.len() as f64
    };
    let pass_at_k = if total_tasks > 0 {
        pass_at_k_hits as f64 / total_tasks as f64
    } else {
        0.0
    };

    let walls: Vec<u64> = results.iter().map(|r| r.duration_ms).collect();
    let tools: Vec<u64> = results.iter().map(|r| r.tool_calls as u64).collect();
    let costs: Vec<f64> = results.iter().filter_map(|r| r.cost_usd).collect();

    MultiRunStats {
        repetitions,
        total_tasks,
        pass_at_1: round3(mean),
        pass_at_k: round3(pass_at_k),
        mean_resolved_rate: round3(mean),
        median_resolved_rate: round3(median_f64(&pass_rates)),
        failure_class_distribution: global_failures,
        median_cost_usd: if costs.is_empty() {
            None
        } else {
            Some(median_f64(&costs))
        },
        median_wall_ms: median(&walls),
        median_tool_calls: median(&tools),
        per_task,
    }
}

fn median_f64(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Render a short Markdown summary of multi-run stats.
pub fn to_markdown(stats: &MultiRunStats) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# Multi-run stats ({} repetitions, {} tasks)\n\n",
        stats.repetitions, stats.total_tasks
    ));
    s.push_str(&format!("- pass@1: {:.3}\n", stats.pass_at_1));
    s.push_str(&format!(
        "- pass@{}: {:.3}\n",
        stats.repetitions, stats.pass_at_k
    ));
    s.push_str(&format!(
        "- mean resolved rate: {:.3}\n",
        stats.mean_resolved_rate
    ));
    s.push_str(&format!(
        "- median resolved rate: {:.3}\n",
        stats.median_resolved_rate
    ));
    s.push_str(&format!(
        "- median tool calls: {}\n",
        stats.median_tool_calls
    ));
    s.push_str(&format!("- median wall ms: {}\n\n", stats.median_wall_ms));
    if !stats.failure_class_distribution.is_empty() {
        s.push_str("Failure classes:\n\n");
        for (k, v) in &stats.failure_class_distribution {
            s.push_str(&format!("- {k}: {v}\n"));
        }
        s.push('\n');
    }
    s.push_str("| task | runs | resolved | pass rate | flaky |\n");
    s.push_str("|------|------|----------|-----------|-------|\n");
    for t in &stats.per_task {
        s.push_str(&format!(
            "| {} | {} | {} | {:.3} | {} |\n",
            t.task_id,
            t.runs,
            t.resolved_count,
            t.pass_rate,
            if t.flaky_or_nondeterministic {
                "yes"
            } else {
                ""
            }
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::FailureClass;
    use crate::runner::test_support::result_with;

    #[test]
    fn aggregates_pass_rate_and_flaky() {
        let results = vec![
            result_with("t1", true, None),
            result_with("t1", true, None),
            result_with("t1", false, Some(FailureClass::CompileError)),
            result_with("t2", false, Some(FailureClass::HiddenTestFailure)),
            result_with("t2", false, Some(FailureClass::HiddenTestFailure)),
            result_with("t2", false, Some(FailureClass::HiddenTestFailure)),
        ];
        let stats = aggregate(&results, 3);
        assert_eq!(stats.total_tasks, 2);
        let t1 = stats.per_task.iter().find(|t| t.task_id == "t1").unwrap();
        assert_eq!(t1.resolved_count, 2);
        assert!((t1.pass_rate - 2.0 / 3.0).abs() < 1e-6);
        assert!(t1.flaky_or_nondeterministic);
        // pass@1 = mean of (2/3, 0) = 1/3
        assert!((stats.pass_at_1 - 0.333).abs() < 1e-3);
        // pass@3 = at least one resolved per task = (t1 yes, t2 no) = 0.5
        assert!((stats.pass_at_k - 0.5).abs() < 1e-6);
        assert_eq!(
            stats.failure_class_distribution.get("hidden_test_failure"),
            Some(&3)
        );
    }
}
