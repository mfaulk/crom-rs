//! Nominal cost and spend caps in eval results (#282), driven by the golden
//! provider, which reports a fixed 100 input / 20 output tokens per turn
//! over 5 turns for `golden-edit-recover`.

use std::time::Duration;

use crom_core::PricingTable;
use crom_eval::artifacts::RunDir;
use crom_eval::fixture::default_tasks_dir;
use crom_eval::providers::ProviderMode;
use crom_eval::report::RunSummary;
use crom_eval::result_schema::Outcome;
use crom_eval::runner::{
    run_task, run_tasks, run_tasks_repeated, EvalResult, EvalStatus, RunConfig,
};
use crom_eval::task::{discover_tasks, EvalTask};
use crom_eval::FailureClass;

fn find(id: &str) -> EvalTask {
    discover_tasks(&default_tasks_dir())
        .expect("discover tasks")
        .into_iter()
        .find(|t| t.id == id)
        .unwrap_or_else(|| panic!("task {id} not found"))
}

/// `golden-edit-recover` under a distinct id so artifacts don't collide.
fn golden_copy(id: &str) -> EvalTask {
    let mut t = find("golden-edit-recover");
    t.id = id.to_string();
    t.golden_script = Some("golden-edit-recover".to_string());
    t
}

/// $1000/M for input and output: 120 tokens per golden turn = $0.12.
fn golden_pricing() -> PricingTable {
    let toml = r#"
version = "test-1"
currency = "USD"
[[models]]
provider = "golden"
model = "golden"
input = 1000.0
output = 1000.0
source_url = "https://example.com/test"
retrieved_at = "2026-01-01"
"#;
    PricingTable::builtin().with_override(PricingTable::parse(toml).unwrap())
}

fn golden(pricing: PricingTable) -> RunConfig {
    RunConfig {
        provider: ProviderMode::Golden,
        timeout_grace: Duration::from_secs(20),
        pricing,
        ..RunConfig::default()
    }
}

async fn run_one(task: &EvalTask, config: &RunConfig) -> (tempfile::TempDir, EvalResult) {
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "cost").unwrap();
    let r = run_task(task, &run, config).await;
    (tmp, r)
}

fn approx(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-9)
}

/// A model with a pricing entry gets a nominal cost and the table version in repro.
#[tokio::test]
async fn priced_model_records_nominal_cost() {
    let (_t, r) = run_one(&find("golden-edit-recover"), &golden(golden_pricing())).await;
    assert!(r.resolved);
    assert!(approx(r.cost_usd, 0.60), "{:?}", r.cost_usd);
    assert_eq!(r.cost_unknown_reason, None);
    let repro = r.repro.unwrap();
    assert!(repro.pricing_source_version.unwrap().starts_with("test-1+"));
}

/// Without a pricing entry the cost is None with reason `no_pricing_entry`.
#[tokio::test]
async fn unpriced_model_records_reason() {
    let (_t, r) = run_one(
        &find("golden-edit-recover"),
        &golden(PricingTable::builtin()),
    )
    .await;
    assert_eq!(r.cost_usd, None);
    assert_eq!(r.cost_unknown_reason.as_deref(), Some("no_pricing_entry"));
}

/// A priced model whose provider reports no usage is unknown, never zero.
#[tokio::test]
async fn unreported_usage_is_unknown_cost() {
    let toml = r#"
version = "test-fake"
currency = "USD"
[[models]]
provider = "fake"
model = "fake-echo"
input = 1.0
output = 1.0
source_url = "https://example.com/test"
retrieved_at = "2026-01-01"
"#;
    let config = RunConfig {
        provider: ProviderMode::Fake,
        timeout_grace: Duration::from_secs(10),
        pricing: PricingTable::parse(toml).unwrap(),
        ..RunConfig::default()
    };
    let (_t, r) = run_one(&find("smoke-noop"), &config).await;
    assert_eq!(r.cost_usd, None);
    assert_eq!(r.cost_unknown_reason.as_deref(), Some("usage_unknown"));
}

/// A per-task cost cap stops the agent deterministically and classifies the failure.
#[tokio::test]
async fn per_task_cost_cap_stops_run() {
    let mut config = golden(golden_pricing());
    config.max_cost_usd = Some(0.30);
    let (_t, r) = run_one(&find("golden-edit-recover"), &config).await;
    // $0.12, $0.24 within the cap; turn 3 reaches $0.36 and stops the run.
    assert_eq!(r.model_round_trips, 3);
    assert!(approx(r.cost_usd, 0.36), "{:?}", r.cost_usd);
    assert!(!r.resolved);
    assert_eq!(r.failure_class, Some(FailureClass::BudgetExceeded));
    assert_eq!(
        r.budget_exceeded.as_deref(),
        Some("budget.max_cost_exceeded")
    );
    assert_eq!(r.outcome(), Outcome::Failed);
}

