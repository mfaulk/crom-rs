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
use crate::behavior::{self, BehaviorMetrics};
use crate::failure::{self, FailureClass, FailureSignals};
use crate::fixture::{copy_dir_recursive, generate_patch, prepare_workspace, PatchInfo};
use crate::isolation::{self, CommandLimits, EnvMetadata, IsolationKind};
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
    pub model: String,
    pub suites: Vec<String>,
    pub tags: Vec<String>,
    pub checks_passed: usize,
    pub checks_total: usize,
    pub tool_calls: usize,
    pub turns_used: usize,
    pub duration_ms: u64,
    pub skipped_reason: Option<String>,

    // --- M15 additions ---
    /// Primary binary outcome: public + hidden pass, no policy/path violation.
    pub resolved: bool,
    pub failure_class: Option<FailureClass>,
    pub secondary_failures: Vec<FailureClass>,
    pub public_passed: bool,
    pub hidden_present: bool,
    pub hidden_passed: Option<bool>,
    pub behavior: BehaviorMetrics,
    pub behavior_warnings: Vec<String>,
    pub category: Option<String>,
    pub difficulty: Option<String>,
    pub capabilities: Vec<String>,
    pub adapter: String,
    pub isolation: String,
    pub quarantined: bool,
    pub patch_lines_added: usize,
    pub patch_lines_removed: usize,
    /// Repetition index (0-based) and seed for multi-run aggregation.
    pub repetition: usize,
    pub seed: u64,
    pub attempts_used: usize,
    /// Estimated cost in USD if the provider reports usage; `None` otherwise.
    pub cost_usd: Option<f64>,
}

