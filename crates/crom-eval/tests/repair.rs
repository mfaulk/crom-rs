//! End-to-end tests for the M16 repair loop, exercised through the real eval
//! runner with the offline golden provider. No live credentials, no network,
//! no Docker.

use std::time::Duration;

use crom_core::repair::RepairPolicy;
use crom_eval::artifacts::RunDir;
use crom_eval::failure::FailureClass;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::ProviderMode;
use crom_eval::repair::FinalValidationStatus;
use crom_eval::runner::{run_task, EvalStatus, RunConfig};
use crom_eval::task::discover_tasks;

fn find_task(id: &str) -> crom_eval::task::EvalTask {
    let tasks = discover_tasks(&default_tasks_dir()).expect("discover tasks");
    tasks
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} not found"))
}

#[tokio::test]
async fn repair_grep_recover_resolves_and_records_metrics() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-repair-recover").unwrap();
    let task = find_task("repair-grep-recover");

    let config = RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        repair: RepairPolicy {
            enabled: true,
            max_attempts: 3,
            ..RepairPolicy::default()
        },
        ..RunConfig::default()
    };
    let result = run_task(&task, &run, &config).await;

    assert_eq!(
        result.status,
        EvalStatus::Passed,
        "expected pass; notes: {:?}",
        result.score.notes
    );
    assert!(result.resolved);
    // The oracle passed on the first attempt, so no repair attempts were
    // needed and the final validation is recorded as passed.
    assert_eq!(result.repair.repair_attempt_count, 0);
    assert_eq!(
        result.repair.final_validation_status,
        FinalValidationStatus::Passed
    );
    // `grep` is (correctly) classified as inspection, not a cargo-style
    // validation command, so `validation_after_last_edit` reflects that the
    // classifier saw no recognized validation after the edit. The metric is
    // honest rather than fabricated.
    assert_eq!(result.repair.validation_after_last_edit, Some(false));
}

#[tokio::test]
async fn repair_grep_trap_detects_repeated_failure_and_stops() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-repair-trap").unwrap();
    let task = find_task("repair-grep-trap");

    let config = RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        repair: RepairPolicy {
            enabled: true,
            max_attempts: 4,
            max_same_failure_repeats: 2,
            stop_on_repeated_identical_failure: true,
            ..RepairPolicy::default()
        },
        ..RunConfig::default()
    };
    let result = run_task(&task, &run, &config).await;

    // This is a negative task: the scripted provider never fixes the value.
    assert!(!result.resolved, "trap task must not resolve");
    assert!(
        result.repair.repair_attempt_count >= 1,
        "expected at least one repair attempt, got {}",
        result.repair.repair_attempt_count
    );
    assert!(
        result.repair.same_error_repeated,
        "the identical failure should be detected as repeated"
    );
    // The refined failure class reflects the repair-loop outcome.
    let fc = result.failure_class.expect("a failure class");
    assert!(
        matches!(
            fc,
            FailureClass::RepeatedSameFailure
                | FailureClass::RepairAttemptLimitExceeded
                | FailureClass::UnrecoveredTestFailure
                | FailureClass::UnrecoveredCompileError
                | FailureClass::PublicTestFailure
        ),
        "unexpected failure class: {fc:?}"
    );

    // Repair artifacts are emitted.
    let dir = run.root.join("tasks").join(&task.id);
    assert!(dir.join("repair.json").exists(), "repair.json missing");
    assert!(dir.join("repair.md").exists(), "repair.md missing");
    assert!(
        dir.join("validation-attempts.jsonl").exists(),
        "validation-attempts.jsonl missing"
    );

    let repair_json = std::fs::read_to_string(dir.join("repair.json")).unwrap();
    let trace: serde_json::Value = serde_json::from_str(&repair_json).unwrap();
    assert!(!trace["attempts"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn repair_disabled_records_no_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "test-repair-off").unwrap();
    let task = find_task("repair-grep-trap");

    let config = RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        repair: RepairPolicy::disabled(),
        ..RunConfig::default()
    };
    let result = run_task(&task, &run, &config).await;

    assert!(!result.repair.enabled);
    assert_eq!(result.repair.repair_attempt_count, 0);
    // A diagnostic is still parsed from the single failed validation.
    assert!(
        result.repair.diagnostic_parse_success || result.repair.diagnostic_primary_kind.is_some()
    );
}
