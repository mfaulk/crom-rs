//! Failure triage workflow (M15H).
//!
//! `eval triage <run>` groups failed tasks by failure class, highest-value
//! first. `eval show <run> <task>` prints a compact, debuggable view of a single
//! task. `eval open <run> <task>` lists the artifact paths for inspection.
//!
//! All of these read the per-task `result.json` the runner writes, plus the
//! verification logs — they never re-run the model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::runner::EvalResult;

/// All `tasks/` directories within a run. For a single run this is just
/// `<run>/tasks`; for a repeated run (`--repetitions k`) the per-repetition
/// artifacts live under `<run>/rep-<n>/tasks`, so those are included too.
fn tasks_dirs(run_dir: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let direct = run_dir.join("tasks");
    if direct.exists() {
        dirs.push(direct);
    }
    if let Ok(read) = std::fs::read_dir(run_dir) {
        for entry in read.flatten() {
            let p = entry.path();
            if p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("rep-"))
            {
                let t = p.join("tasks");
                if t.exists() {
                    dirs.push(t);
                }
            }
        }
    }
    dirs.sort();
    dirs
}

/// Path to a task's artifact directory within a run. Resolves repeated-run
/// layouts (`rep-<n>/tasks/<id>`) by returning the first matching directory,
/// falling back to the canonical `<run>/tasks/<id>` path.
pub fn task_dir(run_dir: &Path, task_id: &str) -> PathBuf {
    for tasks in tasks_dirs(run_dir) {
        let candidate = tasks.join(task_id);
        if candidate.exists() {
            return candidate;
        }
    }
    run_dir.join("tasks").join(task_id)
}

/// Load every per-task `result.json` in a run directory (including all
/// repetitions for a repeated run).
pub fn load_results(run_dir: &Path) -> Result<Vec<EvalResult>> {
    let dirs = tasks_dirs(run_dir);
    if dirs.is_empty() {
        return Err(anyhow!("no tasks/ under {}", run_dir.display()));
    }
    let mut out = Vec::new();
    for tasks_dir in dirs {
        for entry in std::fs::read_dir(&tasks_dir)
            .with_context(|| format!("reading {}", tasks_dir.display()))?
        {
            let p = entry?.path();
            let result_json = p.join("result.json");
            if result_json.exists() {
                if let Ok(text) = std::fs::read_to_string(&result_json) {
                    if let Ok(r) = serde_json::from_str::<EvalResult>(&text) {
                        out.push(r);
                    }
                }
            }
        }
    }
    out.sort_by_key(|r| (r.task_id.clone(), r.repetition));
    Ok(out)
}

/// The list of artifact files for a task (existing ones only).
pub fn artifact_paths(run_dir: &Path, task_id: &str) -> Vec<PathBuf> {
    let dir = task_dir(run_dir, task_id);
    let names = [
        "task.toml",
        "normalized-task.json",
        "prompt.md",
        "result.json",
        "behavior.json",
        "verification.json",
        "events.jsonl",
        "transcript.jsonl",
        "tool-calls.jsonl",
        "final.patch",
        "patch.diff",
        "public-test.log",
        "hidden-test.log",
        "report.md",
        "triage.md",
        "environment.json",
        "isolation.json",
        "adapter.json",
    ];
    let mut out = Vec::new();
    if dir.join("workspace").exists() {
        out.push(dir.join("workspace"));
    }
    for n in names {
        let p = dir.join(n);
        if p.exists() {
            out.push(p);
        }
    }
    out
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

fn read_tail(path: &Path, lines: usize) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|t| tail(&t, lines))
}

/// Last `n` tool calls from `tool-calls.jsonl`, as `name(input-keys)` lines.
fn last_tool_calls(run_dir: &Path, task_id: &str, n: usize) -> Vec<String> {
    let path = task_dir(run_dir, task_id).join("tool-calls.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut calls: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            let name = v
                .get("tool_name")
                .and_then(|x| x.as_str())
                .unwrap_or("?")
                .to_string();
            let detail = v
                .get("input")
                .and_then(|i| {
                    i.get("command")
                        .or_else(|| i.get("file_path"))
                        .and_then(|x| x.as_str())
                })
                .unwrap_or("")
                .to_string();
            calls.push(if detail.is_empty() {
                name
            } else {
                format!("{name}: {detail}")
            });
        }
    }
    let start = calls.len().saturating_sub(n);
    calls[start..].to_vec()
}

