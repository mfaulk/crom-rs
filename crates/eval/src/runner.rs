//! Run a single task or a selection of tasks end-to-end.
//!
//! The runner drives the agent through the headless [`code_agent_sdk`] runtime
//! in an isolated, git-backed workspace, captures the structured event stream,
//! evaluates the oracles, scores the result, and writes the full artifact
//! bundle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use code_agent_sdk::policy::BuiltinToolSet;
use code_agent_sdk::{
    AgentMode, AgentRequest, AgentRuntime, AllowAllApprovals, ApprovalPolicy, EventSink,
    InteractivityMode, PatchPolicy, PlanPolicy, SdkEvent, SideEffectPolicy, ToolPolicy,
    WorkspaceSource,
};

use crate::artifacts::{RunDir, TaskArtifacts};
use crate::fixture::{generate_patch, prepare_workspace, PatchInfo};
use crate::oracle::{self, OracleReport};
use crate::providers::{self, ProviderMode};
use crate::score::{self, EvalScore, ScoreInputs};
use crate::task::{EvalTask, TaskMode};

/// Outcome status for a single evaluated task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    /// All oracle checks passed.
    Passed,
    /// Some but not all oracle checks passed.
    Partial,
    /// No oracle checks passed.
    Failed,
    /// The run exceeded its time budget.
    TimedOut,
    /// A harness or runtime error prevented evaluation.
    Errored,
    /// The provider was unavailable (e.g. missing live credentials).
    Skipped,
}

impl EvalStatus {
    pub fn symbol(&self) -> &'static str {
        match self {
            EvalStatus::Passed => "PASS",
            EvalStatus::Partial => "PART",
            EvalStatus::Failed => "FAIL",
            EvalStatus::TimedOut => "TIME",
            EvalStatus::Errored => "ERR ",
            EvalStatus::Skipped => "SKIP",
        }
    }

    /// Whether this status counts as a clean pass for gating purposes.
    pub fn is_pass(&self) -> bool {
        matches!(self, EvalStatus::Passed)
    }
}

/// The result of evaluating a single task. Serialised into the run summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub task_id: String,
    pub title: String,
    pub status: EvalStatus,
    pub score: EvalScore,
    /// Unified diff produced by the agent, if any.
    pub patch: Option<String>,
    pub transcript_path: PathBuf,
    pub events_path: PathBuf,
    pub report_path: PathBuf,
    pub workspace_path: PathBuf,

    // Aggregation extras.
    pub provider: String,
    pub suites: Vec<String>,
    pub tags: Vec<String>,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub tool_calls: usize,
    pub turns_used: usize,
    pub duration_ms: u64,
    pub skipped_reason: Option<String>,
}

/// Configuration for an eval run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub provider: ProviderMode,
    /// Extra wall-clock grace beyond the task timeout before the harness force
    /// aborts the run.
    pub timeout_grace: Duration,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            provider: ProviderMode::Fake,
            timeout_grace: Duration::from_secs(30),
        }
    }
}

/// Collects SDK events into a shared buffer.
struct CollectingSink {
    events: Arc<Mutex<Vec<SdkEvent>>>,
}

