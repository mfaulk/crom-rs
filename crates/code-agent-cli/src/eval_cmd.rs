//! `code-agent eval` — the coding-agent capability evaluation harness.
//!
//! M14 subcommands: `list`, `run`, `report`, `compare`.
//! M15 subcommands: `adapters`, `validate-task`, `validate-suite`, `history`,
//! `trend`, `regressions`, `task-history`, `triage`, `show`, `open`, plus an
//! adapter-, isolation-, repetition-, and prompt-variant-aware `run`.
//!
//! Runs drive the agent through the SDK runtime; provider selection is `fake`
//! (default, offline) / `golden` (scripted) / a live provider name such as
//! `anthropic` (opt-in via `CODE_AGENT_EVAL_LIVE=1`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use code_agent_eval::adapter::AdapterRegistry;
use code_agent_eval::artifacts::RunDir;
use code_agent_eval::index::{self, GitContext, IndexEntry};
use code_agent_eval::isolation::IsolationKind;
use code_agent_eval::multirun;
use code_agent_eval::providers::ProviderMode;
use code_agent_eval::report;
use code_agent_eval::runner::{run_tasks_repeated, EvalResult, EvalStatus, RunConfig};
use code_agent_eval::task::{discover_tasks, EvalTask};
use code_agent_eval::{triage, validate, RUNS_ROOT};

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// List available eval tasks (id, title, tags, suites).
    List(EvalListArgs),
    /// List available task adapters.
    Adapters,
    /// Run a single task or a suite.
    Run(EvalRunArgs),
    /// Validate a single task (path or id) without invoking a model.
    ValidateTask(ValidateTaskArgs),
    /// Validate every task in a suite (optionally repeated for flake detection).
    ValidateSuite(ValidateSuiteArgs),
    /// Print the summary from a prior run.
    Report(EvalReportArgs),
    /// Compare two prior runs.
    Compare(EvalCompareArgs),
    /// Show per-run rollups from the result index.
    History(HistoryArgs),
    /// Show per-task pass-rate trend for a suite.
    Trend(TrendArgs),
    /// Show fixed/regressed tasks between two runs (from the index).
    Regressions(RegressionsArgs),
    /// Show the history of a single task across runs.
    TaskHistory(TaskHistoryArgs),
    /// Group a run's failures by failure class.
    Triage(TriageArgs),
    /// Show a debuggable view of one task in a run.
    Show(ShowArgs),
    /// List artifact paths for one task in a run.
    Open(ShowArgs),
}

#[derive(Parser, Debug)]
pub struct EvalListArgs {
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    #[arg(long)]
    pub suite: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    /// Include private suites/tasks.
    #[arg(long)]
    pub include_private: bool,
}

#[derive(Parser, Debug)]
pub struct EvalRunArgs {
    /// A task id (internal adapter) or a path (directory/external adapters).
    pub target: Option<String>,
    /// Run every task in this suite (internal adapter; default `smoke`).
    #[arg(long)]
    pub suite: Option<String>,
    /// Adapter: internal | directory | swebench | aider | terminal.
    #[arg(long, default_value = "internal")]
    pub adapter: String,
    /// Provider mode: `fake` (default), `golden`, or a live provider name.
    #[arg(long, default_value = "fake")]
    pub provider: String,
    /// Directory containing internal tasks (defaults to bundled eval/tasks).
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    /// Root directory for run artifacts.
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
    /// Isolation backend: local (default) | docker | podman.
    #[arg(long, default_value = "local")]
    pub isolation: String,
    /// Allow network access during verification.
    #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
    pub network: bool,
    /// Container image (container backends only).
    #[arg(long)]
    pub container_image: Option<String>,
    /// CPU limit in cores (container backends only).
    #[arg(long)]
    pub cpu_limit: Option<f64>,
    /// Memory limit in bytes (container backends only).
    #[arg(long)]
    pub memory_limit: Option<u64>,
    /// Number of repetitions for multi-run statistics.
    #[arg(long, default_value_t = 1)]
    pub repetitions: usize,
    /// Prompt variant name, or `random`.
    #[arg(long)]
    pub prompt_variant: Option<String>,
    /// Max attempts (Aider-style retry with test feedback).
    #[arg(long, default_value_t = 1)]
    pub attempts: usize,
    /// Include private suites/tasks in suite selection.
    #[arg(long)]
    pub include_private: bool,
    /// Do not append results to the regression index.
    #[arg(long)]
    pub no_index: bool,
    /// Do not exit non-zero when tasks fail.
    #[arg(long)]
    pub allow_failures: bool,
}

