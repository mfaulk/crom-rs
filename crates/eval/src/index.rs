//! Result index, history, and trend/regression reporting (M15G).
//!
//! Every task result is appended as a JSON line to a lightweight index at
//! `target/code-agent-eval/index.jsonl`. The index is the substrate for
//! `eval history`, `eval trend`, `eval task-history`, and `eval regressions`.
//! JSONL is used rather than SQLite to keep the dependency surface small while
//! still supporting append-only writes and cheap scans.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::failure::FailureClass;
use crate::runner::EvalResult;

/// Default index path under `target/`.
pub const INDEX_PATH: &str = "target/code-agent-eval/index.jsonl";

/// One row in the result index. Stable, append-only schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub run_id: String,
    /// RFC3339 timestamp (filled by the caller).
    pub timestamp: String,
    pub git_sha: Option<String>,
    pub branch: Option<String>,
    pub dirty_worktree: bool,
    pub suite: Option<String>,
    pub task_id: String,
    pub adapter: String,
    pub isolation: String,
    pub provider: String,
    pub model: String,
    pub resolved: bool,
    pub failure_class: Option<FailureClass>,
    pub cost_usd: Option<f64>,
    pub wall_time_ms: u64,
    pub tool_call_count: u64,
    pub patch_lines_added: u64,
    pub patch_lines_removed: u64,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub capabilities: Vec<String>,
    pub repetition: usize,
    pub quarantined: bool,
}

impl IndexEntry {
    /// Build an index entry from a result plus run-level git/timestamp context.
    pub fn from_result(
        result: &EvalResult,
        run_id: &str,
        timestamp: &str,
        git: &GitContext,
    ) -> Self {
        IndexEntry {
            run_id: run_id.to_string(),
            timestamp: timestamp.to_string(),
            git_sha: git.sha.clone(),
            branch: git.branch.clone(),
            dirty_worktree: git.dirty,
            suite: result.suites.first().cloned(),
            task_id: result.task_id.clone(),
            adapter: result.adapter.clone(),
            isolation: result.isolation.clone(),
            provider: result.provider.clone(),
            model: result.model.clone(),
            resolved: result.resolved,
            failure_class: result.failure_class,
            cost_usd: result.cost_usd,
            wall_time_ms: result.duration_ms,
            tool_call_count: result.tool_calls as u64,
            patch_lines_added: result.patch_lines_added as u64,
            patch_lines_removed: result.patch_lines_removed as u64,
            category: result.category.clone(),
            tags: result.tags.clone(),
            capabilities: result.capabilities.clone(),
            repetition: result.repetition,
            quarantined: result.quarantined,
        }
    }
}

/// Git context captured once per run.
#[derive(Debug, Clone, Default)]
pub struct GitContext {
    pub sha: Option<String>,
    pub branch: Option<String>,
    pub dirty: bool,
}