#[async_trait]
impl EventSink for CollectingSink {
    async fn emit(&self, event: SdkEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Internal: the outcome of driving the agent (before oracle/scoring).
struct RunOutcome {
    events: Vec<SdkEvent>,
    summary: String,
    timed_out: bool,
    errored: bool,
}

/// Build the tool policy for a task from its `allowed_tools` list.
fn tool_policy_for(task: &EvalTask) -> ToolPolicy {
    let mut builtins = BuiltinToolSet::cli_defaults();
    if !task.allowed_tools.is_empty() {
        let allowed = |name: &str| task.allowed_tools.iter().any(|t| t == name);
        builtins.read = allowed("Read");
        builtins.write = allowed("Write");
        builtins.edit = allowed("Edit");
        builtins.glob = allowed("Glob");
        builtins.grep = allowed("Grep");
        builtins.bash = allowed("Bash");
        builtins.todo = allowed("TodoWrite");
    }
    ToolPolicy {
        enable_builtin_tools: builtins,
        side_effect_policy: SideEffectPolicy::full(),
        ..Default::default()
    }
}

/// Drive the agent for a task in the prepared workspace.
async fn drive_agent(
    task: &EvalTask,
    workspace_dir: &std::path::Path,
    config: &RunConfig,
) -> Result<RunOutcome, String> {
    let setup = providers::build(&config.provider, task).map_err(|u| u.reason)?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(CollectingSink {
        events: events.clone(),
    });

    let runtime = AgentRuntime::builder()
        .provider(setup.config)
        .model(setup.model)
        .workspace(WorkspaceSource::filesystem(workspace_dir.to_path_buf()))
        .tool_policy(tool_policy_for(task))
        .interactivity(InteractivityMode::Headless)
        .approval_callback(Arc::new(AllowAllApprovals))
        .event_sink(sink)
        .build()
        .await
        .map_err(|e| format!("runtime build failed: {}", e.message))?;

    let mut request = AgentRequest::new(task.prompt.clone())
        .max_turns(task.max_turns as u32)
        .deadline(task.timeout)
        .approval_policy(ApprovalPolicy::CallbackRequired)
        .interactivity(InteractivityMode::Headless);
    request = match task.mode {
        TaskMode::ModifyWorkspace => request
            .mode(AgentMode::ModifyWorkspace)
            .patch_policy(PatchPolicy::ApplyToWorkspace),
        TaskMode::PlanThenAct => request
            .mode(AgentMode::PlanThenAct)
            .plan_policy(PlanPolicy::PlanThenAct)
            .patch_policy(PatchPolicy::ApplyToWorkspace),
    };

    let hard_deadline = task.timeout + config.timeout_grace;
    let result = tokio::time::timeout(hard_deadline, runtime.run_once(request)).await;

    let collected = events.lock().unwrap().clone();
    match result {
        Err(_elapsed) => Ok(RunOutcome {
            events: collected,
            summary: "run exceeded the hard wall-clock deadline".to_string(),
            timed_out: true,
            errored: true,
        }),
        Ok(Err(sdk_err)) => Ok(RunOutcome {
            events: collected,
            summary: format!("run failed: {}", sdk_err.message),
            timed_out: false,
            errored: true,
        }),
        Ok(Ok(run_result)) => {
            use code_agent_sdk::AgentRunStatus;
            let errored = !matches!(run_result.status, AgentRunStatus::Completed);
            let timed_out = matches!(run_result.status, AgentRunStatus::TimedOut);
            Ok(RunOutcome {
                events: collected,
                summary: run_result.summary,
                timed_out,
                errored,
            })
        }
    }
}

/// Count the turns the agent took (assistant messages observed).
fn count_turns(events: &[SdkEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, SdkEvent::AssistantMessage(_)))
        .count()
}

fn count_tool_calls(events: &[SdkEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, SdkEvent::ToolCallStarted(_)))
        .count()
}

/// Events worth keeping in the human-facing transcript.
fn is_transcript_event(e: &SdkEvent) -> bool {
    matches!(
        e,
        SdkEvent::AssistantMessage(_)
            | SdkEvent::ToolCallStarted(_)
            | SdkEvent::ToolCallCompleted(_)
            | SdkEvent::PlanCreated(_)
            | SdkEvent::ApprovalResolved(_)
            | SdkEvent::RunCompleted(_)
            | SdkEvent::RunFailed(_)
    )
}

/// Decide the final status from the run outcome and oracle report.
fn decide_status(outcome: &RunOutcome, oracle: &OracleReport) -> EvalStatus {
    if outcome.timed_out {
        return EvalStatus::TimedOut;
    }
    let passed = oracle.checks_passed();
    let total = oracle.checks_total();
    if outcome.errored && passed == 0 {
        return EvalStatus::Errored;
    }
    if total == 0 {
        // No oracle checks: a completed run is a pass, an errored one is not.
        return if outcome.errored {
            EvalStatus::Errored
        } else {
            EvalStatus::Passed
        };
    }
    if passed == total {
        EvalStatus::Passed
    } else if passed == 0 {
        EvalStatus::Failed
    } else {
        EvalStatus::Partial
    }
}