/// A task's `limits.max_cost_usd` is enforced and the tighter of task and CLI caps wins.
#[tokio::test]
async fn task_limit_cost_cap_is_enforced() {
    let mut task = find("golden-edit-recover");
    task.limits.max_cost_usd = Some(0.15);
    let mut config = golden(golden_pricing());
    config.max_cost_usd = Some(10.0);
    let (_t, r) = run_one(&task, &config).await;
    assert_eq!(r.model_round_trips, 2);
    assert_eq!(
        r.budget_exceeded.as_deref(),
        Some("budget.max_cost_exceeded")
    );
    assert_eq!(r.repro.unwrap().runtime.max_cost_usd, Some(0.15));
}

/// A task's `limits.max_tool_calls` is enforced by the runner.
#[tokio::test]
async fn task_limit_max_tool_calls_is_enforced() {
    let mut task = find("golden-edit-recover");
    task.limits.max_tool_calls = Some(1);
    let (_t, r) = run_one(&task, &golden(PricingTable::builtin())).await;
    assert_eq!(r.tool_calls, 1);
    assert!(!r.resolved);
    assert_eq!(
        r.budget_exceeded.as_deref(),
        Some("budget.max_tool_calls_exceeded")
    );
    assert_eq!(r.failure_class, Some(FailureClass::BudgetExceeded));
}

/// A run-level spend cap stops scheduling once spent, marks the rest skipped(budget), and is summarized.
#[tokio::test]
async fn run_cost_cap_skips_remaining_tasks() {
    let tasks = vec![golden_copy("g-a"), golden_copy("g-b"), golden_copy("g-c")];
    let mut config = golden(golden_pricing());
    config.max_run_cost_usd = Some(1.00);
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "run-cap").unwrap();
    let results = run_tasks(&tasks, &run, &config).await;

    // g-a: $0.60 resolved. g-b gets the remaining $0.40 as its cap and
    // stops at $0.48. Spend is now $1.08 >= $1.00, so g-c never runs.
    assert!(results[0].resolved);
    assert!(approx(results[0].cost_usd, 0.60));
    assert_eq!(
        results[1].budget_exceeded.as_deref(),
        Some("budget.max_cost_exceeded")
    );
    assert!(approx(results[1].cost_usd, 0.48));
    assert_eq!(results[2].status, EvalStatus::Skipped);
    assert_eq!(results[2].outcome(), Outcome::Skipped);
    assert_eq!(results[2].model_round_trips, 0);
    assert!(results[2]
        .skipped_reason
        .as_deref()
        .unwrap()
        .starts_with("budget:"));

    let summary = RunSummary::from_results("run-cap", "golden", &results);
    assert!(
        approx(summary.total_cost_usd, 1.08),
        "{:?}",
        summary.total_cost_usd
    );
    assert_eq!(summary.budget_skipped, 1);
    assert!(summary.to_markdown().contains("skipped (budget): 1"));
}

/// The run-level cap spans repetitions, not each repetition separately.
#[tokio::test]
async fn run_cost_cap_spans_repetitions() {
    let tasks = vec![golden_copy("g-rep")];
    let mut config = golden(golden_pricing());
    config.max_run_cost_usd = Some(1.00);
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "run-cap-reps").unwrap();
    let results = run_tasks_repeated(&tasks, &run, &config, 3).await;
    assert_eq!(results.len(), 3);
    assert!(approx(results[0].cost_usd, 0.60));
    assert!(approx(results[1].cost_usd, 0.48));
    assert_eq!(results[2].status, EvalStatus::Skipped);
}

/// With a run cap but no price for the model, no task runs (fail-closed).
#[tokio::test]
async fn run_cost_cap_without_price_skips_everything() {
    let tasks = vec![golden_copy("g-x"), golden_copy("g-y")];
    let mut config = golden(PricingTable::builtin());
    config.max_run_cost_usd = Some(5.0);
    let tmp = tempfile::tempdir().unwrap();
    let run = RunDir::create(tmp.path(), "run-cap-unpriced").unwrap();
    let results = run_tasks(&tasks, &run, &config).await;
    assert!(results.iter().all(|r| r.status == EvalStatus::Skipped
        && r.skipped_reason.as_deref().unwrap().starts_with("budget:")));
}
