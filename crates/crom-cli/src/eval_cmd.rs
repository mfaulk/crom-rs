//! `crom eval` — the coding-agent capability evaluation harness.
//!
//! M14 subcommands: `list`, `run`, `report`, `compare`.
//! M15 subcommands: `adapters`, `validate-task`, `validate-suite`, `history`,
//! `trend`, `regressions`, `task-history`, `triage`, `show`, `open`, plus an
//! adapter-, isolation-, repetition-, and prompt-variant-aware `run`.
//! M17.4: `scoreboard`, paired `compare`, and `regressions --since`.
//!
//! Runs drive the agent through the SDK runtime; provider selection is `fake`
//! (default, offline) / `golden` (scripted) / a provider profile id such as
//! `anthropic`, `openai`, `deepseek`, or `gemini` (opt-in via
//! `CROM_EVAL_LIVE=1`).
//!
//! `eval run` exit codes: 0 ok, [`EXIT_TASKS_FAILED`] when a task did not
//! resolve, [`EXIT_ALL_SKIPPED`] when every task was skipped (e.g. missing
//! credentials), so CI can tell "nothing ran" from "ran and failed".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use crom_eval::adapter::AdapterRegistry;
use crom_eval::artifacts::RunDir;
use crom_eval::index::{self, GitContext, IndexEntry};
use crom_eval::isolation::IsolationKind;
use crom_eval::multirun;
use crom_eval::providers::ProviderMode;
use crom_eval::report;
use crom_eval::runner::{run_tasks_repeated, EvalResult, EvalStatus, RunConfig};
use crom_eval::task::{discover_tasks, EvalTask};
use crom_eval::{triage, validate, RUNS_ROOT};

// `Run(EvalRunArgs)` is much larger than the unit variants, but clap's
// `Subcommand` derive cannot flatten a boxed variant, and these args are
// constructed exactly once at startup — the size disparity is irrelevant here.
#[allow(clippy::large_enum_variant)]
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
    /// Paired comparison of two runs with comparability checks and uncertainty.
    Compare(EvalCompareArgs),
    /// Show per-run rollups from the result index.
    History(HistoryArgs),
    /// Show per-task pass-rate trend for a suite.
    Trend(TrendArgs),
    /// Paired regression check against a ref (`--since main`), or an
    /// index-only diff of two runs (`--baseline A --candidate B`).
    Regressions(RegressionsArgs),
    /// Show the history of a single task across runs.
    TaskHistory(TaskHistoryArgs),
    /// Group a run's failures by failure class.
    Triage(TriageArgs),
    /// Show a debuggable view of one task in a run.
    Show(ShowArgs),
    /// List artifact paths for one task in a run.
    Open(ShowArgs),
    /// Cross-run scoreboard: capability, efficiency, reliability, security.
    Scoreboard(ScoreboardArgs),
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
    /// Provider mode: `fake` (default), `golden`, or a live provider-profile
    /// id (`anthropic`, `openai`, `deepseek`, `gemini`, `local`, or one
    /// defined under settings `providers`). Live runs need `CROM_EVAL_LIVE=1`
    /// and the profile's API-key env var; otherwise tasks are skipped.
    #[arg(long, default_value = "fake")]
    pub provider: String,
    /// Model for a live provider. Defaults to `CROM_EVAL_MODEL`, then the
    /// profile's `default_model`. Rejected for offline providers.
    #[arg(long)]
    pub model: Option<String>,
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
    /// OS sandbox for agent tool processes: `required` or `off`. Defaults to
    /// `required` for live providers and `off` for fake/golden. `required`
    /// fails closed when the host cannot enforce the sandbox.
    #[arg(long)]
    pub sandbox: Option<crom_core::SandboxMode>,
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
    /// Base seed for prompt-variant selection; repetition `n` uses `seed + n`.
    /// Recorded in `run.json` and each result's `repro` block.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
    /// Per-task nominal cost cap in USD (all attempts); the tighter of this
    /// and a task's `limits.max_cost_usd` applies. Breach fails the task as
    /// `budget_exceeded`.
    #[arg(long, value_parser = parse_usd)]
    pub max_cost_usd: Option<f64>,
    /// Run-level nominal spend cap in USD. Once reached, remaining tasks are
    /// recorded as skipped with a `budget:` reason.
    #[arg(long, value_parser = parse_usd)]
    pub max_run_cost_usd: Option<f64>,
    /// Pricing override file merged over the built-in table (also read from
    /// `CROM_PRICING_FILE`). See docs/providers.md#cost-semantics.
    #[arg(long)]
    pub pricing_file: Option<PathBuf>,
    /// M16: repair loop — `enabled` or `disabled` (default `disabled`). When
    /// enabled, failed validations are parsed into structured diagnostics and a
    /// targeted repair brief drives bounded retries.
    #[arg(long, default_value = "disabled")]
    pub repair: String,
    /// M16: override the repair loop's max attempts (implies `--repair enabled`).
    #[arg(long)]
    pub repair_max_attempts: Option<u32>,
    /// M16: require a validation command after the last mutation (records the
    /// `final_answer_without_validation` warning otherwise).
    #[arg(long)]
    pub require_validation_after_edit: bool,
    /// M16: diagnostic excerpt format hint for artifacts (`text` or `json`).
    #[arg(long, default_value = "text")]
    pub diagnostic_format: String,
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