/// Run a single task and write its artifact bundle.
pub async fn run_task(task: &EvalTask, run_dir: &RunDir, config: &RunConfig) -> EvalResult {
    let started = Instant::now();
    let artifacts = match run_dir.task(&task.id) {
        Ok(a) => a,
        Err(e) => {
            // We cannot write artifacts; return an errored result with paths
            // pointing at where they would have lived.
            return errored_result(
                task,
                run_dir,
                &config.provider,
                format!("artifact dir: {e}"),
            );
        }
    };

    // Always persist the manifest and prompt up front so even a crashed run is
    // debuggable.
    let _ = persist_manifest(task, &artifacts);
    let _ = artifacts.write_string("prompt.md", &task.prompt);

    let workspace_dir = artifacts.workspace_dir();

    // Provider availability is checked first so skips are clean and cheap.
    if let Err(unavailable) = providers::build(&config.provider, task) {
        return skipped_result(
            task,
            &artifacts,
            &config.provider,
            unavailable.reason,
            started,
        );
    }

    if let Err(e) = prepare_workspace(task, &workspace_dir) {
        let _ = artifacts.write_string(
            "final_summary.md",
            &format!("# {}\n\nworkspace setup failed: {e}\n", task.id),
        );
        return errored_result(
            task,
            run_dir,
            &config.provider,
            format!("workspace setup: {e}"),
        );
    }

    let outcome = match drive_agent(task, &workspace_dir, config).await {
        Ok(o) => o,
        Err(reason) => {
            // Could be a skip (provider unavailable) surfaced late, but build()
            // already guarded that; treat as errored.
            RunOutcome {
                events: Vec::new(),
                summary: reason.clone(),
                timed_out: false,
                errored: true,
            }
        }
    };

    let patch = generate_patch(&workspace_dir).unwrap_or_default();
    let oracle_report = oracle::evaluate(task, &workspace_dir, &outcome.events).await;

    let turns_used = count_turns(&outcome.events);
    let tool_calls = count_tool_calls(&outcome.events);
    let score = score::compute(&ScoreInputs {
        task,
        oracle: &oracle_report,
        patch: &patch,
        events: &outcome.events,
        turns_used,
        max_turns: task.max_turns,
        timed_out: outcome.timed_out,
        errored: outcome.errored,
    });

    let status = decide_status(&outcome, &oracle_report);

    write_run_artifacts(
        &artifacts,
        task,
        &outcome,
        &patch,
        &oracle_report,
        &score,
        status,
    );

    EvalResult {
        task_id: task.id.clone(),
        title: task.title.clone(),
        status,
        score,
        patch: if patch.is_empty() {
            None
        } else {
            Some(patch.unified_diff.clone())
        },
        transcript_path: artifacts.transcript_path(),
        events_path: artifacts.events_path(),
        report_path: artifacts.report_path(),
        workspace_path: workspace_dir,
        provider: config.provider.label(),
        suites: task.suites.clone(),
        tags: task.tags.clone(),
        checks_passed: oracle_report.checks_passed(),
        checks_total: oracle_report.checks_total(),
        tool_calls,
        turns_used,
        duration_ms: started.elapsed().as_millis() as u64,
        skipped_reason: None,
    }
}