impl GitContext {
    /// Best-effort capture from the current working directory.
    pub fn detect() -> Self {
        let sha = git(&["rev-parse", "HEAD"]).map(|s| s.trim().to_string());
        let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).map(|s| s.trim().to_string());
        let dirty = git(&["status", "--porcelain"])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        GitContext { sha, branch, dirty }
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Append entries to the index file, creating parent dirs as needed.
pub fn append_entries(index_path: &Path, entries: &[IndexEntry]) -> std::io::Result<()> {
    if let Some(parent) = index_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(index_path)?;
    for e in entries {
        let line = serde_json::to_string(e).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Load all entries from an index file (missing file => empty).
pub fn load_entries(index_path: &Path) -> std::io::Result<Vec<IndexEntry>> {
    if !index_path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(index_path)?;
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<IndexEntry>(line) {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Resolve the default index path.
pub fn default_index_path() -> PathBuf {
    PathBuf::from(INDEX_PATH)
}

/// A per-run rollup used by `history`.
#[derive(Debug, Clone, Serialize)]
pub struct RunRollup {
    pub run_id: String,
    pub timestamp: String,
    pub git_sha: Option<String>,
    pub branch: Option<String>,
    pub suite: Option<String>,
    pub total: usize,
    pub resolved: usize,
    pub median_tool_calls: u64,
    pub median_wall_ms: u64,
}

/// Summarise the index into per-run rollups, most recent last (input order).
pub fn history(entries: &[IndexEntry]) -> Vec<RunRollup> {
    // Preserve first-seen run order.
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<&IndexEntry>> = BTreeMap::new();
    for e in entries {
        if !groups.contains_key(&e.run_id) {
            order.push(e.run_id.clone());
        }
        groups.entry(e.run_id.clone()).or_default().push(e);
    }
    order
        .into_iter()
        .map(|run_id| {
            let g = &groups[&run_id];
            let resolved = g.iter().filter(|e| e.resolved).count();
            let tool_calls: Vec<u64> = g.iter().map(|e| e.tool_call_count).collect();
            let walls: Vec<u64> = g.iter().map(|e| e.wall_time_ms).collect();
            RunRollup {
                run_id: run_id.clone(),
                timestamp: g[0].timestamp.clone(),
                git_sha: g[0].git_sha.clone(),
                branch: g[0].branch.clone(),
                suite: g[0].suite.clone(),
                total: g.len(),
                resolved,
                median_tool_calls: median(&tool_calls),
                median_wall_ms: median(&walls),
            }
        })
        .collect()
}

/// Per-task history across all runs.
pub fn task_history<'a>(entries: &'a [IndexEntry], task_id: &str) -> Vec<&'a IndexEntry> {
    entries.iter().filter(|e| e.task_id == task_id).collect()
}

/// A task's aggregate trend within a suite.
#[derive(Debug, Clone, Serialize)]
pub struct TaskTrend {
    pub task_id: String,
    pub runs: usize,
    pub resolved: usize,
    pub pass_rate: f64,
    pub chronic_failure: bool,
    pub high_variance: bool,
}

/// Trend for a suite: per-task pass rates and flags. `suite=None` => all tasks.
pub fn trend(entries: &[IndexEntry], suite: Option<&str>) -> Vec<TaskTrend> {
    let mut by_task: BTreeMap<&str, Vec<&IndexEntry>> = BTreeMap::new();
    for e in entries {
        if let Some(s) = suite {
            if e.suite.as_deref() != Some(s) {
                continue;
            }
        }
        by_task.entry(&e.task_id).or_default().push(e);
    }
    by_task
        .into_iter()
        .map(|(task_id, g)| {
            let runs = g.len();
            let resolved = g.iter().filter(|e| e.resolved).count();
            let pass_rate = if runs > 0 {
                resolved as f64 / runs as f64
            } else {
                0.0
            };
            TaskTrend {
                task_id: task_id.to_string(),
                runs,
                resolved,
                pass_rate,
                chronic_failure: runs >= 2 && resolved == 0,
                high_variance: runs >= 2 && resolved > 0 && resolved < runs,
            }
        })
        .collect()
}

/// Regression analysis between a baseline and candidate run.
#[derive(Debug, Clone, Serialize)]
pub struct RegressionReport {
    pub baseline_run: String,
    pub candidate_run: String,
    pub newly_fixed: Vec<String>,
    pub new_regressions: Vec<String>,
    pub unchanged_passing: Vec<String>,
    pub unchanged_failing: Vec<String>,
    pub baseline_resolved: usize,
    pub candidate_resolved: usize,
    pub baseline_median_tool_calls: u64,
    pub candidate_median_tool_calls: u64,
    pub failure_class_shift: BTreeMap<String, i64>,
}

/// Select entries for a run by run_id (or git_sha prefix as a fallback).
fn entries_for_ref<'a>(entries: &'a [IndexEntry], reference: &str) -> Vec<&'a IndexEntry> {
    let by_run: Vec<&IndexEntry> = entries.iter().filter(|e| e.run_id == reference).collect();
    if !by_run.is_empty() {
        return by_run;
    }
    entries
        .iter()
        .filter(|e| {
            e.git_sha
                .as_deref()
                .is_some_and(|s| s.starts_with(reference))
        })
        .collect()
}

/// Compute regressions between two run references.
pub fn regressions(
    entries: &[IndexEntry],
    baseline_ref: &str,
    candidate_ref: &str,
) -> RegressionReport {
    let base = entries_for_ref(entries, baseline_ref);
    let cand = entries_for_ref(entries, candidate_ref);

    let base_map: BTreeMap<&str, bool> = base
        .iter()
        .map(|e| (e.task_id.as_str(), e.resolved))
        .collect();
    let cand_map: BTreeMap<&str, bool> = cand
        .iter()
        .map(|e| (e.task_id.as_str(), e.resolved))
        .collect();

    let mut newly_fixed = Vec::new();
    let mut new_regressions = Vec::new();
    let mut unchanged_passing = Vec::new();
    let mut unchanged_failing = Vec::new();

    let mut all: Vec<&str> = base_map.keys().chain(cand_map.keys()).copied().collect();
    all.sort();
    all.dedup();
    for id in all {
        let b = base_map.get(id).copied();
        let c = cand_map.get(id).copied();
        match (b, c) {
            (Some(false), Some(true)) | (None, Some(true)) => newly_fixed.push(id.to_string()),
            (Some(true), Some(false)) | (Some(true), None) => new_regressions.push(id.to_string()),
            (Some(true), Some(true)) => unchanged_passing.push(id.to_string()),
            (Some(false), Some(false)) => unchanged_failing.push(id.to_string()),
            _ => {}
        }
    }

    let mut failure_class_shift: BTreeMap<String, i64> = BTreeMap::new();
    for e in &base {
        if let Some(fc) = e.failure_class {
            *failure_class_shift.entry(fc.key().to_string()).or_insert(0) -= 1;
        }
    }
    for e in &cand {
        if let Some(fc) = e.failure_class {
            *failure_class_shift.entry(fc.key().to_string()).or_insert(0) += 1;
        }
    }
    failure_class_shift.retain(|_, v| *v != 0);

    let base_tools: Vec<u64> = base.iter().map(|e| e.tool_call_count).collect();
    let cand_tools: Vec<u64> = cand.iter().map(|e| e.tool_call_count).collect();

    RegressionReport {
        baseline_run: baseline_ref.to_string(),
        candidate_run: candidate_ref.to_string(),
        newly_fixed,
        new_regressions,
        unchanged_passing,
        unchanged_failing,
        baseline_resolved: base.iter().filter(|e| e.resolved).count(),
        candidate_resolved: cand.iter().filter(|e| e.resolved).count(),
        baseline_median_tool_calls: median(&base_tools),
        candidate_median_tool_calls: median(&cand_tools),
        failure_class_shift,
    }
}

/// Median of a slice (0 when empty).
pub fn median(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut v = values.to_vec();
    v.sort_unstable();
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2
    } else {
        v[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(run: &str, task: &str, resolved: bool, fc: Option<FailureClass>) -> IndexEntry {
        IndexEntry {
            run_id: run.into(),
            timestamp: "2026-06-03T00:00:00Z".into(),
            git_sha: Some("abc123".into()),
            branch: Some("main".into()),
            dirty_worktree: false,
            suite: Some("rust-mini".into()),
            task_id: task.into(),
            adapter: "internal".into(),
            isolation: "local".into(),
            provider: "fake".into(),
            model: "fake".into(),
            resolved,
            failure_class: fc,
            cost_usd: None,
            wall_time_ms: 100,
            tool_call_count: 5,
            patch_lines_added: 1,
            patch_lines_removed: 0,
            category: Some("bugfix".into()),
            tags: vec![],
            capabilities: vec![],
            repetition: 0,
            quarantined: false,
        }
    }

    #[test]
    fn roundtrip_append_and_load() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("index.jsonl");
        append_entries(&path, &[entry("r1", "t1", true, None)]).unwrap();
        append_entries(
            &path,
            &[entry("r2", "t1", false, Some(FailureClass::CompileError))],
        )
        .unwrap();
        let loaded = load_entries(&path).unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[test]
    fn history_groups_by_run() {
        let entries = vec![
            entry("r1", "t1", true, None),
            entry("r1", "t2", false, Some(FailureClass::CompileError)),
        ];
        let h = history(&entries);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].total, 2);
        assert_eq!(h[0].resolved, 1);
    }