/// Where `validate-task` / `validate-suite` run verification commands.
#[derive(clap::Args, Debug, Clone)]
pub struct ValidateIsolationArgs {
    /// Verification backend: local | docker | podman (containers are opt-in
    /// via CROM_EVAL_DOCKER=1 / CROM_EVAL_PODMAN=1).
    #[arg(long, default_value = "local")]
    pub isolation: String,
    /// Container image (container backends only).
    #[arg(long)]
    pub container_image: Option<String>,
}

impl ValidateIsolationArgs {
    fn options(&self) -> Result<validate::ValidationOptions> {
        Ok(validate::ValidationOptions {
            isolation: IsolationKind::parse(&self.isolation).map_err(|e| anyhow!("{e}"))?,
            container_image: self.container_image.clone(),
        })
    }
}

#[derive(Parser, Debug)]
pub struct ValidateTaskArgs {
    /// A task id (internal) or a path to a task directory / task.toml.
    pub target: String,
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    #[command(flatten)]
    pub isolation_args: ValidateIsolationArgs,
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
    #[command(flatten)]
    pub isolation_args: ValidateIsolationArgs,
}

#[derive(Parser, Debug)]
pub struct EvalReportArgs {
    pub run: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct EvalCompareArgs {
    /// Baseline run id or run directory path.
    pub baseline: String,
    /// Candidate run id or run directory path.
    pub candidate: String,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
    #[command(flatten)]
    pub paired: PairedArgs,
}

/// Options shared by `compare` and `regressions --since`.
#[derive(clap::Args, Debug, Clone)]
pub struct PairedArgs {
    /// Experiment preset naming the factor allowed to differ:
    /// commit | model | security | repair | policy | reps.
    #[arg(long)]
    pub experiment: Option<crom_eval::compare::Experiment>,
    /// Additional factor allowed to differ (repeatable): model, task_version,
    /// tool_policy, security, isolation, budgets, repair, prompt, metadata.
    #[arg(long)]
    pub allow: Vec<crom_eval::compare::Factor>,
    /// Compare even when disallowed factors differ (prints a warning).
    #[arg(long)]
    pub force: bool,
    /// Task kinds to pair: capability (default) | harness | security | all.
    #[arg(long, default_value = "capability")]
    pub kind: crom_eval::compare::KindScope,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: CompareFormat,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareFormat {
    Text,
    Json,
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
#[command(group(clap::ArgGroup::new("base").required(true).args(["since", "baseline"])))]
pub struct RegressionsArgs {
    /// Paired check: baseline = the latest indexed run at this ref's commit
    /// (else the latest run recorded on the branch of that name).
    #[arg(long)]
    pub since: Option<String>,
    /// Index-only diff: baseline run id or SHA prefix (needs --candidate).
    #[arg(long, requires = "candidate")]
    pub baseline: Option<String>,
    /// Candidate run id (default with --since: the latest other indexed run).
    #[arg(long)]
    pub candidate: Option<String>,
    #[arg(long)]
    pub index: Option<PathBuf>,
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
    #[command(flatten)]
    pub paired: PairedArgs,
}

#[derive(Parser, Debug)]
pub struct TaskHistoryArgs {
    pub task_id: String,
    #[arg(long)]
    pub index: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct ScoreboardArgs {
    /// Root directory holding run directories (default `target/crom-eval/runs`).
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
    /// Only these run ids (repeatable; default: every run under --runs-dir).
    #[arg(long)]
    pub run: Vec<String>,
    /// Only results whose evaluating commit is `<rev>` or reachable from HEAD
    /// but not from `<rev>` (`git rev-list <rev>..HEAD`).
    #[arg(long)]
    pub since: Option<String>,
    /// Only this model, as `model` or `provider/model` (repeatable).
    #[arg(long)]
    pub model: Vec<String>,
    /// Only tasks at this level, L0..L5 (repeatable).
    #[arg(long, value_parser = parse_level)]
    pub level: Vec<crom_eval::schema::Level>,
    /// Column grouping: `model` or `commit`.
    #[arg(long, default_value = "model")]
    pub group_by: crom_eval::scoreboard::GroupBy,
    /// Output format: `text`, `md`, or `json`.
    #[arg(long, default_value = "text")]
    pub format: crom_eval::scoreboard::Format,
}

fn parse_level(s: &str) -> std::result::Result<crom_eval::schema::Level, String> {
    serde_json::from_value(serde_json::Value::String(s.to_ascii_uppercase()))
        .map_err(|_| format!("unknown level '{s}' (L0..L5)"))
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
        EvalCommand::Scoreboard(args) => cmd_scoreboard(args),
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
        PathBuf::from("crates/crom-eval/eval/tasks"),
        PathBuf::from("eval/tasks"),
        crom_eval::fixture::default_tasks_dir(),
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

/// Settings-level provider-profile overrides for live eval runs, loaded
/// from user + project settings rooted at the current directory.
fn eval_provider_profiles(
) -> Result<std::collections::BTreeMap<String, crom_core::ProviderProfileConfig>> {
    let cwd = std::env::current_dir().context("current dir")?;
    let settings = crom_core::settings::load(&cwd, crom_core::SettingSource::UserAndProject)
        .context("load settings for provider profiles")?;
    Ok(settings.providers)
}

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

/// Build the repair policy from the eval run flags. `--repair enabled|disabled`,
/// with `--repair-max-attempts` and `--require-validation-after-edit` implying
/// `enabled`.
fn build_repair_policy(args: &EvalRunArgs) -> Result<crom_core::repair::RepairPolicy> {
    use crom_core::repair::RepairPolicy;
    let explicit_enable = args.repair_max_attempts.is_some() || args.require_validation_after_edit;
    let enabled = match args.repair.to_ascii_lowercase().as_str() {
        "enabled" | "on" | "true" => true,
        "disabled" | "off" | "false" => explicit_enable,
        other => {
            return Err(anyhow!(
                "invalid --repair '{other}' (expected enabled|disabled)"
            ))
        }
    };
    let mut policy = if enabled {
        RepairPolicy::default()
    } else {
        RepairPolicy::disabled()
    };
    if let Some(n) = args.repair_max_attempts {
        policy.max_attempts = n.max(1);
    }
    if args.require_validation_after_edit {
        policy.require_validation_after_mutation = true;
    }
    Ok(policy)
}

fn parse_usd(s: &str) -> std::result::Result<f64, String> {
    let v: f64 = s.parse().map_err(|e| format!("{e}"))?;
    if v.is_finite() && v >= 0.0 {
        Ok(v)
    } else {
        Err("must be a finite amount >= 0".to_string())
    }
}

/// Built-in pricing, then `CROM_PRICING_FILE`, then `--pricing-file`.
fn eval_pricing(args: &EvalRunArgs) -> Result<crom_core::PricingTable> {
    let table = crom_core::PricingTable::load_default().map_err(|e| anyhow!("{e}"))?;
    match &args.pricing_file {
        Some(p) => {
            Ok(table
                .with_override(crom_core::PricingTable::load_file(p).map_err(|e| anyhow!("{e}"))?))
        }
        None => Ok(table),
    }
}

/// `eval run` exit code when at least one task did not resolve.
pub const EXIT_TASKS_FAILED: u8 = 1;
/// `eval run` exit code when every task was skipped (nothing ran).
pub const EXIT_ALL_SKIPPED: u8 = 3;

/// An error that maps to a specific process exit code.
#[derive(Debug)]
pub struct EvalExit {
    pub code: u8,
    pub message: String,
}

impl std::fmt::Display for EvalExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EvalExit {}

#[derive(Debug, PartialEq, Eq)]
enum RunVerdict {
    Ok,
    /// Non-resolved, non-skipped, non-quarantined result count.
    Failed(usize),
    /// Every result was skipped; holds the count.
    AllSkipped(usize),
}

/// Gate an `eval run`. Each item is `(status, resolved, quarantined)`.
/// Failures win over skips; `allow_failures` does not hide an all-skipped run.
fn run_verdict(results: &[(EvalStatus, bool, bool)], allow_failures: bool) -> RunVerdict {
    let skipped = |s: &EvalStatus| matches!(s, EvalStatus::Skipped);
    let failures = results
        .iter()
        .filter(|(s, resolved, quarantined)| !resolved && !quarantined && !skipped(s))
        .count();
    if failures > 0 && !allow_failures {
        return RunVerdict::Failed(failures);
    }
    if !results.is_empty() && results.iter().all(|(s, _, _)| skipped(s)) {
        return RunVerdict::AllSkipped(results.len());
    }
    RunVerdict::Ok
}

/// `--model` for a live provider; an error for offline providers, whose
/// model is fixed.
fn model_override(args: &EvalRunArgs, provider: &ProviderMode) -> Result<Option<String>> {
    match (&args.model, provider.is_live()) {
        (Some(m), false) => Err(anyhow!(
            "--model {m} applies only to live providers; '{}' is offline",
            provider.label()
        )),
        (m, _) => Ok(m.clone()),
    }
}

async fn cmd_run(args: EvalRunArgs) -> Result<()> {
    let provider = ProviderMode::parse(&args.provider);
    let model = model_override(&args, &provider)?;
    let provider_profiles = if let ProviderMode::Live(name) = &provider {
        let profiles = eval_provider_profiles()?;
        crom_eval::providers::resolve_live_profile(name, &profiles).map_err(|u| {
            anyhow!(
                "unknown provider '{name}'; expected fake, golden, or a provider profile: {}",
                u.reason
            )
        })?;
        profiles
    } else {
        Default::default()
    };
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

    let repair = build_repair_policy(&args)?;
    let sandbox = resolve_eval_sandbox(args.sandbox, &provider);
    let sandbox_overhead_ms = prepare_eval_sandbox(sandbox, args.network).await?;
    let pricing = eval_pricing(&args)?;

    let config = RunConfig {
        provider: provider.clone(),
        model,
        timeout_grace: Duration::from_secs(30),
        adapter: args.adapter.clone(),
        isolation: isolation.clone(),
        container_image: args.container_image.clone(),
        network: args.network,
        cpu_limit: args.cpu_limit,
        memory_limit: args.memory_limit,
        prompt_variant: args.prompt_variant.clone(),
        repetition: 0,
        seed: args.seed,
        attempts: args.attempts.max(1),
        repair,
        provider_profiles,
        environment: Some(crom_eval::result_schema::RunEnvironment::gather()),
        workspace_root: None,
        sandbox,
        sandbox_overhead_ms,
        pricing,
        max_cost_usd: args.max_cost_usd,
        max_run_cost_usd: args.max_run_cost_usd,
        run_cost_remaining: None,
    };

    let results = run_tasks_repeated(&selected, &run, &config, repetitions).await;
    let summary = report::write_summaries(&run, &provider.label(), &results)
        .map_err(|e| anyhow!("writing summaries: {e}"))?;

    // Multi-run statistics.
    if repetitions > 1 {
        let stats = multirun::aggregate(&results, repetitions);
        let _ = write_artifact(
            run.root.join("repetitions.json"),
            serde_json::to_string_pretty(&stats).unwrap_or_default(),
        );
        println!("\n{}", multirun::to_markdown(&stats));
    }

    write_behavior_summary(&run.root, &results);
    let _ = write_artifact(
        run.root.join("run.json"),
        serde_json::to_string_pretty(&crom_eval::runner::run_metadata(&config, repetitions))
            .unwrap_or_default(),
    );

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
                let _ = write_artifact(
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
    match summary.total_cost_usd {
        Some(c) => println!("nominal cost: ${c:.4}"),
        None => println!(
            "nominal cost: unknown ({} result(s) without a cost)",
            summary.cost_unknown
        ),
    }
    if summary.budget_skipped > 0 {
        println!(
            "spend cap: {} task(s) skipped (budget); see skipped_reason",
            summary.budget_skipped
        );
    }
    println!("artifacts: {}", run.root.display());

    let gate: Vec<(EvalStatus, bool, bool)> = results
        .iter()
        .map(|r| (r.status, r.resolved, r.quarantined))
        .collect();
    match run_verdict(&gate, args.allow_failures) {
        RunVerdict::Ok => Ok(()),
        RunVerdict::Failed(n) => Err(EvalExit {
            code: EXIT_TASKS_FAILED,
            message: format!("{n} task result(s) did not resolve (use --allow-failures to ignore)"),
        }
        .into()),
        RunVerdict::AllSkipped(n) => Err(EvalExit {
            code: EXIT_ALL_SKIPPED,
            message: format!(
                "all {n} task result(s) were skipped; nothing ran ({})",
                results
                    .iter()
                    .find_map(|r| r.skipped_reason.as_deref())
                    .unwrap_or("no reason recorded")
            ),
        }
        .into()),
    }
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
    let _ = write_artifact(
        run_root.join("behavior-summary.json"),
        serde_json::to_string_pretty(&summary).unwrap_or_default(),
    );
}

/// Run-level eval files go through the same secret redaction as task artifacts.
fn write_artifact(path: std::path::PathBuf, content: String) -> std::io::Result<()> {
    crom_eval::artifacts::write_redacted(&path, &content)
}

/// Agent-tool sandbox mode for a run: the explicit flag, else `required`
/// for live providers and `off` for the offline fake/golden providers.
fn resolve_eval_sandbox(
    flag: Option<crom_core::SandboxMode>,
    provider: &ProviderMode,
) -> crom_core::SandboxMode {
    flag.unwrap_or(if provider.is_live() {
        crom_core::SandboxMode::Required
    } else {
        crom_core::SandboxMode::Off
    })
}

/// Fail closed before any task runs when `required` cannot be enforced, then
/// measure the mean per-call overhead recorded in results.
async fn prepare_eval_sandbox(mode: crom_core::SandboxMode, network: bool) -> Result<Option<f64>> {
    if mode == crom_core::SandboxMode::Off {
        println!("agent sandbox: off (agent tools run unsandboxed)");
        return Ok(None);
    }
    let cfg = crom_core::SandboxConfig {
        network,
        ..crom_core::SandboxConfig::required()
    };
    let info = crom_tools::sandbox::probe(&cfg).map_err(|reason| {
        anyhow!(
            "agent sandbox is required but cannot be enforced on this host: {reason}; \
             pass --sandbox off to run agent tools unsandboxed"
        )
    })?;
    let scratch = tempfile::TempDir::new().context("sandbox overhead scratch dir")?;
    let overhead = crom_tools::sandbox::measure_overhead_ms(&cfg, scratch.path(), 20)
        .await
        .map_err(|e| anyhow!("measuring sandbox overhead: {e}"))?;
    println!(
        "agent sandbox: required ({} ABI {}), overhead {overhead:.2} ms/call",
        info.backend, info.landlock_abi
    );
    Ok(Some(overhead))
}

async fn cmd_validate_task(args: ValidateTaskArgs) -> Result<()> {
    let task = resolve_single_task(&args.target, args.tasks_dir)?;
    let report = validate::validate_task_with(&task, &args.isolation_args.options()?).await;
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
    let opts = args.isolation_args.options()?;
    let mut total_errors = 0usize;
    for task in tasks {
        let report = validate::validate_task_with(task, &opts).await;
        total_errors += report.errors();
        println!("{}", report.to_markdown());
        if args.repeat > 1 {
            let verdict = validate::flake_detect_with(task, args.repeat, &opts).await;
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
            return crom_eval::adapter::DirectoryAdapter::load_one(p);
        }
        // A task.toml file.
        return crom_eval::task::load_task(p).map_err(|e| anyhow!("{e}"));
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
    paired_compare(&base_path, &cand_path, &args.paired)
}

/// Load every attempt of the run at `path` (including repetitions).
fn load_run_attempts(path: &Path) -> Result<(String, Vec<crom_eval::scoreboard::Attempt>)> {
    let run_id = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| anyhow!("bad run path {}", path.display()))?;
    let parent = path.parent().unwrap_or(Path::new("."));
    let (attempts, warnings) =
        crom_eval::scoreboard::load_attempts(parent, std::slice::from_ref(&run_id))
            .with_context(|| format!("reading {}", path.display()))?;
    for w in &warnings {
        eprintln!("warning: skipping unreadable result {w}");
    }
    if attempts.is_empty() {
        return Err(anyhow!("no result.json found under {}", path.display()));
    }
    Ok((run_id, attempts))
}

fn paired_compare(base_path: &Path, cand_path: &Path, args: &PairedArgs) -> Result<()> {
    use crom_eval::compare::{compare_runs, CompareOptions};
    let (base_id, base) = load_run_attempts(base_path)?;
    let (cand_id, cand) = load_run_attempts(cand_path)?;
    let cmp = compare_runs(
        &base_id,
        &base,
        &cand_id,
        &cand,
        &CompareOptions {
            kind: args.kind,
            experiment: args.experiment,
            allow: args.allow.clone(),
            force: args.force,
        },
    );
    let blocking = cmp.blocking();
    if !blocking.is_empty() && !args.force {
        eprintln!("Runs are not comparable; differing factors:");
        for m in &blocking {
            eprintln!(
                "  {} ({} task(s), e.g. {}): {} != {}",
                m.factor.as_str(),
                m.tasks,
                m.example_task,
                m.baseline,
                m.candidate
            );
        }
        return Err(anyhow!(
            "refusing to compare: pass --experiment <preset> or --allow <factor> if the difference is intended, or --force"
        ));
    }
    match args.format {
        CompareFormat::Text => print!("{}", cmp.to_text()),
        CompareFormat::Json => print!("{}", cmp.to_json()),
    }
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

fn cmd_scoreboard(args: ScoreboardArgs) -> Result<()> {
    use crom_eval::scoreboard::{self, Filters, Scoreboard};
    let runs_dir = resolve_runs_dir(args.runs_dir);
    let (attempts, warnings) = scoreboard::load_attempts(&runs_dir, &args.run)
        .with_context(|| format!("reading runs under {}", runs_dir.display()))?;
    for w in &warnings {
        eprintln!("warning: skipping unreadable result {w}");
    }
    let since_shas = match &args.since {
        Some(rev) => Some(scoreboard::commits_since(rev)?),
        None => None,
    };
    let filters = Filters {
        since: args.since,
        since_shas,
        models: args.model,
        levels: args.level,
    };
    let board = Scoreboard::build(&attempts, &filters, args.group_by);
    print!("{}", board.render(args.format));
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
    if let Some(reference) = &args.since {
        use crom_eval::compare::{latest_run_except, resolve_baseline_run};
        let sha = std::process::Command::new("git")
            .args(["rev-parse", "--verify", &format!("{reference}^{{commit}}")])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let baseline = resolve_baseline_run(&entries, sha.as_deref(), reference)
            .ok_or_else(|| anyhow!("no indexed run at '{reference}' (commit or branch)"))?;
        let candidate = match args.candidate {
            Some(c) => c,
            None => latest_run_except(&entries, &baseline)
                .ok_or_else(|| anyhow!("no candidate run in the index besides {baseline}"))?,
        };
        eprintln!("baseline: {baseline} ({reference}); candidate: {candidate}");
        let base_path = resolve_run_path(&baseline, args.runs_dir.clone());
        let cand_path = resolve_run_path(&candidate, args.runs_dir);
        return paired_compare(&base_path, &cand_path, &args.paired);
    }
    // `--baseline` requires `--candidate` (enforced by clap).
    let (Some(baseline), Some(candidate)) = (args.baseline, args.candidate) else {
        return Err(anyhow!("--baseline needs --candidate"));
    };
    let r = index::regressions(&entries, &baseline, &candidate);
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

#[cfg(test)]
mod sandbox_flag_tests {
    use super::*;
    use crom_core::SandboxMode;
    use crom_eval::providers::ProviderMode;

    // Live providers default to a required agent sandbox; offline providers default to off.
    #[test]
    fn sandbox_defaults_follow_provider() {
        assert_eq!(
            resolve_eval_sandbox(None, &ProviderMode::Live("anthropic".into())),
            SandboxMode::Required
        );
        assert_eq!(
            resolve_eval_sandbox(None, &ProviderMode::Fake),
            SandboxMode::Off
        );
        assert_eq!(
            resolve_eval_sandbox(None, &ProviderMode::Golden),
            SandboxMode::Off
        );
    }

    // An explicit --sandbox flag wins over the provider default in both directions.
    #[test]
    fn sandbox_flag_overrides_default() {
        assert_eq!(
            resolve_eval_sandbox(Some(SandboxMode::Off), &ProviderMode::Live("x".into())),
            SandboxMode::Off
        );
        assert_eq!(
            resolve_eval_sandbox(Some(SandboxMode::Required), &ProviderMode::Fake),
            SandboxMode::Required
        );
    }

    // `crom eval run --sandbox` parses required|off and rejects anything else.
    #[test]
    fn sandbox_flag_parses() {
        let args = EvalRunArgs::try_parse_from(["run", "--sandbox", "required"]).unwrap();
        assert_eq!(args.sandbox, Some(SandboxMode::Required));
        let args = EvalRunArgs::try_parse_from(["run"]).unwrap();
        assert_eq!(args.sandbox, None);
        assert!(EvalRunArgs::try_parse_from(["run", "--sandbox", "maybe"]).is_err());
    }
}

#[cfg(test)]
mod validate_isolation_flag_tests {
    use super::*;

    // validate-task and validate-suite accept --isolation and --container-image and map them to ValidationOptions.
    #[test]
    fn validate_flags_map_to_options() {
        let a = ValidateTaskArgs::try_parse_from([
            "validate-task",
            "l3-x",
            "--isolation",
            "podman",
            "--container-image",
            "rust:1.88",
        ])
        .unwrap();
        let o = a.isolation_args.options().unwrap();
        assert_eq!(o.isolation, IsolationKind::Podman);
        assert_eq!(o.container_image.as_deref(), Some("rust:1.88"));

        let s = ValidateSuiteArgs::try_parse_from(["validate-suite", "l3"]).unwrap();
        let o = s.isolation_args.options().unwrap();
        assert_eq!(o.isolation, IsolationKind::Local);
        assert_eq!(o.container_image, None);

        let bad = ValidateSuiteArgs::try_parse_from(["validate-suite", "l3", "--isolation", "vm"])
            .unwrap();
        assert!(bad.isolation_args.options().is_err());
    }
}

#[cfg(test)]
mod live_provider_flag_tests {
    use super::*;
    use crom_eval::providers::ProviderMode;

    // `--model` parses and is passed through for live providers.
    #[test]
    fn model_flag_applies_to_live_providers() {
        let args = EvalRunArgs::try_parse_from([
            "run",
            "--provider",
            "deepseek",
            "--model",
            "deepseek-chat",
        ])
        .unwrap();
        let mode = ProviderMode::parse(&args.provider);
        assert_eq!(
            model_override(&args, &mode).unwrap().as_deref(),
            Some("deepseek-chat")
        );
        let args = EvalRunArgs::try_parse_from(["run", "--provider", "openai"]).unwrap();
        assert_eq!(
            model_override(&args, &ProviderMode::parse("openai")).unwrap(),
            None
        );
    }

    // `--model` with an offline provider is rejected instead of silently ignored.
    #[test]
    fn model_flag_rejected_for_offline_providers() {
        let args = EvalRunArgs::try_parse_from(["run", "--model", "gpt-4.1"]).unwrap();
        assert!(model_override(&args, &ProviderMode::Fake).is_err());
    }

    // Failures exit 1, an all-skipped run exits 3, and a run with anything resolved exits 0.
    #[test]
    fn verdict_distinguishes_skipped_from_failed() {
        use EvalStatus::*;
        assert_eq!(run_verdict(&[(Passed, true, false)], false), RunVerdict::Ok);
        assert_eq!(
            run_verdict(&[(Skipped, false, false), (Skipped, false, false)], false),
            RunVerdict::AllSkipped(2)
        );
        assert_eq!(
            run_verdict(&[(Skipped, false, false), (Failed, false, false)], false),
            RunVerdict::Failed(1)
        );
        assert_eq!(
            run_verdict(&[(Skipped, false, false), (Passed, true, false)], false),
            RunVerdict::Ok
        );
        // --allow-failures ignores failures but not an all-skipped run.
        assert_eq!(run_verdict(&[(Failed, false, false)], true), RunVerdict::Ok);
        assert_eq!(
            run_verdict(&[(Skipped, false, false)], true),
            RunVerdict::AllSkipped(1)
        );
        // Quarantined failures do not gate.
        assert_eq!(run_verdict(&[(Failed, false, true)], false), RunVerdict::Ok);
        assert_eq!(EXIT_TASKS_FAILED, 1);
        assert_eq!(EXIT_ALL_SKIPPED, 3);
    }
}

#[cfg(test)]
mod scoreboard_flag_tests {
    use super::*;
    use crom_eval::schema::Level;
    use crom_eval::scoreboard::{Format, GroupBy};

    // scoreboard parses repeatable --model/--level/--run filters, --since, --group-by, and --format.
    #[test]
    fn scoreboard_flags_parse() {
        let a = ScoreboardArgs::try_parse_from([
            "scoreboard",
            "--since",
            "main",
            "--model",
            "deepseek-chat",
            "--model",
            "golden",
            "--level",
            "L1",
            "--level",
            "l2",
            "--run",
            "r1",
            "--group-by",
            "commit",
            "--format",
            "md",
        ])
        .unwrap();
        assert_eq!(a.since.as_deref(), Some("main"));
        assert_eq!(a.model, ["deepseek-chat", "golden"]);
        assert_eq!(a.level, [Level::L1, Level::L2]);
        assert_eq!(a.run, ["r1"]);
        assert_eq!(a.group_by, GroupBy::Commit);
        assert_eq!(a.format, Format::Markdown);

        let d = ScoreboardArgs::try_parse_from(["scoreboard"]).unwrap();
        assert_eq!((d.group_by, d.format), (GroupBy::Model, Format::Text));
        assert!(ScoreboardArgs::try_parse_from(["scoreboard", "--level", "L9"]).is_err());
        assert!(ScoreboardArgs::try_parse_from(["scoreboard", "--format", "csv"]).is_err());
    }
}

#[cfg(test)]
mod compare_flag_tests {
    use super::*;
    use crom_eval::compare::{Experiment, Factor, KindScope};

    // compare parses the experiment preset, repeatable --allow factors, --force, --kind, and --format.
    #[test]
    fn compare_flags_parse() {
        let a = EvalCompareArgs::try_parse_from([
            "compare",
            "base",
            "cand",
            "--experiment",
            "model",
            "--allow",
            "budgets",
            "--allow",
            "prompt",
            "--force",
            "--kind",
            "all",
            "--format",
            "json",
        ])
        .unwrap();
        assert_eq!(
            (a.baseline.as_str(), a.candidate.as_str()),
            ("base", "cand")
        );
        assert_eq!(a.paired.experiment, Some(Experiment::Model));
        assert_eq!(a.paired.allow, [Factor::Budgets, Factor::Prompt]);
        assert!(a.paired.force);
        assert_eq!(a.paired.kind, KindScope::All);
        assert_eq!(a.paired.format, CompareFormat::Json);
        assert!(
            EvalCompareArgs::try_parse_from(["compare", "a", "b", "--experiment", "x"]).is_err()
        );
    }

    // regressions takes either --since <ref> (paired, baseline from the index) or --baseline with --candidate.
    #[test]
    fn regressions_since_or_baseline() {
        let s = RegressionsArgs::try_parse_from(["regressions", "--since", "main"]).unwrap();
        assert_eq!(s.since.as_deref(), Some("main"));
        assert_eq!(s.candidate, None);
        let old =
            RegressionsArgs::try_parse_from(["regressions", "--baseline", "a", "--candidate", "b"])
                .unwrap();
        assert_eq!(old.baseline.as_deref(), Some("a"));
        assert!(RegressionsArgs::try_parse_from(["regressions"]).is_err());
        assert!(RegressionsArgs::try_parse_from([
            "regressions",
            "--since",
            "main",
            "--baseline",
            "a"
        ])
        .is_err());
    }
}