/// Configuration for an eval run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub provider: ProviderMode,
    /// Extra wall-clock grace beyond the task timeout before the harness force
    /// aborts the run.
    pub timeout_grace: Duration,
    /// Adapter id that produced the task (recorded in artifacts/index).
    pub adapter: String,
    /// Isolation backend for verification commands.
    pub isolation: IsolationKind,
    /// Container image override (container backends only).
    pub container_image: Option<String>,
    /// Whether verification may access the network.
    pub network: bool,
    /// Optional CPU limit (container backends only).
    pub cpu_limit: Option<f64>,
    /// Optional memory limit in bytes (container backends only).
    pub memory_limit: Option<u64>,
    /// Prompt variant name (`None` => default prompt; `Some("random")` picks).
    pub prompt_variant: Option<String>,
    /// Repetition index for this run (0-based).
    pub repetition: usize,
    /// Seed used for prompt-variant selection and reporting.
    pub seed: u64,
    /// Max attempts (Aider-style retry with test feedback). Default 1.
    pub attempts: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            provider: ProviderMode::Fake,
            timeout_grace: Duration::from_secs(30),
            adapter: "internal".to_string(),
            isolation: IsolationKind::Local,
            container_image: None,
            network: false,
            cpu_limit: None,
            memory_limit: None,
            prompt_variant: None,
            repetition: 0,
            seed: 0,
            attempts: 1,
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
    prompt: &str,
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

    let mut request = AgentRequest::new(prompt.to_string())
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
            let mut events = collected;
            // The SDK plan/act path returns the plan on the result rather than
            // always emitting a `PlanCreated` into the event sink. Synthesize one
            // so behavior oracles (`must_emit_plan`) and process scoring can
            // observe that a plan was produced.
            if let Some(plan) = run_result.plan.clone() {
                let already = events.iter().any(|e| matches!(e, SdkEvent::PlanCreated(_)));
                if !already {
                    events.push(SdkEvent::PlanCreated(code_agent_sdk::event::PlanCreated {
                        run_id: run_result.run_id.clone(),
                        plan,
                    }));
                }
            }
            Ok(RunOutcome {
                events,
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

/// Build an `EvalResult` skeleton with neutral M15 fields filled from the task
/// and config. Callers override `score`, `status`, and outcome-specific fields.
fn default_result(
    task: &EvalTask,
    config: &RunConfig,
    status: EvalStatus,
    dir: &std::path::Path,
) -> EvalResult {
    EvalResult {
        task_id: task.id.clone(),
        title: task.title.clone(),
        status,
        score: EvalScore::zero(""),
        patch: None,
        transcript_path: dir.join("transcript.jsonl"),
        events_path: dir.join("events.jsonl"),
        report_path: dir.join("report.md"),
        workspace_path: dir.join("workspace"),
        provider: config.provider.label(),
        model: provider_model(&config.provider),
        suites: task.suites.clone(),
        tags: task.tags.clone(),
        checks_passed: 0,
        checks_total: task.oracle.check_count(),
        tool_calls: 0,
        turns_used: 0,
        duration_ms: 0,
        skipped_reason: None,
        resolved: false,
        failure_class: None,
        secondary_failures: Vec::new(),
        public_passed: false,
        hidden_present: task.hidden.as_ref().is_some_and(|h| h.has_checks()),
        hidden_passed: None,
        behavior: BehaviorMetrics::default(),
        behavior_warnings: Vec::new(),
        category: task.category.clone(),
        difficulty: task.difficulty.clone(),
        capabilities: task
            .capabilities
            .enabled_labels()
            .into_iter()
            .map(String::from)
            .collect(),
        adapter: config.adapter.clone(),
        isolation: config.isolation.id().to_string(),
        quarantined: task.is_quarantined(),
        patch_lines_added: 0,
        patch_lines_removed: 0,
        repetition: config.repetition,
        seed: config.seed,
        attempts_used: 0,
        cost_usd: None,
    }
}

fn provider_model(provider: &ProviderMode) -> String {
    match provider {
        ProviderMode::Fake => "fake-echo".to_string(),
        ProviderMode::Golden => "golden".to_string(),
        ProviderMode::Live(_) => std::env::var("CODE_AGENT_EVAL_MODEL")
            .unwrap_or_else(|_| "claude-sonnet-4-6".to_string()),
    }
}

/// Per-command isolation limits for a task under a run config.
fn limits_for(task: &EvalTask, config: &RunConfig) -> CommandLimits {
    CommandLimits {
        timeout: oracle::task_command_timeout(task),
        network: config.network || task.requires_network,
        cpu_limit: config.cpu_limit,
        memory_limit: config.memory_limit,
        container_image: config.container_image.clone(),
    }
}

/// Whether `path` matches a forbidden/required pattern. Supports exact match,
/// directory prefix (`dir/`), and a trailing `*` wildcard.
fn path_matches(path: &str, pattern: &str) -> bool {
    let path = path.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    if pattern.ends_with('/') {
        return path.starts_with(&pattern);
    }
    path == pattern || path.starts_with(&format!("{pattern}/"))
}

const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "mkfs",
    "dd if=",
    ":(){",
    "> /dev/sd",
    "chmod -r 000",
];

fn unsafe_command_used(events: &[SdkEvent]) -> bool {
    oracle::bash_commands(events).any(|c| {
        let lc = c.to_lowercase();
        DESTRUCTIVE_PATTERNS.iter().any(|p| lc.contains(p))
    })
}

/// Heuristic: did a verification command fail with a compiler error?
fn looks_like_compile_error(report: &OracleReport) -> bool {
    report.commands.iter().any(|c| {
        !c.success
            && (c.stderr.contains("error[")
                || c.stderr.contains("could not compile")
                || c.stderr.contains("cannot find")
                || c.stdout.contains("error[")
                || c.stdout.contains("could not compile"))
    })
}

fn excessive_patch(task: &EvalTask, patch: &PatchInfo) -> bool {
    let churn = patch.insertions + patch.deletions;
    let files = patch.changed_files.len();
    let small = task.tags.iter().any(|t| t == "small");
    (small && (churn > 200 || files > 6)) || churn > 800 || files > 30
}

/// Overlay a hidden fixture directory (if any) into the workspace, then run the
/// hidden oracle. Returns `None` when the task has no hidden checks.
async fn run_hidden(
    task: &EvalTask,
    workspace_dir: &std::path::Path,
    events: &[SdkEvent],
    backend: &dyn isolation::IsolationBackend,
    limits: &CommandLimits,
) -> Option<OracleReport> {
    let hidden = task.hidden.as_ref()?;
    if !hidden.has_checks() {
        return None;
    }
    if let Some(rel) = &hidden.fixture {
        let src = task.dir.join(rel);
        if src.exists() {
            let _ = copy_dir_recursive(&src, workspace_dir);
        }
    }
    Some(oracle::evaluate_oracle(&hidden.oracle, workspace_dir, events, backend, limits).await)
}

/// Run a single task and write its artifact bundle.
pub async fn run_task(task: &EvalTask, run_dir: &RunDir, config: &RunConfig) -> EvalResult {
    let started = Instant::now();
    let dir = run_dir.root.join("tasks").join(&task.id);
    let artifacts = match run_dir.task(&task.id) {
        Ok(a) => a,
        Err(e) => {
            let mut r = default_result(task, config, EvalStatus::Errored, &dir);
            r.score = EvalScore::zero(format!("errored: artifact dir: {e}"));
            r.failure_class = Some(FailureClass::HarnessError);
            r.duration_ms = started.elapsed().as_millis() as u64;
            return r;
        }
    };

    // Resolve the prompt (variant selection) and persist it + the manifest.
    let prompt = task.resolved_prompt(config.prompt_variant.as_deref(), config.seed);
    let _ = persist_manifest(task, &artifacts);
    let _ = artifacts.write_string("prompt.md", &prompt);
    write_metadata_artifacts(&artifacts, task, config);

    let workspace_dir = artifacts.workspace_dir();

    // Provider availability is checked first so skips are clean and cheap.
    if let Err(unavailable) = providers::build(&config.provider, task) {
        return skipped_result(task, &artifacts, config, unavailable.reason, started);
    }

    // Build the isolation backend; a missing engine is an isolation error.
    let backend = match isolation::build_backend(&config.isolation, config.container_image.clone())
    {
        Ok(b) => b,
        Err(e) => {
            let mut r = default_result(task, config, EvalStatus::Errored, &dir);
            r.score = EvalScore::zero(format!("isolation: {e}"));
            r.failure_class = Some(FailureClass::IsolationError);
            r.duration_ms = started.elapsed().as_millis() as u64;
            let _ = artifacts.write_string(
                "report.md",
                &format!("# {} — isolation error\n\n{e}\n", task.id),
            );
            let _ = artifacts.write_json("result.json", &r);
            return r;
        }
    };
    let limits = limits_for(task, config);
    let _ = artifacts.write_json(
        "environment.json",
        &EnvMetadata::gather(backend.as_ref(), limits.network),
    );

    if let Err(e) = prepare_workspace(task, &workspace_dir) {
        let _ = artifacts.write_string(
            "final_summary.md",
            &format!("# {}\n\nworkspace setup failed: {e}\n", task.id),
        );
        let mut r = default_result(task, config, EvalStatus::Errored, &dir);
        r.score = EvalScore::zero(format!("errored: workspace setup: {e}"));
        r.failure_class = Some(FailureClass::HarnessError);
        r.duration_ms = started.elapsed().as_millis() as u64;
        let _ = artifacts.write_json("result.json", &r);
        return r;
    }

    // Drive the agent, optionally retrying with test feedback (Aider-style).
    let mut events: Vec<SdkEvent> = Vec::new();
    let mut summary = String::new();
    let mut timed_out = false;
    let mut errored = false;
    let mut public_report = OracleReport {
        commands: Vec::new(),
        assertions: Vec::new(),
        behavior: Vec::new(),
    };
    let attempts = config.attempts.max(1);
    let mut attempts_used = 0usize;

    for attempt in 0..attempts {
        attempts_used += 1;
        let attempt_prompt = if attempt == 0 {
            prompt.clone()
        } else {
            feedback_prompt(&prompt, &public_report)
        };
        match drive_agent(task, &attempt_prompt, &workspace_dir, config).await {
            Ok(o) => {
                events.extend(o.events);
                summary = o.summary;
                timed_out |= o.timed_out;
                errored = o.errored;
            }
            Err(reason) => {
                summary = reason;
                errored = true;
            }
        }
        public_report = oracle::evaluate_oracle(
            &task.oracle,
            &workspace_dir,
            &events,
            backend.as_ref(),
            &limits,
        )
        .await;
        if public_report.all_passed() || attempt + 1 == attempts {
            break;
        }
    }

    // Generate the public patch (before any hidden overlay).
    let patch = match generate_patch(&workspace_dir) {
        Ok(p) => p,
        Err(e) => {
            errored = true;
            summary = format!("{summary}; patch generation failed: {e}");
            PatchInfo::default()
        }
    };

    // Hidden verification (overlay + run) — only after the agent has finished.
    let hidden_report = run_hidden(task, &workspace_dir, &events, backend.as_ref(), &limits).await;

    // Path policy.
    let forbidden_changed: Vec<String> = patch
        .changed_files
        .iter()
        .filter(|f| task.forbidden_paths.iter().any(|p| path_matches(f, p)))
        .cloned()
        .collect();
    let required_missing: Vec<String> = task
        .required_paths
        .iter()
        .filter(|rp| !workspace_dir.join(rp).exists())
        .cloned()
        .collect();

    // Behavior metrics + rules.
    let behavior = BehaviorMetrics::derive(&events, &patch, task);
    let rule_results = behavior::evaluate_rules(&task.behavior_rules, &behavior);
    let mut behavior_warnings = behavior::warnings(&behavior, &rule_results);
    if !forbidden_changed.is_empty() {
        behavior_warnings.push(format!("modified forbidden path(s): {forbidden_changed:?}"));
    }
    if !required_missing.is_empty() {
        behavior_warnings.push(format!("required path(s) missing: {required_missing:?}"));
    }

    // Merge public + hidden for status/check counting.
    let merged = merge_reports(&public_report, hidden_report.as_ref());

    let public_passed = public_report.all_passed();
    let hidden_passed = hidden_report.as_ref().map(|r| r.all_passed());
    let rule_violation = task.behavior_rules.enforce && rule_results.iter().any(|r| !r.satisfied);

    let resolved = public_passed
        && hidden_passed.unwrap_or(true)
        && forbidden_changed.is_empty()
        && required_missing.is_empty()
        && !timed_out
        && !rule_violation;

    // Failure classification.
    let signals = FailureSignals {
        timed_out,
        provider_unavailable: false,
        harness_errored: false,
        empty_patch: patch.is_empty(),
        public_passed,
        hidden_present: hidden_report.is_some(),
        hidden_passed: hidden_passed.unwrap_or(false),
        compile_failed: looks_like_compile_error(&public_report)
            || hidden_report.as_ref().is_some_and(looks_like_compile_error),
        forbidden_changed: !forbidden_changed.is_empty(),
        required_missing: !required_missing.is_empty(),
        unsafe_command: unsafe_command_used(&events),
        tool_loop: behavior.same_error_repeated == Some(true),
        planning_required: task.mode == TaskMode::PlanThenAct
            || task
                .oracle
                .behavior
                .iter()
                .any(|b| matches!(b, crate::task::BehaviorSpec::MustEmitPlan)),
        plan_emitted: behavior.plan_submitted == Some(true),
        excessive_patch: excessive_patch(task, &patch),
    };
    let (failure_class, secondary_failures) = if resolved {
        (None, Vec::new())
    } else {
        match failure::classify(&signals) {
            Some((p, s)) => (Some(p), s),
            // Resolved disagreement (e.g. behavior-rule enforcement): default.
            None => (Some(FailureClass::PublicTestFailure), Vec::new()),
        }
    };

    let turns_used = count_turns(&events);
    let tool_calls = count_tool_calls(&events);
    let score = score::compute(&ScoreInputs {
        task,
        oracle: &merged,
        patch: &patch,
        events: &events,
        turns_used,
        max_turns: task.max_turns,
        timed_out,
        errored,
    });

    let outcome = RunOutcome {
        events,
        summary,
        timed_out,
        errored,
    };
    let status = status_for(resolved, &outcome, &merged);

    write_run_artifacts(WriteArtifacts {
        artifacts: &artifacts,
        task,
        outcome: &outcome,
        patch: &patch,
        public: &public_report,
        hidden: hidden_report.as_ref(),
        merged: &merged,
        score: &score,
        status,
        resolved,
        behavior: &behavior,
        rule_results: &rule_results,
        warnings: &behavior_warnings,
        forbidden: &forbidden_changed,
        required_missing: &required_missing,
    });

    let mut result = default_result(task, config, status, &dir);
    result.score = score;
    result.patch = if patch.is_empty() {
        None
    } else {
        Some(patch.unified_diff.clone())
    };
    result.workspace_path = workspace_dir;
    result.checks_passed = merged.checks_passed();
    result.checks_total = merged.checks_total();
    result.tool_calls = tool_calls;
    result.turns_used = turns_used;
    result.duration_ms = started.elapsed().as_millis() as u64;
    result.resolved = resolved;
    result.failure_class = failure_class;
    result.secondary_failures = secondary_failures;
    result.public_passed = public_passed;
    result.hidden_passed = hidden_passed;
    result.behavior = behavior;
    result.behavior_warnings = behavior_warnings;
    result.patch_lines_added = patch.insertions;
    result.patch_lines_removed = patch.deletions;
    result.attempts_used = attempts_used;

    let _ = artifacts.write_json("result.json", &result);
    result
}

/// Build the second-attempt prompt from the failing public verification.
fn feedback_prompt(original: &str, public: &OracleReport) -> String {
    /// Last `n` lines of a stream (compilers/test runners put failures here).
    fn tail(s: &str, n: usize) -> String {
        let lines: Vec<&str> = s.lines().collect();
        let start = lines.len().saturating_sub(n);
        lines[start..].join("\n")
    }
    let mut feedback = String::from("Your previous attempt did not pass verification.\n");
    for c in public.commands.iter().filter(|c| !c.success) {
        feedback.push_str(&format!("\n$ {}\n", c.command));
        // Show the tail of stdout and stderr independently so the error output
        // (usually on stderr) is never crowded out by a large stdout log.
        let out = tail(&c.stdout, 15);
        if !out.trim().is_empty() {
            feedback.push_str("[stdout]\n");
            feedback.push_str(&out);
            feedback.push('\n');
        }
        let err = tail(&c.stderr, 25);
        if !err.trim().is_empty() {
            feedback.push_str("[stderr]\n");
            feedback.push_str(&err);
            feedback.push('\n');
        }
    }
    format!("{original}\n\n---\n{feedback}\nPlease fix the issue and try again.")
}

/// Decide the displayed status from `resolved` and the merged oracle.
fn status_for(resolved: bool, outcome: &RunOutcome, merged: &OracleReport) -> EvalStatus {
    if outcome.timed_out {
        return EvalStatus::TimedOut;
    }
    if resolved {
        return EvalStatus::Passed;
    }
    let passed = merged.checks_passed();
    let total = merged.checks_total();
    if outcome.errored && passed == 0 {
        return EvalStatus::Errored;
    }
    if total == 0 {
        return if outcome.errored {
            EvalStatus::Errored
        } else {
            // No checks but not resolved (e.g. path policy) -> Failed.
            EvalStatus::Failed
        };
    }
    if passed == 0 {
        EvalStatus::Failed
    } else {
        EvalStatus::Partial
    }
}

/// Merge public and hidden reports into one for aggregate counts/status.
fn merge_reports(public: &OracleReport, hidden: Option<&OracleReport>) -> OracleReport {
    let mut merged = public.clone();
    if let Some(h) = hidden {
        merged.commands.extend(h.commands.iter().cloned());
        merged.assertions.extend(h.assertions.iter().cloned());
        merged.behavior.extend(h.behavior.iter().cloned());
    }
    merged
}

/// Concatenate command outputs into a single log string.
fn commands_log(report: &OracleReport) -> String {
    let mut s = String::new();
    for c in &report.commands {
        s.push_str(&format!(
            "$ {}\n[exit {:?}{}]\n{}\n{}\n\n",
            c.command,
            c.exit_code,
            if c.timed_out { ", timed out" } else { "" },
            c.stdout,
            c.stderr
        ));
    }
    s
}

/// Bundle of everything `write_run_artifacts` needs.
struct WriteArtifacts<'a> {
    artifacts: &'a TaskArtifacts,
    task: &'a EvalTask,
    outcome: &'a RunOutcome,
    patch: &'a PatchInfo,
    public: &'a OracleReport,
    hidden: Option<&'a OracleReport>,
    merged: &'a OracleReport,
    score: &'a EvalScore,
    status: EvalStatus,
    resolved: bool,
    behavior: &'a BehaviorMetrics,
    rule_results: &'a [behavior::BehaviorRuleResult],
    warnings: &'a [String],
    forbidden: &'a [String],
    required_missing: &'a [String],
}

