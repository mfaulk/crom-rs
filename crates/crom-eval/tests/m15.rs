//! M15 end-to-end tests: hidden verification, adapters, repetitions, and the
//! result index. All run offline under the fake provider with shell-command
//! oracles (no cargo/network) so they are fast and deterministic in CI.

use std::path::Path;

use crom_eval::adapter::{AdapterRegistry, DirectoryAdapter};
use crom_eval::artifacts::RunDir;
use crom_eval::external::SweBenchAdapter;
use crom_eval::index::{self, GitContext, IndexEntry};
use crom_eval::multirun;
use crom_eval::runner::{run_task, run_tasks_repeated, RunConfig};
use crom_eval::task::load_task;

fn write(path: &Path, content: &str) {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn hidden_verification_runs_after_completion() {
    let tmp = tempfile::tempdir().unwrap();
    let task_dir = tmp.path().join("hidden-task");
    write(&task_dir.join("repo/README.md"), "hello world\n");
    // Hidden overlay carries a marker file, but the hidden check requires a
    // file the agent would have to create — so it fails on the untouched repo.
    write(&task_dir.join("hidden/.keep"), "");
    write(
        &task_dir.join("task.toml"),
        r#"
schema_version = 2
id = "hidden-task"
title = "hidden check demo"
category = "bugfix"
prompt = "Create solution.txt"
suites = ["rust-mini"]

[fixture]
kind = "copy_dir"
path = "repo"

[oracle]
kind = "none"

[[oracle.assertions]]
kind = "file_contains"
path = "README.md"
text = "hello"

[hidden]
fixture = "hidden"

[hidden.oracle]
kind = "command"
commands = ["test -f solution.txt"]

[capabilities]
requires_test_feedback = true
"#,
    );

    let task = load_task(&task_dir.join("task.toml")).unwrap();
    let run = RunDir::create(tmp.path(), "run-hidden").unwrap();
    let result = run_task(&task, &run, &RunConfig::default()).await;

    assert!(result.public_passed, "public assertion should pass");
    assert_eq!(
        result.hidden_passed,
        Some(false),
        "hidden should fail on base"
    );
    assert!(!result.resolved, "not resolved when hidden fails");
    assert!(result.failure_class.is_some());
    // The hidden verification log was written.
    let log = run.root.join("tasks/hidden-task/hidden-test.log");
    assert!(
        log.exists(),
        "hidden-test.log should exist at {}",
        log.display()
    );
    let verification = run.root.join("tasks/hidden-task/verification.json");
    assert!(verification.exists());
}

#[tokio::test]
async fn directory_adapter_runs_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let task_dir = tmp.path().join("adhoc");
    write(&task_dir.join("repo/README.md"), "marker present\n");
    write(
        &task_dir.join("task.toml"),
        r#"
schema_version = 2
id = "adhoc-noop"
title = "ad hoc noop"
prompt = "no changes needed"
[fixture]
kind = "copy_dir"
path = "repo"
[oracle]
kind = "none"
[[oracle.assertions]]
kind = "file_contains"
path = "README.md"
text = "marker present"
"#,
    );

    let task = DirectoryAdapter::load_one(&task_dir).unwrap();
    let config = RunConfig {
        adapter: "directory".to_string(),
        ..RunConfig::default()
    };
    let run = RunDir::create(tmp.path(), "run-dir").unwrap();
    let result = run_task(&task, &run, &config).await;
    assert!(result.resolved, "noop assertion task should resolve");
    assert_eq!(result.adapter, "directory");
    // result.json artifact written for triage.
    assert!(run.root.join("tasks/adhoc-noop/result.json").exists());
}

#[tokio::test]
async fn swebench_local_task_loads_and_verifies() {
    let tmp = tempfile::tempdir().unwrap();
    let inst = tmp.path().join("inst1");
    write(&inst.join("repo/src/lib.rs"), "pub fn f() {}\n");
    // Hidden verification command passes on the materialized repo, proving the
    // adapter -> runner -> hidden-verify pipeline works locally.
    write(
        &inst.join("instance.json"),
        r#"{
  "instance_id": "example__repo-1",
  "repo": "example/repo",
  "problem_statement": "Improve f().",
  "test_command": "test -f src/lib.rs"
}"#,
    );

    let task = SweBenchAdapter::load_one(&inst).unwrap();
    let config = RunConfig {
        adapter: "swebench".to_string(),
        ..RunConfig::default()
    };
    let run = RunDir::create(tmp.path(), "run-swe").unwrap();
    let result = run_task(&task, &run, &config).await;
    assert!(result.hidden_present);
    assert_eq!(result.hidden_passed, Some(true));
    assert!(result.resolved, "hidden command passes on local fixture");
    assert!(run
        .root
        .join("tasks/example__repo-1/hidden-test.log")
        .exists());
}

