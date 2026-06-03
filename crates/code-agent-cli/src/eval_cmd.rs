//! `code-agent eval` — the coding-agent capability evaluation harness.
//!
//! Subcommands:
//! - `eval list` — list discoverable tasks.
//! - `eval run` — run one task or a suite, writing an artifact bundle.
//! - `eval report` — print the summary from a prior run.
//! - `eval compare` — diff pass/fail/score/tool-count/runtime across two runs.
//!
//! Runs drive the agent through the M12 SDK runtime; provider selection is
//! `fake` (default, offline) / `golden` (scripted) / a live provider name such
//! as `anthropic` (opt-in via `CODE_AGENT_EVAL_LIVE=1`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};

use code_agent_eval::artifacts::RunDir;
use code_agent_eval::providers::ProviderMode;
use code_agent_eval::report;
use code_agent_eval::runner::{run_tasks, EvalStatus, RunConfig};
use code_agent_eval::task::{discover_tasks, EvalTask};
use code_agent_eval::RUNS_ROOT;

#[derive(Subcommand, Debug)]
pub enum EvalCommand {
    /// List available eval tasks (id, title, tags, suites).
    List(EvalListArgs),
    /// Run a single task or a suite.
    Run(EvalRunArgs),
    /// Print the summary from a prior run.
    Report(EvalReportArgs),
    /// Compare two prior runs.
    Compare(EvalCompareArgs),
}

#[derive(Parser, Debug)]
pub struct EvalListArgs {
    /// Directory containing tasks (defaults to the bundled eval/tasks).
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    /// Only list tasks in this suite.
    #[arg(long)]
    pub suite: Option<String>,
    /// Only list tasks with this tag.
    #[arg(long)]
    pub tag: Option<String>,
}

#[derive(Parser, Debug)]
pub struct EvalRunArgs {
    /// Run a single task by id. Omit to run a suite (see `--suite`).
    pub task_id: Option<String>,
    /// Run every task in this suite (default: `smoke`).
    #[arg(long)]
    pub suite: Option<String>,
    /// Provider mode: `fake` (default), `golden`, or a live provider name
    /// (e.g. `anthropic`, opt-in via CODE_AGENT_EVAL_LIVE=1).
    #[arg(long, default_value = "fake")]
    pub provider: String,
    /// Directory containing tasks (defaults to the bundled eval/tasks).
    #[arg(long)]
    pub tasks_dir: Option<PathBuf>,
    /// Root directory for run artifacts (default: target/code-agent-eval/runs).
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
    /// Do not exit non-zero when tasks fail (useful for live exploration).
    #[arg(long)]
    pub allow_failures: bool,
}

#[derive(Parser, Debug)]
pub struct EvalReportArgs {
    /// Run id or a path to a run directory / summary.json.
    pub run: String,
    /// Root directory for run artifacts (default: target/code-agent-eval/runs).
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

#[derive(Parser, Debug)]
pub struct EvalCompareArgs {
    /// Baseline run id or path.
    pub baseline: String,
    /// Candidate run id or path.
    pub candidate: String,
    /// Root directory for run artifacts (default: target/code-agent-eval/runs).
    #[arg(long)]
    pub runs_dir: Option<PathBuf>,
}

/// Dispatch an `eval` subcommand.
pub async fn run(cmd: EvalCommand) -> Result<()> {
    match cmd {
        EvalCommand::List(args) => cmd_list(args),
        EvalCommand::Run(args) => cmd_run(args).await,
        EvalCommand::Report(args) => cmd_report(args),
        EvalCommand::Compare(args) => cmd_compare(args),
    }
}

/// Resolve the tasks directory, trying explicit then well-known locations.
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

fn cmd_list(args: EvalListArgs) -> Result<()> {
    let dir = resolve_tasks_dir(args.tasks_dir)?;
    let tasks = discover_tasks(&dir).map_err(|e| anyhow!("{e}"))?;
    let filtered: Vec<&EvalTask> = tasks
        .iter()
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
        println!("{}", t.id);
        println!("    {}", t.title);
        if !t.tags.is_empty() {
            println!("    tags:   {}", t.tags.join(", "));
        }
        if !t.suites.is_empty() {
            println!("    suites: {}", t.suites.join(", "));
        }
    }
    Ok(())
}

/// Live provider names the harness knows how to drive. Anything else is almost
/// certainly a typo and must be a hard error rather than a silent skip.
const KNOWN_LIVE_PROVIDERS: &[&str] = &["anthropic"];

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
    let dir = resolve_tasks_dir(args.tasks_dir)?;
    let all = discover_tasks(&dir).map_err(|e| anyhow!("{e}"))?;

    let selected: Vec<EvalTask> = if let Some(id) = &args.task_id {
        let task = all
            .iter()
            .find(|t| &t.id == id)
            .ok_or_else(|| anyhow!("no task with id '{id}' in {}", dir.display()))?;
        vec![task.clone()]
    } else {
        let suite = args.suite.as_deref().unwrap_or("smoke");
        let tasks: Vec<EvalTask> = all.iter().filter(|t| t.in_suite(suite)).cloned().collect();
        if tasks.is_empty() {
            return Err(anyhow!(
                "suite '{suite}' matched no tasks in {}",
                dir.display()
            ));
        }
        tasks
    };

    let runs_dir = resolve_runs_dir(args.runs_dir);
    let run_id = generate_run_id();
    let run = RunDir::create(&runs_dir, &run_id)
        .with_context(|| format!("creating run dir under {}", runs_dir.display()))?;

    println!(
        "running {} task(s) with provider '{}' → {}",
        selected.len(),
        provider.label(),
        run.root.display()
    );

    let config = RunConfig {
        provider: provider.clone(),
        timeout_grace: Duration::from_secs(30),
    };
    let results = run_tasks(&selected, &run, &config).await;
    let summary = report::write_summaries(&run, &provider.label(), &results)
        .map_err(|e| anyhow!("writing summaries: {e}"))?;

    // Per-task line output.
    println!();
    for r in &results {
        println!(
            "  [{}] {:<28} score {:.3}  ({}/{} checks, {} tools)",
            r.status.symbol(),
            r.task_id,
            r.score.total,
            r.checks_passed,
            r.checks_total,
            r.tool_calls
        );
        if let Some(reason) = &r.skipped_reason {
            println!("           skipped: {reason}");
        }
    }
    println!(
        "\n{} passed, {} partial, {} failed, {} errored, {} timed out, {} skipped (mean score {:.3})",
        summary.passed,
        summary.partial,
        summary.failed,
        summary.errored,
        summary.timed_out,
        summary.skipped,
        summary.mean_score
    );
    println!("artifacts: {}", run.root.display());

    // Gate: non-pass, non-skip tasks fail the command unless allowed.
    let failures = results
        .iter()
        .filter(|r| !matches!(r.status, EvalStatus::Passed | EvalStatus::Skipped))
        .count();
    if failures > 0 && !args.allow_failures {
        return Err(anyhow!(
            "{failures} task(s) did not pass (use --allow-failures to ignore)"
        ));
    }
    Ok(())
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
    // Fall back to rendering from summary.json.
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

/// Resolve a run reference (id or path) to a directory or summary.json path.
fn resolve_run_path(run: &str, runs_dir: Option<PathBuf>) -> PathBuf {
    let p = Path::new(run);
    if p.exists() {
        return p.to_path_buf();
    }
    resolve_runs_dir(runs_dir).join(run)
}

/// Generate a sortable, unique run id.
fn generate_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("run-{nanos}")
}