/// Write every per-task artifact for a completed (or failed) run.
fn write_run_artifacts(w: WriteArtifacts<'_>) {
    let a = w.artifacts;
    let _ = a.write_jsonl("events.jsonl", &w.outcome.events);
    let transcript: Vec<&SdkEvent> = w
        .outcome
        .events
        .iter()
        .filter(|e| is_transcript_event(e))
        .collect();
    let _ = a.write_jsonl("transcript.jsonl", &transcript);
    // Tool-call log.
    let tool_calls: Vec<&SdkEvent> = w
        .outcome
        .events
        .iter()
        .filter(|e| {
            matches!(
                e,
                SdkEvent::ToolCallStarted(_) | SdkEvent::ToolCallCompleted(_)
            )
        })
        .collect();
    let _ = a.write_jsonl("tool-calls.jsonl", &tool_calls);

    let _ = std::fs::write(a.final_patch_path(), &w.patch.unified_diff);
    let _ = std::fs::write(a.final_diff_path(), &w.patch.stat);
    let _ = a.write_string("patch.diff", &w.patch.unified_diff);
    let _ = a.write_command_outputs(&w.merged.commands);
    let _ = a.write_string("public-test.log", &commands_log(w.public));
    if let Some(h) = w.hidden {
        let _ = a.write_string("hidden-test.log", &commands_log(h));
    }
    let _ = a.write_json("score.json", w.score);

    // Behavior + verification structured artifacts.
    let _ = a.write_json(
        "behavior.json",
        &serde_json::json!({
            "metrics": w.behavior,
            "rules": w.rule_results,
            "warnings": w.warnings,
        }),
    );
    let _ = a.write_json(
        "verification.json",
        &serde_json::json!({
            "resolved": w.resolved,
            "public": w.public,
            "hidden": w.hidden,
            "forbidden_changed": w.forbidden,
            "required_missing": w.required_missing,
        }),
    );

    let report = crate::report::render_task_report(
        w.task,
        w.outcome.errored,
        &w.outcome.summary,
        w.patch,
        w.merged,
        w.score,
        w.status,
    );
    let _ = a.write_string("report.md", &report);

    // Triage note.
    let mut triage = format!(
        "# {} — {:?}\n\nresolved: {}\n",
        w.task.id, w.status, w.resolved
    );
    if !w.warnings.is_empty() {
        triage.push_str("\nWarnings:\n");
        for warn in w.warnings {
            triage.push_str(&format!("- {warn}\n"));
        }
    }
    let _ = a.write_string("triage.md", &triage);

    let final_summary = format!(
        "# {} — {:?}\n\n{}\n\nscore: {:.3} (correctness {:.2}, patch {:.2}, process {:.2}, efficiency {:.2})\n\n## Patch\n\n```\n{}\n```\n",
        w.task.id,
        w.status,
        w.outcome.summary,
        w.score.total,
        w.score.correctness,
        w.score.patch_quality,
        w.score.process_quality,
        w.score.efficiency,
        if w.patch.stat.is_empty() { "(no changes)" } else { &w.patch.stat },
    );
    let _ = a.write_string("final_summary.md", &final_summary);
}