/// Write every per-task artifact for a completed (or failed) run.
#[allow(clippy::too_many_arguments)]
fn write_run_artifacts(
    artifacts: &TaskArtifacts,
    task: &EvalTask,
    outcome: &RunOutcome,
    patch: &PatchInfo,
    oracle: &OracleReport,
    score: &EvalScore,
    status: EvalStatus,
) {
    let _ = artifacts.write_jsonl("events.jsonl", &outcome.events);
    let transcript: Vec<&SdkEvent> = outcome
        .events
        .iter()
        .filter(|e| is_transcript_event(e))
        .collect();
    let _ = artifacts.write_jsonl("transcript.jsonl", &transcript);
    let _ = std::fs::write(artifacts.final_patch_path(), &patch.unified_diff);
    let _ = std::fs::write(artifacts.final_diff_path(), &patch.stat);
    let _ = artifacts.write_command_outputs(&oracle.commands);
    let _ = artifacts.write_json("score.json", score);

    let report = crate::report::render_task_report(
        task,
        outcome.errored,
        &outcome.summary,
        patch,
        oracle,
        score,
        status,
    );
    let _ = artifacts.write_string("report.md", &report);

    let final_summary = format!(
        "# {} — {:?}\n\n{}\n\nscore: {:.3} (correctness {:.2}, patch {:.2}, process {:.2}, efficiency {:.2})\n\n## Patch\n\n```\n{}\n```\n",
        task.id,
        status,
        outcome.summary,
        score.total,
        score.correctness,
        score.patch_quality,
        score.process_quality,
        score.efficiency,
        if patch.stat.is_empty() { "(no changes)" } else { &patch.stat },
    );
    let _ = artifacts.write_string("final_summary.md", &final_summary);
}

fn persist_manifest(task: &EvalTask, artifacts: &TaskArtifacts) -> std::io::Result<()> {
    let manifest_src = task.dir.join("task.toml");
    if manifest_src.exists() {
        let text = std::fs::read_to_string(&manifest_src)?;
        artifacts.write_string("task.toml", &text)
    } else {
        Ok(())
    }
}

fn skipped_result(
    task: &EvalTask,
    artifacts: &TaskArtifacts,
    provider: &ProviderMode,
    reason: String,
    started: Instant,
) -> EvalResult {
    let score = EvalScore::zero(format!("skipped: {reason}"));
    let _ = artifacts.write_json("score.json", &score);
    let _ = artifacts.write_string(
        "report.md",
        &format!("# {} — SKIPPED\n\n{reason}\n", task.id),
    );
    EvalResult {
        task_id: task.id.clone(),
        title: task.title.clone(),
        status: EvalStatus::Skipped,
        score,
        patch: None,
        transcript_path: artifacts.transcript_path(),
        events_path: artifacts.events_path(),
        report_path: artifacts.report_path(),
        workspace_path: artifacts.workspace_dir(),
        provider: provider.label(),
        suites: task.suites.clone(),
        tags: task.tags.clone(),
        checks_passed: 0,
        checks_total: task.oracle.check_count(),
        tool_calls: 0,
        turns_used: 0,
        duration_ms: started.elapsed().as_millis() as u64,
        skipped_reason: Some(reason),
    }
}

fn errored_result(
    task: &EvalTask,
    run_dir: &RunDir,
    provider: &ProviderMode,
    detail: String,
) -> EvalResult {
    let dir = run_dir.root.join("tasks").join(&task.id);
    EvalResult {
        task_id: task.id.clone(),
        title: task.title.clone(),
        status: EvalStatus::Errored,
        score: EvalScore::zero(format!("errored: {detail}")),
        patch: None,
        transcript_path: dir.join("transcript.jsonl"),
        events_path: dir.join("events.jsonl"),
        report_path: dir.join("report.md"),
        workspace_path: dir.join("workspace"),
        provider: provider.label(),
        suites: task.suites.clone(),
        tags: task.tags.clone(),
        checks_passed: 0,
        checks_total: task.oracle.check_count(),
        tool_calls: 0,
        turns_used: 0,
        duration_ms: 0,
        skipped_reason: None,
    }
}

/// Run a selection of tasks sequentially, returning their results.
pub async fn run_tasks(
    tasks: &[EvalTask],
    run_dir: &RunDir,
    config: &RunConfig,
) -> Vec<EvalResult> {
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        let result = run_task(task, run_dir, config).await;
        results.push(result);
    }
    results
}