/// Render a compact, debuggable view of a single task in a run.
pub fn show(run_dir: &Path, task_id: &str) -> Result<String> {
    let dir = task_dir(run_dir, task_id);
    if !dir.exists() {
        return Err(anyhow!(
            "no artifacts for task '{task_id}' in {}",
            run_dir.display()
        ));
    }
    let result: Option<EvalResult> = std::fs::read_to_string(dir.join("result.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok());

    let mut s = String::new();
    s.push_str(&format!("# {task_id}\n\n"));

    if let Some(r) = &result {
        s.push_str(&format!("- title: {}\n", r.title));
        s.push_str(&format!("- status: {:?}\n", r.status));
        s.push_str(&format!("- resolved: {}\n", r.resolved));
        if let Some(fc) = r.failure_class {
            s.push_str(&format!("- failure class: {}\n", fc.key()));
        }
        if !r.secondary_failures.is_empty() {
            let labels: Vec<&str> = r.secondary_failures.iter().map(|c| c.key()).collect();
            s.push_str(&format!("- secondary: {}\n", labels.join(", ")));
        }
        if let Some(cat) = &r.category {
            s.push_str(&format!("- category: {cat}\n"));
        }
        if !r.tags.is_empty() {
            s.push_str(&format!("- tags: {}\n", r.tags.join(", ")));
        }
        if !r.capabilities.is_empty() {
            s.push_str(&format!("- capabilities: {}\n", r.capabilities.join(", ")));
        }
        s.push_str(&format!(
            "- adapter/isolation/provider: {}/{}/{}\n",
            r.adapter, r.isolation, r.provider
        ));
        s.push_str(&format!(
            "- patch: +{} -{} ({} files context)\n",
            r.patch_lines_added, r.patch_lines_removed, r.checks_total
        ));
        if !r.behavior_warnings.is_empty() {
            s.push_str("\n## Behavior warnings\n\n");
            for w in &r.behavior_warnings {
                s.push_str(&format!("- ⚠ {w}\n"));
            }
        }
    }

    // M16: repair trace, when present.
    if let Ok(md) = std::fs::read_to_string(dir.join("repair.md")) {
        s.push_str("\n## Repair trace\n\n");
        s.push_str(md.trim());
        s.push('\n');
    } else if let Some(r) = &result {
        let m = &r.repair;
        if m.repair_attempt_count > 0 || m.diagnostic_primary_kind.is_some() {
            s.push_str("\n## Repair\n\n");
            s.push_str(&format!("- attempts: {}\n", m.repair_attempt_count));
            if let Some(k) = m.diagnostic_primary_kind {
                s.push_str(&format!("- initial failure: {}\n", k.as_str()));
            }
            s.push_str(&format!("- repeated: {}\n", m.same_error_repeated));
            s.push_str(&format!(
                "- final validation: {:?}\n",
                m.final_validation_status
            ));
        }
    }

    // Prompt.
    if let Ok(p) = std::fs::read_to_string(dir.join("prompt.md")) {
        s.push_str("\n## Prompt\n\n");
        s.push_str(&tail(&p, 40));
        s.push('\n');
    }

    // Last tool calls.
    let calls = last_tool_calls(run_dir, task_id, 12);
    if !calls.is_empty() {
        s.push_str("\n## Last tool calls\n\n");
        for c in calls {
            s.push_str(&format!("- {c}\n"));
        }
    }

    // Verification tails.
    if let Some(t) = read_tail(&dir.join("public-test.log"), 30) {
        if !t.trim().is_empty() {
            s.push_str("\n## Public verification (tail)\n\n```\n");
            s.push_str(&t);
            s.push_str("\n```\n");
        }
    }
    if let Some(t) = read_tail(&dir.join("hidden-test.log"), 30) {
        if !t.trim().is_empty() {
            s.push_str("\n## Hidden verification (tail)\n\n```\n");
            s.push_str(&t);
            s.push_str("\n```\n");
        }
    }

    // Final answer.
    if let Ok(fs) = std::fs::read_to_string(dir.join("final_summary.md")) {
        s.push_str("\n## Final summary\n\n");
        s.push_str(&tail(&fs, 30));
        s.push('\n');
    }

    s.push_str("\n## Artifacts\n\n");
    for p in artifact_paths(run_dir, task_id) {
        s.push_str(&format!("- {}\n", p.display()));
    }
    Ok(s)
}

/// Group failed tasks by failure class, highest-value first.
pub fn triage(run_dir: &Path) -> Result<String> {
    let results = load_results(run_dir)?;
    let failures: Vec<&EvalResult> = results.iter().filter(|r| !r.resolved).collect();

    let mut by_class: BTreeMap<String, Vec<&EvalResult>> = BTreeMap::new();
    for r in &failures {
        let key = r
            .failure_class
            .map(|c| c.key().to_string())
            .unwrap_or_else(|| "unclassified".to_string());
        by_class.entry(key).or_default().push(r);
    }

    // Priority ordering: most actionable / most concerning first.
    let priority = [
        "harness_error",
        "isolation_error",
        "adapter_error",
        "task_invalid",
        "oracle_validation_failed",
        "forbidden_path_changed",
        "unsafe_command",
        "overfit_public_tests",
        "hidden_test_failure",
        // M16 repair-loop classes: grouped near the relevant build/test failures.
        "repeated_same_failure",
        "repair_attempt_limit_exceeded",
        "unrecovered_compile_error",
        "unrecovered_test_failure",
        "unvalidated_completion",
        "diagnostic_parse_failure",
        "compile_error",
        "public_test_failure",
        "required_path_missing",
        "planning_failure",
        "timeout",
        "tool_loop",
        "did_not_attempt",
        "provider_error",
    ];
    let mut ordered: Vec<String> = priority
        .iter()
        .filter(|k| by_class.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    for k in by_class.keys() {
        if !ordered.contains(k) {
            ordered.push(k.clone());
        }
    }

    let mut s = String::new();
    s.push_str(&format!("# Triage `{}`\n\n", run_dir.display()));
    s.push_str(&format!(
        "{} task(s), {} failed.\n\n",
        results.len(),
        failures.len()
    ));
    if failures.is_empty() {
        s.push_str("No failures. 🎉\n");
        return Ok(s);
    }
    for class in ordered {
        let group = &by_class[&class];
        s.push_str(&format!("## {class} ({})\n\n", group.len()));
        for r in group {
            let mut line = format!("- {} — {}", r.task_id, r.title);
            if !r.behavior_warnings.is_empty() {
                line.push_str(&format!(" [⚠ {}]", r.behavior_warnings.len()));
            }
            s.push_str(&line);
            s.push('\n');
            s.push_str(&format!(
                "    show: `crom eval show {} {}`\n",
                run_id_hint(run_dir),
                r.task_id
            ));
        }
        s.push('\n');
    }
    Ok(s)
}

fn run_id_hint(run_dir: &Path) -> String {
    run_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| run_dir.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::failure::FailureClass;
    use crate::runner::test_support::result_with;

    fn write_result(run_dir: &Path, r: &EvalResult) {
        let dir = task_dir(run_dir, &r.task_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("result.json"),
            serde_json::to_string_pretty(r).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("prompt.md"), "do the thing").unwrap();
    }

    #[test]
    fn triage_groups_by_failure_class() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run-1");
        write_result(
            &run,
            &result_with("t1", false, Some(FailureClass::CompileError)),
        );
        write_result(
            &run,
            &result_with("t2", false, Some(FailureClass::HiddenTestFailure)),
        );
        write_result(&run, &result_with("t3", true, None));
        let report = triage(&run).unwrap();
        assert!(report.contains("compile_error (1)"));
        assert!(report.contains("hidden_test_failure (1)"));
        assert!(report.contains("3 task(s), 2 failed"));
    }

    #[test]
    fn show_includes_details_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run-1");
        write_result(
            &run,
            &result_with("t1", false, Some(FailureClass::CompileError)),
        );
        let out = show(&run, "t1").unwrap();
        assert!(out.contains("failure class: compile_error"));
        assert!(out.contains("## Artifacts"));
        assert!(out.contains("result.json"));
    }
}