/// Write adapter/isolation/normalized-task metadata up front.
fn write_metadata_artifacts(artifacts: &TaskArtifacts, task: &EvalTask, config: &RunConfig) {
    let _ = artifacts.write_json(
        "adapter.json",
        &serde_json::json!({ "adapter": config.adapter }),
    );
    let _ = artifacts.write_json(
        "isolation.json",
        &serde_json::json!({
            "backend": config.isolation.id(),
            "network": config.network || task.requires_network,
            "cpu_limit": config.cpu_limit,
            "memory_limit": config.memory_limit,
            "container_image": config.container_image,
        }),
    );
    let _ = artifacts.write_json(
        "normalized-task.json",
        &serde_json::json!({
            "id": task.id,
            "title": task.title,
            "schema_version": task.schema_version,
            "category": task.category,
            "difficulty": task.difficulty,
            "suites": task.suites,
            "tags": task.tags,
            "capabilities": task.capabilities.enabled_labels(),
            "has_hidden": task.hidden.as_ref().is_some_and(|h| h.has_checks()),
            "forbidden_paths": task.forbidden_paths,
            "required_paths": task.required_paths,
            "visibility": task.visibility,
            "quarantined": task.is_quarantined(),
        }),
    );
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
    config: &RunConfig,
    reason: String,
    started: Instant,
) -> EvalResult {
    let score = EvalScore::zero(format!("skipped: {reason}"));
    let _ = artifacts.write_json("score.json", &score);
    let _ = artifacts.write_string(
        "report.md",
        &format!("# {} — SKIPPED\n\n{reason}\n", task.id),
    );
    let mut r = default_result(task, config, EvalStatus::Skipped, &artifacts.dir);
    r.score = score;
    r.skipped_reason = Some(reason);
    r.duration_ms = started.elapsed().as_millis() as u64;
    let _ = artifacts.write_json("result.json", &r);
    r
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

/// Run a selection of tasks `repetitions` times. Each repetition writes its
/// artifacts under `rep-<n>/tasks/<id>` and carries its own seed so prompt
/// variant selection differs deterministically across repetitions.
pub async fn run_tasks_repeated(
    tasks: &[EvalTask],
    run_dir: &RunDir,
    config: &RunConfig,
    repetitions: usize,
) -> Vec<EvalResult> {
    let repetitions = repetitions.max(1);
    let mut all = Vec::new();
    for rep in 0..repetitions {
        // Each repetition gets its own sub-run-dir so artifacts never collide.
        // If a repetition's dir cannot be created, skip it rather than writing
        // into the parent dir (which would clobber other repetitions' artifacts).
        let sub = if repetitions == 1 {
            run_dir.clone()
        } else {
            match RunDir::create(&run_dir.root, &format!("rep-{rep}")) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("skipping repetition {rep}: cannot create run dir: {e}");
                    continue;
                }
            }
        };
        let mut rep_config = config.clone();
        rep_config.repetition = rep;
        rep_config.seed = config.seed.wrapping_add(rep as u64);
        let results = run_tasks(tasks, &sub, &rep_config).await;
        all.extend(results);
    }
    all
}