#[tokio::test]
async fn repetitions_aggregate_and_index() {
    let tmp = tempfile::tempdir().unwrap();
    let task_dir = tmp.path().join("rep-task");
    write(&task_dir.join("repo/README.md"), "ok\n");
    write(
        &task_dir.join("task.toml"),
        r#"
schema_version = 2
id = "rep-noop"
title = "rep noop"
category = "negative"
prompt = "noop"
suites = ["rust-mini"]
[fixture]
kind = "copy_dir"
path = "repo"
[oracle]
kind = "none"
[[oracle.assertions]]
kind = "file_contains"
path = "README.md"
text = "ok"
[capabilities]
negative_noop = true
"#,
    );
    let task = load_task(&task_dir.join("task.toml")).unwrap();
    let run = RunDir::create(tmp.path(), "run-rep").unwrap();
    let results = run_tasks_repeated(&[task], &run, &RunConfig::default(), 3).await;
    assert_eq!(results.len(), 3);

    let stats = multirun::aggregate(&results, 3);
    assert_eq!(stats.total_tasks, 1);
    assert!((stats.pass_at_1 - 1.0).abs() < 1e-9);
    let per = &stats.per_task[0];
    assert_eq!(per.runs, 3);
    assert_eq!(per.resolved_count, 3);

    // Index round-trip.
    let index_path = tmp.path().join("index.jsonl");
    let git = GitContext::default();
    let entries: Vec<IndexEntry> = results
        .iter()
        .map(|r| IndexEntry::from_result(r, "run-rep", "2026-06-03T00:00:00Z", &git))
        .collect();
    index::append_entries(&index_path, &entries).unwrap();
    let loaded = index::load_entries(&index_path).unwrap();
    assert_eq!(loaded.len(), 3);
    assert!(loaded.iter().all(|e| e.resolved));
    assert_eq!(loaded[0].category.as_deref(), Some("negative"));
    assert!(loaded[0]
        .capabilities
        .contains(&"negative_noop".to_string()));
}

#[tokio::test]
async fn triage_finds_repeated_run_artifacts() {
    use crom_eval::triage;
    let tmp = tempfile::tempdir().unwrap();
    let task_dir = tmp.path().join("rep-task");
    write(&task_dir.join("repo/README.md"), "ok\n");
    write(
        &task_dir.join("task.toml"),
        r#"
schema_version = 2
id = "rep-triage"
title = "rep triage"
prompt = "noop"
[fixture]
kind = "copy_dir"
path = "repo"
[oracle]
kind = "none"
[[oracle.assertions]]
kind = "file_contains"
path = "README.md"
text = "ok"
"#,
    );
    let task = load_task(&task_dir.join("task.toml")).unwrap();
    let run = RunDir::create(tmp.path(), "run-rt").unwrap();
    // Repeated run writes artifacts under rep-<n>/tasks/<id>.
    let _ = run_tasks_repeated(&[task], &run, &RunConfig::default(), 2).await;

    // triage must find the per-repetition results, not only run/tasks.
    let results = triage::load_results(&run.root).unwrap();
    assert_eq!(results.len(), 2, "should find both repetitions");
    let shown = triage::show(&run.root, "rep-triage").unwrap();
    assert!(shown.contains("rep-triage"));
    let paths = triage::artifact_paths(&run.root, "rep-triage");
    assert!(paths.iter().any(|p| p.ends_with("result.json")));
}

#[test]
fn registry_exposes_all_adapters() {
    let reg = AdapterRegistry::new();
    let ids: Vec<&str> = reg.list().iter().map(|(id, _)| *id).collect();
    for id in ["internal", "directory", "swebench", "aider", "terminal"] {
        assert!(ids.contains(&id), "missing adapter {id}");
    }
}