#[derive(Parser, Debug)]
pub struct ValidateTaskArgs {
    /// A task id (internal) or a path to a task directory / task.toml.
    pub target: String,
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct ValidateSuiteArgs {
    pub suite: String,
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    /// Repeat verification N times to classify flakiness.
    #[arg(long, default_value_t = 1)]
    pub repeat: usize,
    #[arg(long)]
    pub include_private: bool,
}

#[derive(Parser, Debug)]
pub struct EvalReportArgs {
    pub run: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct EvalCompareArgs {
    pub baseline: String,
    pub candidate: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct HistoryArgs {
    #[arg(long)]
    pub index: Option<PathBuf>,
    /// Limit to the most recent N runs.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Parser, Debug)]
pub struct TrendArgs {
    /// Suite to trend (omit for all tasks).
    pub suite: Option<String>,
    #[arg(long)]
    pub index: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct RegressionsArgs {
    #[arg(long)]
    pub baseline: String,
    #[arg(long)]
    pub candidate: String,
    #[arg(long)]
    pub index: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct TaskHistoryArgs {
    pub task_id: String,
    #[arg(long)]
    pub index: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct TriageArgs {
    pub run: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct ShowArgs {
    pub run: String,
    pub task_id: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

/// Dispatch an `eval` subcommand.
pub async fn run(cmd: EvalCommand) -> Result<()> {
    match cmd {
        EvalCommand::List(args) => cmd_list(args),
        EvalCommand::Adapters => cmd_adapters(),
        EvalCommand::Run(args) => cmd_run(args).await,
        EvalCommand::ValidateTask(args) => cmd_validate_task(args).await,
        EvalCommand::ValidateSuite(args) => cmd_validate_suite(args).await,
        EvalCommand::Report(args) => cmd_report(args),
        EvalCommand::Compare(args) => cmd_compare(args),
        EvalCommand::History(args) => cmd_history(args),
        EvalCommand::Trend(args) => cmd_trend(args),
        EvalCommand::Regressions(args) => cmd_regressions(args),
        EvalCommand::TaskHistory(args) => cmd_task_history(args),
        EvalCommand::Triage(args) => cmd_triage(args),
        EvalCommand::Show(args) => cmd_show(args, false),
        EvalCommand::Open(args) => cmd_show(args, true),
    }
}

fn resolve_tasks_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        if dir.exists() {
            return Ok(dir);
        }
        return Err(anyhow!("tasks dir does not exist: {}", dir.display()));
    }
    let candidates = [
        PathBuf::from("crates/eval/eval/tasks"),
        PathBuf::from("eval/tasks"),
        code_agent_eval::fixture::default_tasks_dir(),
    ];
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
    }
    Err(anyhow!(
        "could not locate an eval tasks directory; pass --tasks-dir"
    ))
}

fn resolve_runs_dir(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| PathBuf::from(RUNS_ROOT))
}

fn resolve_index(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(index::default_index_path)
}

fn cmd_adapters() -> Result<()> {
    let reg = AdapterRegistry::new();
    println!("available adapters:\n");
    for (id, desc) in reg.list() {
        println!("  {id:<10} {desc}");
    }
    Ok(())
}

fn cmd_list(args: EvalListArgs) -> Result<()> {
    let dir = resolve_tasks_dir(args.tasks_dir)?;
    let tasks = discover_tasks(&dir).map_err(|e| anyhow!("{e}"))?;
    let filtered: Vec<&EvalTask> = tasks
        .iter()
        .filter(|t| args.include_private || !t.is_private())
        .filter(|t| args.suite.as_ref().is_none_or(|s| t.in_suite(s)))
        .filter(|t| {
            args.tag
                .as_ref()
                .is_none_or(|tag| t.tags.iter().any(|x| x == tag))
        })
        .collect();

    if filtered.is_empty() {
        println!("no matching tasks in {}", dir.display());
        return Ok(());
    }
    println!("{} task(s) in {}:\n", filtered.len(), dir.display());
    for t in filtered {
        let mut suffix = String::new();
        if t.is_quarantined() {
            suffix.push_str(" [quarantined]");
        }
        if t.is_private() {
            suffix.push_str(" [private]");
        }
        println!("{}{}", t.id, suffix);
        println!("    {}", t.title);
        if let Some(cat) = &t.category {
            println!("    category: {cat}");
        }
        if !t.tags.is_empty() {
            println!("    tags:   {}", t.tags.join(", "));
        }
        if !t.suites.is_empty() {
            println!("    suites: {}", t.suites.join(", "));
        }
        let caps = t.capabilities.enabled_labels();
        if !caps.is_empty() {
            println!("    caps:   {}", caps.join(", "));
        }
    }
    Ok(())
}

const KNOWN_LIVE_PROVIDERS: &[&str] = &["anthropic"];

/// Select the tasks to run via the requested adapter.
fn select_tasks(args: &EvalRunArgs) -> Result<Vec<EvalTask>> {
    let registry = AdapterRegistry::new();
    let adapter = registry.get(&args.adapter)?;

    if args.adapter == "internal" {
        let dir = resolve_tasks_dir(args.tasks_dir.clone())?;
        let all = adapter.discover_tasks(&dir)?;
        if let Some(id) = &args.target {
            let task = all
                .iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| anyhow!("no task with id '{id}' in {}", dir.display()))?;
            return Ok(vec![task.clone()]);
        }
        let suite = args.suite.as_deref().unwrap_or("smoke");
        let tasks: Vec<EvalTask> = all
            .iter()
            .filter(|t| t.in_suite(suite))
            .filter(|t| args.include_private || !t.is_private())
            .cloned()
            .collect();
        if tasks.is_empty() {
            return Err(anyhow!(
                "suite '{suite}' matched no tasks in {}",
                dir.display()
            ));
        }
        return Ok(tasks);
    }

    // Non-internal adapters operate on a path target.
    let target = args
        .target
        .as_ref()
        .ok_or_else(|| anyhow!("adapter '{}' requires a path target", args.adapter))?;
    adapter.discover_tasks(Path::new(target))
}

async fn cmd_run(args: EvalRunArgs) -> Result<()> {
    let provider = ProviderMode::parse(&args.provider);
    if let ProviderMode::Live(name) = &provider {
        if !KNOWN_LIVE_PROVIDERS.contains(&name.as_str()) {
            return Err(anyhow!(
                "unknown provider '{name}'; expected one of: fake, golden, {}",
                KNOWN_LIVE_PROVIDERS.join(", ")
            ));
        }
    }
    let isolation = IsolationKind::parse(&args.isolation).map_err(|e| anyhow!("{e}"))?;
    let selected = select_tasks(&args)?;

    let runs_dir = resolve_runs_dir(args.runs_dir.clone());
    let run_id = generate_run_id();
    let run = RunDir::create(&runs_dir, &run_id)
        .with_context(|| format!("creating run dir under {}", runs_dir.display()))?;

    let repetitions = args.repetitions.max(1);
    println!(
        "running {} task(s) x{} via adapter '{}' provider '{}' isolation '{}' → {}",
        selected.len(),
        repetitions,
        args.adapter,
        provider.label(),
        isolation.id(),
        run.root.display()
    );

    let config = RunConfig {
        provider: provider.clone(),
        timeout_grace: Duration::from_secs(30),
        adapter: args.adapter.clone(),
        isolation: isolation.clone(),
        container_image: args.container_image.clone(),
        network: args.network,
        cpu_limit: args.cpu_limit,
        memory_limit: args.memory_limit,
        prompt_variant: args.prompt_variant.clone(),
        repetition: 0,
        seed: 0,
        attempts: args.attempts.max(1),
    };

    let results = run_tasks_repeated(&selected, &run, &config, repetitions).await;
    let summary = report::write_summaries(&run, &provider.label(), &results)
        .map_err(|e| anyhow!("writing summaries: {e}"))?;

    // Multi-run statistics.
    if repetitions > 1 {
        let stats = multirun::aggregate(&results, repetitions);
        let _ = std::fs::write(
            run.root.join("repetitions.json"),
            serde_json::to_string_pretty(&stats).unwrap_or_default(),
        );
        println!("\n{}", multirun::to_markdown(&stats));
    }

    write_behavior_summary(&run.root, &results);
    write_run_metadata(&run.root, &args, &provider, &isolation, repetitions);

    // Index + per-task index-entry.json.
    if !args.no_index {
        let git = GitContext::detect();
        let ts = now_rfc3339();
        let entries: Vec<IndexEntry> = results
            .iter()
            .map(|r| IndexEntry::from_result(r, &run_id, &ts, &git))
            .collect();
        let index_path = resolve_index(None);
        if let Err(e) = index::append_entries(&index_path, &entries) {
            eprintln!(
                "warning: failed to write index {}: {e}",
                index_path.display()
            );
        }
        for (r, e) in results.iter().zip(&entries) {
            let dir = if repetitions > 1 {
                run.root
                    .join(format!("rep-{}", r.repetition))
                    .join("tasks")
                    .join(&r.task_id)
            } else {
                run.root.join("tasks").join(&r.task_id)
            };
            if dir.exists() {
                let _ = std::fs::write(
                    dir.join("index-entry.json"),
                    serde_json::to_string_pretty(e).unwrap_or_default(),
                );
            }
        }
    }

    // Per-task console output (first repetition only for brevity).
    println!();
    for r in results.iter().filter(|r| r.repetition == 0) {
        let mark = if r.quarantined {
            "Q"
        } else {
            r.status.symbol()
        };
        println!(
            "  [{}] {:<28} resolved={} score {:.3}  ({}/{} checks, {} tools)",
            mark,
            r.task_id,
            r.resolved,
            r.score.total,
            r.checks_passed,
            r.checks_total,
            r.tool_calls
        );
        if let Some(reason) = &r.skipped_reason {
            println!("           skipped: {reason}");
        }
        for w in &r.behavior_warnings {
            println!("           ⚠ {w}");
        }
    }
    println!(
        "\nresolved {}/{} — {} passed, {} partial, {} failed, {} errored, {} timed out, {} skipped (mean score {:.3})",
        summary.resolved,
        summary.total,
        summary.passed,
        summary.partial,
        summary.failed,
        summary.errored,
        summary.timed_out,
        summary.skipped,
        summary.mean_score
    );
    if summary.quarantined > 0 {
        println!(
            "({} quarantined task result(s) reported separately)",
            summary.quarantined
        );
    }
    println!("artifacts: {}", run.root.display());

    // Gate: non-resolved, non-skip, non-quarantined tasks fail the command.
    let failures = results
        .iter()
        .filter(|r| !r.resolved && !r.quarantined && !matches!(r.status, EvalStatus::Skipped))
        .count();
    if failures > 0 && !args.allow_failures {
        return Err(anyhow!(
            "{failures} task result(s) did not resolve (use --allow-failures to ignore)"
        ));
    }
    Ok(())
}

fn write_behavior_summary(run_root: &Path, results: &[EvalResult]) {
    let resolved_but_risky: Vec<&str> = results
        .iter()
        .filter(|r| r.resolved && !r.behavior_warnings.is_empty())
        .map(|r| r.task_id.as_str())
        .collect();
    let failed_without_validation: Vec<&str> = results
        .iter()
        .filter(|r| !r.resolved && r.behavior.test_after_edit == Some(false))
        .map(|r| r.task_id.as_str())
        .collect();
    let repeated_errors: Vec<&str> = results
        .iter()
        .filter(|r| r.behavior.same_error_repeated == Some(true))
        .map(|r| r.task_id.as_str())
        .collect();
    let changed_tests: Vec<&str> = results
        .iter()
        .filter(|r| {
            r.behavior.deleted_tests == Some(true) || r.behavior.weakened_assertions == Some(true)
        })
        .map(|r| r.task_id.as_str())
        .collect();
    let skipped_reading: Vec<&str> = results
        .iter()
        .filter(|r| r.behavior.read_before_write == Some(false))
        .map(|r| r.task_id.as_str())
        .collect();
    let broad_edits: Vec<&str> = results
        .iter()
        .filter(|r| r.behavior.large_blind_edit == Some(true))
        .map(|r| r.task_id.as_str())
        .collect();
    let summary = serde_json::json!({
        "resolved_but_risky": resolved_but_risky,
        "failed_without_validation": failed_without_validation,
        "repeated_identical_errors": repeated_errors,
        "changed_tests": changed_tests,
        "skipped_reading_before_writing": skipped_reading,
        "broad_or_suspicious_edits": broad_edits,
    });
    let _ = std::fs::write(
        run_root.join("behavior-summary.json"),
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}

fn write_run_metadata(
    run_root: &Path,
    args: &EvalRunArgs,
    provider: &ProviderMode,
    isolation: &IsolationKind,
    repetitions: usize,
) {
    let meta = serde_json::json!({
        "adapter": args.adapter,
        "provider": provider.label(),
        "isolation": isolation.id(),
        "network": args.network,
        "repetitions": repetitions,
        "attempts": args.attempts,
        "prompt_variant": args.prompt_variant,
    });
    let _ = std::fs::write(
        run_root.join("run.json"),
        serde_json::to_string_pretty(&meta).unwrap_or_default(),
    );
}

async fn cmd_validate_task(args: ValidateTaskArgs) -> Result<()> {
    let task = resolve_single_task(&args.target, args.tasks_dir)?;
    let report = validate::validate_task(&task).await;
    println!("{}", report.to_markdown());
    if !report.ok() {
        return Err(anyhow!("{} validation error(s)", report.errors()));
    }
    Ok(())
}

async fn cmd_validate_suite(args: ValidateSuiteArgs) -> Result<()> {
    let dir = resolve_tasks_dir(args.tasks_dir)?;
    let all = discover_tasks(&dir).map_err(|e| anyhow!("{e}"))?;
    let tasks: Vec<&EvalTask> = all
        .iter()
        .filter(|t| t.in_suite(&args.suite))
        .filter(|t| args.include_private || !t.is_private())
        .collect();
    if tasks.is_empty() {
        return Err(anyhow!("suite '{}' matched no tasks", args.suite));
    }
    let mut total_errors = 0usize;
    for task in tasks {
        let report = validate::validate_task(task).await;
        total_errors += report.errors();
        println!("{}", report.to_markdown());
        if args.repeat > 1 {
            let verdict = validate::flake_detect(task, args.repeat).await;
            println!("flake verdict ({}x): {:?}\n", args.repeat, verdict);
        }
    }
    if total_errors > 0 {
        return Err(anyhow!("{total_errors} validation error(s) across suite"));
    }
    Ok(())
}

fn resolve_single_task(target: &str, tasks_dir: Option<PathBuf>) -> Result<EvalTask> {
    let p = Path::new(target);
    if p.exists() {
        if p.is_dir() {
            return code_agent_eval::adapter::DirectoryAdapter::load_one(p);
        }
        // A task.toml file.
        return code_agent_eval::task::load_task(p).map_err(|e| anyhow!("{e}"));
    }
    // Treat as an internal task id.
    let dir = resolve_tasks_dir(tasks_dir)?;
    let all = discover_tasks(&dir).map_err(|e| anyhow!("{e}"))?;
    all.into_iter()
        .find(|t| t.id == target)
        .ok_or_else(|| anyhow!("no task '{target}' (as path or internal id)"))
}

fn cmd_report(args: EvalReportArgs) -> Result<()> {
    let run_dir = resolve_run_path(&args.run, args.runs_dir);
    let md_path = if run_dir.is_dir() {
        run_dir.join("summary.md")
    } else {
        run_dir.clone()
    };
    if md_path.exists() && md_path.extension().map(|e| e == "md").unwrap_or(false) {
        let text = std::fs::read_to_string(&md_path)
            .with_context(|| format!("reading {}", md_path.display()))?;
        println!("{text}");
        return Ok(());
    }
    let summary = report::load_summary(&run_dir)
        .with_context(|| format!("loading summary from {}", run_dir.display()))?;
    println!("{}", summary.to_markdown());
    Ok(())
}

fn cmd_compare(args: EvalCompareArgs) -> Result<()> {
    let base_path = resolve_run_path(&args.baseline, args.runs_dir.clone());
    let cand_path = resolve_run_path(&args.candidate, args.runs_dir);
    let baseline = report::load_summary(&base_path)
        .with_context(|| format!("loading baseline {}", base_path.display()))?;
    let candidate = report::load_summary(&cand_path)
        .with_context(|| format!("loading candidate {}", cand_path.display()))?;
    println!("{}", report::compare(&baseline, &candidate));
    Ok(())
}

fn cmd_history(args: HistoryArgs) -> Result<()> {
    let entries = index::load_entries(&resolve_index(args.index))
        .map_err(|e| anyhow!("reading index: {e}"))?;
    if entries.is_empty() {
        println!("index is empty; run an eval first");
        return Ok(());
    }
    let mut rollups = index::history(&entries);
    if let Some(limit) = args.limit {
        let start = rollups.len().saturating_sub(limit);
        rollups = rollups.split_off(start);
    }
    println!(
        "{:<28} {:>8} {:<8} suite/sha",
        "run", "resolved", "median_tc"
    );
    for r in rollups {
        println!(
            "{:<28} {:>3}/{:<3}  {:<8} {}/{}",
            r.run_id,
            r.resolved,
            r.total,
            r.median_tool_calls,
            r.suite.unwrap_or_else(|| "-".into()),
            r.git_sha
                .unwrap_or_else(|| "-".into())
                .chars()
                .take(8)
                .collect::<String>(),
        );
    }
    Ok(())
}

fn cmd_trend(args: TrendArgs) -> Result<()> {
    let entries = index::load_entries(&resolve_index(args.index))
        .map_err(|e| anyhow!("reading index: {e}"))?;
    let trends = index::trend(&entries, args.suite.as_deref());
    if trends.is_empty() {
        println!("no matching tasks in index");
        return Ok(());
    }
    println!("{:<32} {:>5} {:>9} flags", "task", "runs", "pass_rate");
    for t in trends {
        let mut flags = Vec::new();
        if t.chronic_failure {
            flags.push("chronic");
        }
        if t.high_variance {
            flags.push("high-variance");
        }
        println!(
            "{:<32} {:>5} {:>9.2} {}",
            t.task_id,
            t.runs,
            t.pass_rate,
            flags.join(",")
        );
    }
    Ok(())
}

fn cmd_regressions(args: RegressionsArgs) -> Result<()> {
    let entries = index::load_entries(&resolve_index(args.index))
        .map_err(|e| anyhow!("reading index: {e}"))?;
    let r = index::regressions(&entries, &args.baseline, &args.candidate);
    println!(
        "Resolved: {} -> {} ({:+})",
        r.baseline_resolved,
        r.candidate_resolved,
        r.candidate_resolved as i64 - r.baseline_resolved as i64
    );
    println!("New fixes: {}", r.newly_fixed.len());
    for id in &r.newly_fixed {
        println!("  + {id}");
    }
    println!("Regressions: {}", r.new_regressions.len());
    for id in &r.new_regressions {
        println!("  - {id}");
    }
    println!(
        "Median tool calls: {} -> {}",
        r.baseline_median_tool_calls, r.candidate_median_tool_calls
    );
    if !r.failure_class_shift.is_empty() {
        println!("Failure-class shift:");
        for (k, v) in &r.failure_class_shift {
            println!("  {k}: {v:+}");
        }
    }
    Ok(())
}

fn cmd_task_history(args: TaskHistoryArgs) -> Result<()> {
    let entries = index::load_entries(&resolve_index(args.index))
        .map_err(|e| anyhow!("reading index: {e}"))?;
    let hist = index::task_history(&entries, &args.task_id);
    if hist.is_empty() {
        println!("no history for task '{}'", args.task_id);
        return Ok(());
    }
    println!("{:<28} {:<9} {:<22} model", "run", "resolved", "failure");
    for e in hist {
        println!(
            "{:<28} {:<9} {:<22} {}",
            e.run_id,
            e.resolved,
            e.failure_class.map(|c| c.key()).unwrap_or("-"),
            e.model
        );
    }
    Ok(())
}

fn cmd_triage(args: TriageArgs) -> Result<()> {
    let run_dir = resolve_run_path(&args.run, args.runs_dir);
    let report = triage::triage(&run_dir)?;
    println!("{report}");
    Ok(())
}

fn cmd_show(args: ShowArgs, paths_only: bool) -> Result<()> {
    let run_dir = resolve_run_path(&args.run, args.runs_dir);
    if paths_only {
        let paths = triage::artifact_paths(&run_dir, &args.task_id);
        if paths.is_empty() {
            return Err(anyhow!(
                "no artifacts for task '{}' in {}",
                args.task_id,
                run_dir.display()
            ));
        }
        for p in paths {
            println!("{}", p.display());
        }
        return Ok(());
    }
    let out = triage::show(&run_dir, &args.task_id)?;
    println!("{out}");
    Ok(())
}

fn resolve_run_path(run: &str, runs_dir: Option<PathBuf>) -> PathBuf {
    let p = Path::new(run);
    if p.exists() {
        return p.to_path_buf();
    }
    resolve_runs_dir(runs_dir).join(run)
}

fn generate_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("run-{nanos}")
}

fn now_rfc3339() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