/// Test-only helpers for building `EvalResult` values without a full run.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::path::PathBuf;

    /// A minimal result for aggregation/triage tests.
    pub fn result_with(
        task_id: &str,
        resolved: bool,
        failure_class: Option<FailureClass>,
    ) -> EvalResult {
        EvalResult {
            task_id: task_id.to_string(),
            title: task_id.to_string(),
            status: if resolved {
                EvalStatus::Passed
            } else {
                EvalStatus::Failed
            },
            score: EvalScore::zero(""),
            patch: None,
            transcript_path: PathBuf::from("t"),
            events_path: PathBuf::from("e"),
            report_path: PathBuf::from("r"),
            workspace_path: PathBuf::from("w"),
            provider: "fake".to_string(),
            model: "fake".to_string(),
            suites: vec!["rust-mini".to_string()],
            tags: vec![],
            checks_passed: if resolved { 1 } else { 0 },
            checks_total: 1,
            tool_calls: 3,
            turns_used: 1,
            duration_ms: 100,
            skipped_reason: None,
            resolved,
            failure_class,
            secondary_failures: vec![],
            public_passed: resolved,
            hidden_present: false,
            hidden_passed: None,
            behavior: BehaviorMetrics::default(),
            behavior_warnings: vec![],
            category: Some("bugfix".to_string()),
            difficulty: Some("easy".to_string()),
            capabilities: vec![],
            adapter: "internal".to_string(),
            isolation: "local".to_string(),
            quarantined: false,
            patch_lines_added: 1,
            patch_lines_removed: 0,
            repetition: 0,
            seed: 0,
            attempts_used: 1,
            cost_usd: None,
        }
    }
}
