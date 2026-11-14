//! End-to-end tests for the eval harness itself, using the offline fake and
//! golden providers. These run in ordinary CI without live credentials.

use std::time::Duration;

use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::ProviderMode;
use crom_eval::runner::{run_task, EvalStatus, RunConfig};
use crom_eval::task::{discover_tasks, EvalTask};
use crom_eval::{artifacts::RunDir, report};

fn load_all() -> Vec<EvalTask> {
    discover_tasks(&default_tasks_dir()).expect("discover tasks")
}

fn find<'a>(tasks: &'a [EvalTask], id: &str) -> &'a EvalTask {
    tasks
        .iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} not found"))
}

#[test]
fn discovers_at_least_eight_tasks() {
    let tasks = load_all();
    assert!(
        tasks.len() >= 8,
        "expected >= 8 tasks, found {}",
        tasks.len()
    );
    // Every manifest parsed and resolved cleanly.
    for t in &tasks {
        assert!(!t.id.is_empty());
        assert!(!t.prompt.trim().is_empty());
    }
    // Required suites exist.
    assert!(tasks.iter().any(|t| t.in_suite("smoke")));
    assert!(tasks.iter().any(|t| t.in_suite("rust-small")));
    // A planning task and safety tasks exist.
    assert!(tasks.iter().any(|t| t
        .oracle
        .behavior
        .iter()
        .any(|b| matches!(b, crom_eval::task::BehaviorSpec::MustEmitPlan))));
    assert!(tasks.iter().any(|t| t.in_suite("safety")));
}

/// Every shipped task is a v3 manifest labelled with a kind and level.
#[test]
fn every_shipped_task_is_v3_with_kind_and_level() {
    for t in load_all() {
        assert_eq!(t.schema_version, 3, "{} is not schema v3", t.id);
        assert!(t.kind.is_some(), "{} has no kind", t.id);
        assert!(t.level.is_some(), "{} has no level", t.id);
        assert!(t.task_version.is_some(), "{} has no task_version", t.id);
    }
}

/// Tasks in the scripted/offline CI suites are harness tasks at L0, never capability tasks.
#[test]
fn offline_ci_suites_are_harness_l0() {
    use crom_eval::schema::{Level, TaskKind};
    for t in load_all() {
        if ["smoke", "golden", "safety", "repair-smoke", "repair-mini"]
            .iter()
            .any(|s| t.in_suite(s))
        {
            assert_eq!(t.kind, Some(TaskKind::Harness), "{}", t.id);
            assert_eq!(t.level, Some(Level::L0), "{}", t.id);
        }
    }
}

#[tokio::test]
async fn smoke_noop_passes_under_fake_provider() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-smoke").unwrap();
    let tasks = load_all();
    let task = find(&tasks, "smoke-noop");

    let config = RunConfig {
        provider: ProviderMode::Fake,
        timeout_grace: Duration::from_secs(10),
        ..RunConfig::default()
    };
    let result = run_task(task, &run, &config).await;

    assert_eq!(
        result.status,
        EvalStatus::Passed,
        "smoke-noop should pass under fake provider; score notes: {:?}",
        result.score.notes
    );
    // Artifacts exist and are self-contained.
    assert!(result.events_path.exists());
    assert!(result.report_path.exists());
    assert!(result.workspace_path.exists());
    assert!(run.task("smoke-noop").unwrap().score_path().exists());
}

#[tokio::test]
async fn golden_edit_recover_passes_and_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-golden").unwrap();
    let tasks = load_all();
    let task = find(&tasks, "golden-edit-recover");

    let config = RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        ..RunConfig::default()
    };
    let result = run_task(task, &run, &config).await;

    assert_eq!(
        result.status,
        EvalStatus::Passed,
        "golden-edit-recover should pass; notes: {:?}",
        result.score.notes
    );
    // The agent made a real edit, so a patch was produced.
    assert!(result.patch.is_some(), "expected a non-empty patch");
    assert!(result.patch.unwrap().contains("value=42"));
    // It used at least the Edit tool and ran a command (the recovery path).
    assert!(result.tool_calls >= 2);
    assert_eq!(result.checks_passed, result.checks_total);

    // The command oracle confirms the post-run workspace satisfies the goal.
    let data = std::fs::read_to_string(result.workspace_path.join("data.txt")).unwrap();
    assert!(data.contains("value=42"));
}

#[tokio::test]
async fn run_summary_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-summary").unwrap();
    let tasks = load_all();
    let smoke: Vec<EvalTask> = tasks
        .iter()
        .filter(|t| t.in_suite("smoke"))
        .cloned()
        .collect();
    assert!(!smoke.is_empty());

    let config = RunConfig {
        provider: ProviderMode::Fake,
        timeout_grace: Duration::from_secs(10),
        ..RunConfig::default()
    };
    let results = crom_eval::runner::run_tasks(&smoke, &run, &config).await;
    let summary = report::write_summaries(&run, "fake", &results).unwrap();

    assert_eq!(summary.total, smoke.len());
    // All smoke tasks pass deterministically under the fake provider.
    assert_eq!(
        summary.passed, summary.total,
        "all smoke tasks should pass under fake; summary: {summary:?}"
    );
    assert!(run.summary_json_path().exists());
    assert!(run.summary_md_path().exists());
}

/// Returns true when a fixture's `Cargo.toml` declares any registry dependency.
fn fixture_has_dependencies(task: &EvalTask) -> bool {
    let Some(rel) = &task.fixture.path else {
        return false;
    };
    let Ok(manifest) = std::fs::read_to_string(task.dir.join(rel).join("Cargo.toml")) else {
        return false;
    };
    let mut in_deps = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            in_deps = matches!(
                line,
                "[dependencies]" | "[dev-dependencies]" | "[build-dependencies]"
            );
        } else if in_deps && !line.is_empty() && !line.starts_with('#') {
            return true;
        }
    }
    false
}

/// Every task whose Rust fixture pulls crates.io dependencies declares `requires_network = true`.
#[test]
fn fixtures_with_registry_dependencies_require_network() {
    let offenders: Vec<String> = load_all()
        .iter()
        .filter(|t| fixture_has_dependencies(t) && !t.requires_network)
        .map(|t| t.id.clone())
        .collect();
    assert!(
        offenders.is_empty(),
        "tasks with external dependencies must set requires_network = true: {offenders:?}"
    );
}