    #[test]
    fn regressions_finds_fixes_and_breaks() {
        let entries = vec![
            entry("base", "t1", true, None),
            entry("base", "t2", false, Some(FailureClass::CompileError)),
            entry("base", "t3", true, None),
            entry("cand", "t1", false, Some(FailureClass::HiddenTestFailure)),
            entry("cand", "t2", true, None),
            entry("cand", "t3", true, None),
        ];
        let r = regressions(&entries, "base", "cand");
        assert_eq!(r.newly_fixed, vec!["t2"]);
        assert_eq!(r.new_regressions, vec!["t1"]);
        assert_eq!(r.unchanged_passing, vec!["t3"]);
    }

    #[test]
    fn trend_flags_chronic_and_variance() {
        let entries = vec![
            entry("r1", "chronic", false, Some(FailureClass::CompileError)),
            entry("r2", "chronic", false, Some(FailureClass::CompileError)),
            entry("r1", "flaky", true, None),
            entry("r2", "flaky", false, Some(FailureClass::HiddenTestFailure)),
        ];
        let t = trend(&entries, Some("rust-mini"));
        let chronic = t.iter().find(|x| x.task_id == "chronic").unwrap();
        assert!(chronic.chronic_failure);
        let flaky = t.iter().find(|x| x.task_id == "flaky").unwrap();
        assert!(flaky.high_variance);
    }

    #[test]
    fn median_works() {
        assert_eq!(median(&[]), 0);
        assert_eq!(median(&[5]), 5);
        assert_eq!(median(&[1, 3]), 2);
        assert_eq!(median(&[1, 2, 3]), 2);
    }
}
