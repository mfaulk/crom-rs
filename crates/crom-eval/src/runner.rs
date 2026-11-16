//! Run a single task or a selection of tasks end-to-end.
//!
//! The runner drives the agent through the headless [`crom_sdk`] runtime
//! in an isolated, git-backed workspace, captures the structured event stream,
//! evaluates the oracles, scores the result, and writes the full artifact
//! bundle.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crom_sdk::policy::BuiltinToolSet;
use crom_sdk::{
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
use crate::result_schema::{
    Outcome, ReproInputs, ReproMetadata, RunEnvironment, LEGACY_RESULT_SCHEMA_VERSION,
    RESULT_SCHEMA_VERSION,
};
use crate::score::{self, EvalScore, ScoreInputs};
use crate::task::{EvalTask, TaskMode};
use crate::workspace::AgentWorkspace;

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
    /// Result schema version: 2 for results written by this build; 1 when
    /// the field is absent (pre-M17.4 results).
    #[serde(default = "legacy_result_schema_version")]
    pub schema_version: u32,
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
    /// Nominal list-price cost in USD (pricing table x reported usage, all
    /// attempts). `None` whenever any needed usage or price is unknown; see
    /// `cost_unknown_reason`. Never zero-filled.
    pub cost_usd: Option<f64>,
    /// Why `cost_usd` is `None` (`no_pricing_entry`, `usage_unknown`, ...).
    #[serde(default)]
    pub cost_unknown_reason: Option<String>,
    /// SDK budget diagnostic that stopped the agent
    /// (`budget.max_cost_exceeded`, `budget.max_tool_calls_exceeded`, ...).
    #[serde(default)]
    pub budget_exceeded: Option<String>,
    /// M16: repair-loop metrics (always present; `enabled=false` when the
    /// repair loop was off).
    #[serde(default)]
    pub repair: crate::repair::RepairMetrics,
    /// Token usage summed strictly over every model round trip in every
    /// attempt. `None` (JSON `null`) when there were no round trips or any
    /// round trip reported no usage; individual fields are `null` when any
    /// round trip left that quantity unreported. Never zero-filled.
    #[serde(default)]
    pub usage: Option<crom_core::TokenUsage>,
    /// Number of model round trips (`ModelTurnCompleted` events) across all
    /// attempts, including planning turns and turns with unknown usage.
    #[serde(default)]
    pub model_round_trips: usize,
    /// M17.4 outcome class. Always set on v2 results; `None` on v1 results,
    /// where [`EvalResult::outcome`] derives it.
    #[serde(default)]
    pub outcome: Option<Outcome>,
    /// M17.4 reproducibility metadata. `None` on v1 results.
    #[serde(default)]
    pub repro: Option<ReproMetadata>,
    /// Task kind from a v3 manifest (#285). `None` for v1/v2 tasks and for
    /// results written before the field existed; such results are reported
    /// as `unclassified`, never as capability.
    #[serde(default)]
    pub kind: Option<crate::schema::TaskKind>,
    /// Capability level from a v3 manifest; `None` like `kind`.
    #[serde(default)]
    pub level: Option<crate::schema::Level>,
}

fn legacy_result_schema_version() -> u32 {
    LEGACY_RESULT_SCHEMA_VERSION
}

impl EvalResult {
    /// The recorded outcome, or one derived from status/resolution/failure
    /// class for v1 results.
    pub fn outcome(&self) -> Outcome {
        self.outcome
            .unwrap_or_else(|| Outcome::derive(self.status, self.resolved, self.failure_class))
    }
}

/// Configuration for an eval run.
#[derive(Debug, Clone)]
pub struct RunConfig {
    pub provider: ProviderMode,
    /// Live model override (`--model`). Falls back to `CROM_EVAL_MODEL`,
    /// then the profile's `default_model`. Ignored by offline providers.
    pub model: Option<String>,
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
    /// M16: bounded repair-loop policy. When enabled, failed validations are
    /// parsed into structured diagnostics and a targeted repair brief drives
    /// the next attempt (up to `repair.max_attempts`). Disabled by default to
    /// preserve existing single-attempt behavior.
    pub repair: crom_core::repair::RepairPolicy,
    /// Settings-level provider-profile overrides used to resolve live
    /// providers (same resolver as the CLI and SDK).
    pub provider_profiles:
        std::collections::BTreeMap<String, crom_core::provider_profile::ProviderProfileConfig>,
    /// Run-level git/host/build context recorded in each result's `repro`.
    /// Gathered once per `run_tasks` call when `None`.
    pub environment: Option<RunEnvironment>,
    /// Parent dir for agent workspaces (`None` => `$TMPDIR/crom-eval-ws`).
    /// Must be disjoint from the runs dir and the task source (M17.2).
    pub workspace_root: Option<PathBuf>,
    /// OS sandbox for agent tool processes (M17.3, #253). The CLI defaults
    /// it to `required` for live providers.
    pub sandbox: crom_core::SandboxMode,
    /// Measured mean per-call sandbox overhead, recorded in results.
    pub sandbox_overhead_ms: Option<f64>,
    /// Nominal pricing used for `cost_usd` and cost caps (#282).
    pub pricing: crom_core::PricingTable,
    /// Per-task nominal cost cap (`--max-cost-usd`); the tighter of this and
    /// a task's `limits.max_cost_usd` applies across all of its attempts.
    pub max_cost_usd: Option<f64>,
    /// Run-level nominal spend cap (`--max-run-cost-usd`). Once reached,
    /// remaining tasks are recorded as `skipped` with a `budget:` reason.
    pub max_run_cost_usd: Option<f64>,
    /// Remaining run-level budget for the task being run. Set by
    /// [`run_tasks`]; callers leave it `None`.
    pub run_cost_remaining: Option<f64>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            provider: ProviderMode::Fake,
            model: None,
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
            repair: crom_core::repair::RepairPolicy::disabled(),
            provider_profiles: Default::default(),
            environment: None,
            workspace_root: None,
            sandbox: crom_core::SandboxMode::Off,
            sandbox_overhead_ms: None,
            pricing: crom_core::PricingTable::builtin(),
            max_cost_usd: None,
            max_run_cost_usd: None,
            run_cost_remaining: None,
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
    /// The model provider failed (HTTP 5xx, auth, connect, stream error).
    provider_errored: bool,
    /// The harness/runtime failed for a reason other than the provider.
    harness_errored: bool,
    /// SDK budget diagnostic code that stopped the run, if any.
    budget_breach: Option<String>,
}

/// SDK diagnostic codes that mean a run budget (not a turn cap) stopped it.
const BUDGET_STOP_CODES: &[&str] = &[
    "budget.max_tool_calls_exceeded",
    "budget.max_input_tokens_exceeded",
    "budget.max_output_tokens_exceeded",
    "budget.max_cost_exceeded",
    "budget.usage_unknown",
    "budget.unsupported_mode",
];

fn budget_breach(diags: &[crom_sdk::Diagnostic]) -> Option<String> {
    diags
        .iter()
        .find(|d| BUDGET_STOP_CODES.contains(&d.code.as_str()))
        .map(|d| d.code.clone())
}

/// The static per-task nominal cost cap: the tighter of the task's
/// `limits.max_cost_usd` and the run config's `max_cost_usd`.
pub fn task_cost_cap(task: &EvalTask, config: &RunConfig) -> Option<f64> {
    match (task.limits.max_cost_usd, config.max_cost_usd) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Cost cap for this task right now: the static cap tightened by the
/// remaining run-level budget.
fn effective_cost_cap(task: &EvalTask, config: &RunConfig) -> Option<f64> {
    match (task_cost_cap(task, config), config.run_cost_remaining) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Run budgets handed to the SDK for one attempt.
fn budgets_for(task: &EvalTask, cost_cap: Option<f64>) -> crom_sdk::RunBudgets {
    let mut b = crom_sdk::RunBudgets::default();
    if let Some(n) = task.limits.max_tool_calls {
        b = b.with_max_tool_calls(u32::try_from(n).unwrap_or(u32::MAX));
    }
    if let Some(c) = cost_cap {
        b = b.with_max_cost_usd(c.max(0.0));
    }
    b
}

/// Nominal cost of the events so far, or why it is unknown.
fn cost_of(config: &RunConfig, usage: Option<&crom_core::TokenUsage>) -> crom_core::CostEstimate {
    config
        .pricing
        .estimate(&config.provider.label(), &provider_model(config), usage)
}

/// Prefix the core agent loop puts on a provider failure's result message.
const PROVIDER_ERROR_PREFIX: &str = "provider error:";

/// Whether SDK diagnostics carry a provider failure.
fn diagnostics_show_provider_error(diags: &[crom_sdk::Diagnostic]) -> bool {
    diags.iter().any(|d| {
        d.code == "model.provider_error"
            || d.code.starts_with("provider.")
            || d.message.starts_with(PROVIDER_ERROR_PREFIX)
    })
}

/// Classify an SDK error as (provider, harness) failure. Budget, timeout,
/// tool, permission, and similar kinds are agent outcomes (neither).
fn classify_sdk_error(err: &crom_sdk::AgentSdkError) -> (bool, bool) {
    use crom_sdk::AgentSdkErrorKind as K;
    if err.kind == K::Provider || diagnostics_show_provider_error(&err.diagnostics) {
        return (true, false);
    }
    let harness = matches!(
        err.kind,
        K::Internal | K::Storage | K::Workspace | K::InvalidRequest
    );
    (false, harness)
}

/// Agent-tool sandbox for a task: network follows `--network` or the task's
/// `requires_network`, all-or-nothing.
pub fn sandbox_config_for(task: &EvalTask, config: &RunConfig) -> crom_core::SandboxConfig {
    crom_core::SandboxConfig {
        mode: config.sandbox,
        network: config.network || task.requires_network,
        ..crom_core::SandboxConfig::default()
    }
}

/// Build the tool policy for a task from its `allowed_tools` list.
fn tool_policy_for(task: &EvalTask, config: &RunConfig) -> ToolPolicy {
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
    let custom_tools = match &config.provider {
        ProviderMode::Scripted(script) => script.tools().to_vec(),
        _ => Vec::new(),
    };
    ToolPolicy {
        enable_builtin_tools: builtins,
        custom_tools,
        side_effect_policy: SideEffectPolicy::full(),
        sandbox: sandbox_config_for(task, config),
        ..Default::default()
    }
}

/// The fixed approval mode the harness runs agents under.
const EVAL_APPROVALS: &str = "allow_all";

/// Drive the agent for a task in the prepared workspace.
async fn drive_agent(
    task: &EvalTask,
    prompt: &str,
    workspace_dir: &std::path::Path,
    config: &RunConfig,
    cost_cap: Option<f64>,
) -> Result<RunOutcome, String> {
    let setup = providers::build(
        &config.provider,
        task,
        &config.provider_profiles,
        config.model.as_deref(),
    )
    .map_err(|u| u.reason)?;
    let price = config
        .pricing
        .lookup(&config.provider.label(), &provider_model(config))
        .map(|e| e.model_pricing());
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(CollectingSink {
        events: events.clone(),
    });

    let mut builder = AgentRuntime::builder()
        .provider(setup.config)
        .model(setup.model)
        .workspace(WorkspaceSource::filesystem(workspace_dir.to_path_buf()))
        .tool_policy(tool_policy_for(task, config))
        .interactivity(InteractivityMode::Headless)
        .approval_callback(Arc::new(AllowAllApprovals))
        .event_sink(sink);
    if let Some(p) = price {
        builder = builder.pricing(p);
    }
    let runtime = builder.build().await;
    let runtime = match runtime {
        Ok(r) => r,
        Err(e) => {
            let (provider_errored, harness_errored) = classify_sdk_error(&e);
            return Ok(RunOutcome {
                events: Vec::new(),
                summary: format!("runtime build failed: {}", e.message),
                timed_out: false,
                errored: true,
                provider_errored,
                harness_errored,
                budget_breach: budget_breach(&e.diagnostics),
            });
        }
    };

    let mut request = AgentRequest::new(prompt.to_string())
        .max_turns(task.max_turns as u32)
        .budgets(budgets_for(task, cost_cap))
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
            provider_errored: false,
            harness_errored: false,
            budget_breach: None,
        }),
        Ok(Err(sdk_err)) => {
            let (provider_errored, harness_errored) = classify_sdk_error(&sdk_err);
            Ok(RunOutcome {
                events: collected,
                summary: format!("run failed: {}", sdk_err.message),
                timed_out: false,
                errored: true,
                provider_errored,
                harness_errored,
                budget_breach: budget_breach(&sdk_err.diagnostics),
            })
        }
        Ok(Ok(run_result)) => {
            use crom_sdk::AgentRunStatus;
            let errored = !matches!(run_result.status, AgentRunStatus::Completed);
            let timed_out = matches!(run_result.status, AgentRunStatus::TimedOut);
            // The SDK plan/act path now natively emits `PlanCreated` and the
            // act phase's mapper-produced SdkEvents (tool calls, messages,
            // denials) into the event sink, so the harness no longer
            // synthesizes a PlanCreated from `run_result.plan` (#34). Approval
            // events remain counted-only (not emitted as SdkEvents) on both
            // the plan/act and regular run paths.
            let provider_errored = matches!(run_result.status, AgentRunStatus::Failed)
                && diagnostics_show_provider_error(&run_result.diagnostics);
            Ok(RunOutcome {
                events: collected,
                summary: run_result.summary,
                timed_out,
                errored,
                provider_errored,
                harness_errored: false,
                budget_breach: budget_breach(&run_result.diagnostics),
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

/// Strict usage total and round-trip count from the per-turn usage events.
fn usage_from_events(events: &[SdkEvent]) -> (Option<crom_core::TokenUsage>, usize) {
    let totals: crom_core::UsageTotals = events
        .iter()
        .filter_map(|e| match e {
            SdkEvent::ModelTurnCompleted(t) => Some(t.usage.as_ref()),
            _ => None,
        })
        .collect();
    (totals.total(), totals.turns() as usize)
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
        schema_version: RESULT_SCHEMA_VERSION,
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
        model: provider_model(config),
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
        cost_unknown_reason: None,
        budget_exceeded: None,
        repair: crate::repair::RepairMetrics::default(),
        usage: None,
        model_round_trips: 0,
        kind: task.kind,
        level: task.level,
        outcome: None,
        repro: None,
    }
}

/// Stamp the v2 outcome and reproducibility metadata onto a finished result.
/// Call immediately before writing `result.json`.
fn stamp_v2(r: &mut EvalResult, task: &EvalTask, config: &RunConfig, prompt: &str) {
    let env = config
        .environment
        .clone()
        .unwrap_or_else(RunEnvironment::gather);
    let policy = tool_policy_for(task, config);
    r.schema_version = RESULT_SCHEMA_VERSION;
    r.outcome = Some(Outcome::derive(r.status, r.resolved, r.failure_class));
    r.repro = Some(ReproMetadata::build(ReproInputs {
        task,
        config,
        env: &env,
        prompt,
        model: provider_model(config),
        base_url_host: endpoint_host(config),
        builtin_tools: serde_json::to_value(policy.enable_builtin_tools).unwrap_or_default(),
        side_effects: serde_json::to_value(policy.side_effect_policy).unwrap_or_default(),
        approvals: EVAL_APPROVALS,
    }));
}

fn provider_model(config: &RunConfig) -> String {
    match &config.provider {
        ProviderMode::Fake => "fake-echo".to_string(),
        ProviderMode::Golden => "golden".to_string(),
        ProviderMode::Fault(_) => "fault".to_string(),
        ProviderMode::Scripted(_) => "scripted".to_string(),
        ProviderMode::Live(name) => {
            providers::resolve_live_profile(name, &config.provider_profiles)
                .ok()
                .and_then(|p| providers::live_model(&p, config.model.as_deref()))
                .unwrap_or_default()
        }
    }
}

/// Provider run metadata: id, model, and endpoint host. Never the API key
/// or the full base URL.
fn provider_metadata(config: &RunConfig) -> serde_json::Value {
    serde_json::json!({
        "provider": config.provider.label(),
        "model": provider_model(config),
        "endpoint_host": endpoint_host(config),
    })
}

/// Run-level `run.json` (schema v2). Per-task reproducibility metadata lives
/// in each `result.json` `repro` block; this records the shared run inputs.
pub fn run_metadata(config: &RunConfig, repetitions: usize) -> serde_json::Value {
    let env = config
        .environment
        .clone()
        .unwrap_or_else(RunEnvironment::gather);
    serde_json::json!({
        "schema_version": RESULT_SCHEMA_VERSION,
        "adapter": config.adapter,
        "provider": config.provider.label(),
        "model": provider_model(config),
        "base_url_host": endpoint_host(config),
        "isolation": config.isolation.id(),
        "network": config.network,
        "cpu_limit": config.cpu_limit,
        "memory_limit": config.memory_limit,
        "container_image": config.container_image,
        "repetitions": repetitions,
        "attempts": config.attempts,
        "prompt_variant": config.prompt_variant,
        "prompt_template_version": crate::result_schema::PROMPT_TEMPLATE_VERSION,
        "seed": config.seed,
        "repair": {
            "enabled": config.repair.enabled,
            "max_attempts": config.repair.max_attempts,
            "require_validation_after_mutation": config.repair.require_validation_after_mutation,
        },
        "git": env.git,
        "host": env.host,
        "crom_version": env.crom_version,
        "pricing_source_version": config.pricing.version,
        "max_cost_usd": config.max_cost_usd,
        "max_run_cost_usd": config.max_run_cost_usd,
        "agent_sandbox": {
            "mode": config.sandbox.as_str(),
            "overhead_ms": config.sandbox_overhead_ms,
        },
    })
}

/// Endpoint host of a live provider profile (never the full URL).
fn endpoint_host(config: &RunConfig) -> Option<String> {
    match &config.provider {
        ProviderMode::Live(name) => {
            providers::resolve_live_profile(name, &config.provider_profiles)
                .ok()
                .and_then(|p| p.base_url_host())
        }
        _ => None,
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
            let prompt = task.resolved_prompt(config.prompt_variant.as_deref(), config.seed);
            stamp_v2(&mut r, task, config, &prompt);
            return r;
        }
    };

    // Resolve the prompt (variant selection) and persist it + the manifest.
    let prompt = task.resolved_prompt(config.prompt_variant.as_deref(), config.seed);
    let _ = persist_manifest(task, &artifacts);
    let _ = artifacts.write_string("prompt.md", &prompt);
    write_metadata_artifacts(&artifacts, task, config);

    // Grader material (above) lives in the artifact dir; the agent works in a
    // separate temp workspace that is moved here only after it exits (#286).
    let verification_dir = artifacts.workspace_dir();

    // Provider availability is checked first so skips are clean and cheap.
    if let Err(unavailable) = providers::build(
        &config.provider,
        task,
        &config.provider_profiles,
        config.model.as_deref(),
    ) {
        return skipped_result(
            task,
            &artifacts,
            config,
            unavailable.reason,
            started,
            &prompt,
        );
    }

    // The SDK does not meter usage in plan modes, so a cost cap there could
    // not be enforced: skip instead of running unmetered.
    if task.mode == TaskMode::PlanThenAct && effective_cost_cap(task, config).is_some() {
        return skipped_result(
            task,
            &artifacts,
            config,
            "budget: cost caps are not enforced in plan_then_act mode".to_string(),
            started,
            &prompt,
        );
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
            stamp_v2(&mut r, task, config, &prompt);
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

    let ws_root = config
        .workspace_root
        .clone()
        .unwrap_or_else(crate::workspace::default_root);
    let runs_root = run_dir.root.parent().unwrap_or(&run_dir.root);
    let tasks_root = task.dir.parent().unwrap_or(&task.dir);
    let agent_ws = AgentWorkspace::create(&ws_root, &[runs_root, tasks_root])
        .map_err(|e| crate::fixture::FixtureError::Io(e.to_string()));
    let prepared = agent_ws.and_then(|ws| prepare_workspace(task, ws.path()).map(|()| ws));
    let agent_ws = match prepared {
        Ok(ws) => ws,
        Err(e) => {
            let _ = artifacts.write_string(
                "final_summary.md",
                &format!("# {}\n\nworkspace setup failed: {e}\n", task.id),
            );
            let mut r = default_result(task, config, EvalStatus::Errored, &dir);
            r.score = EvalScore::zero(format!("errored: workspace setup: {e}"));
            r.failure_class = Some(FailureClass::HarnessError);
            r.duration_ms = started.elapsed().as_millis() as u64;
            stamp_v2(&mut r, task, config, &prompt);
            let _ = artifacts.write_json("result.json", &r);
            return r;
        }
    };
    let workspace_dir = agent_ws.path().to_path_buf();

    // Drive the agent, optionally retrying with test feedback (Aider-style).
    let mut events: Vec<SdkEvent> = Vec::new();
    let mut summary = String::new();
    let mut timed_out = false;
    let mut errored = false;
    let mut provider_errored = false;
    let mut harness_errored = false;
    let mut public_report = OracleReport {
        commands: Vec::new(),
        assertions: Vec::new(),
        behavior: Vec::new(),
    };
    // The repair loop bounds attempts by the repair policy when enabled,
    // otherwise by the Aider-style `attempts` budget. A `RepairSession` always
    // observes each validation so diagnostics/metrics are recorded even when
    // the repair loop itself is disabled.
    let repair_enabled = config.repair.enabled;
    let max_attempts = if repair_enabled {
        config.repair.max_attempts.max(1) as usize
    } else {
        config.attempts.max(1)
    };
    let mut repair_session = crate::repair::RepairSession::new(config.repair.clone());
    let mut attempts_used = 0usize;
    let mut feedback = String::new();
    let cost_cap = effective_cost_cap(task, config);
    let mut budget_breach_code: Option<String> = None;

    for attempt in 0..max_attempts {
        attempts_used += 1;
        let attempt_prompt = if attempt == 0 {
            prompt.clone()
        } else {
            feedback.clone()
        };
        // Retries share the task's cost cap: each attempt gets what earlier
        // attempts left over.
        let attempt_cap = cost_cap.map(|cap| {
            let spent = cost_of(config, usage_from_events(&events).0.as_ref())
                .cost_usd
                .unwrap_or(0.0);
            (cap - spent).max(0.0)
        });
        match drive_agent(task, &attempt_prompt, &workspace_dir, config, attempt_cap).await {
            Ok(o) => {
                events.extend(o.events);
                summary = o.summary;
                timed_out |= o.timed_out;
                errored = o.errored;
                provider_errored = o.provider_errored;
                harness_errored = o.harness_errored;
                if o.budget_breach.is_some() {
                    budget_breach_code = o.budget_breach;
                }
            }
            Err(reason) => {
                summary = reason;
                errored = true;
                harness_errored = true;
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
        if public_report.all_passed() {
            repair_session.record_pass(&public_report, &workspace_dir);
            break;
        }
        // No retry once a budget stopped the agent, or when the cost cap can
        // no longer be checked because earlier spend is unknown.
        let spend_unknown = cost_cap.is_some()
            && cost_of(config, usage_from_events(&events).0.as_ref())
                .cost_usd
                .is_none();
        let is_last = attempt + 1 == max_attempts || budget_breach_code.is_some() || spend_unknown;
        let outcome = repair_session.record_failure(&public_report, &workspace_dir);
        if is_last {
            break;
        }
        feedback = if repair_enabled {
            if outcome.stop {
                break;
            }
            outcome
                .feedback
                .unwrap_or_else(|| feedback_prompt(&prompt, &public_report))
        } else {
            feedback_prompt(&prompt, &public_report)
        };
    }

    // Finalize repair trace + metrics from the accumulated diagnostics. Capture
    // the raw diagnostics + validation attempts for artifact emission first
    // (finish consumes the session).
    let repair_diagnostics = repair_session.diagnostics.clone();
    let repair_validation_attempts = repair_session.validation_attempts.clone();
    let (repair_trace, repair_metrics) = repair_session.finish(public_report.all_passed(), &events);
    // Emit repair artifacts (M16L). Raw command logs remain in the existing
    // command-output artifacts; these add structured summaries.
    if !repair_trace.attempts.is_empty() || !repair_diagnostics.is_empty() {
        let _ = artifacts.write_json("repair.json", &repair_trace);
        let _ =
            artifacts.write_string("repair.md", &crate::repair::render_repair_md(&repair_trace));
        let _ = artifacts.write_jsonl("diagnostics.jsonl", &repair_diagnostics);
        let _ = artifacts.write_jsonl("validation-attempts.jsonl", &repair_validation_attempts);
    }

    // Generate the public patch (before any hidden overlay).
    let patch = match generate_patch(&workspace_dir) {
        Ok(p) => p,
        Err(e) => {
            errored = true;
            harness_errored = true;
            summary = format!("{summary}; patch generation failed: {e}");
            PatchInfo::default()
        }
    };

    // The agent has exited: move its workspace into the artifact bundle as the
    // verification copy. Hidden material is overlaid only onto this copy.
    let workspace_dir = match agent_ws.into_verification_copy(&verification_dir) {
        Ok(p) => p,
        Err(e) => {
            errored = true;
            harness_errored = true;
            summary = format!("{summary}; moving workspace to verification copy failed: {e}");
            verification_dir
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
        && budget_breach_code.is_none()
        && !rule_violation;

    // Failure classification.
    let signals = FailureSignals {
        timed_out,
        provider_unavailable: provider_errored,
        harness_errored,
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
    let (mut failure_class, mut secondary_failures) = if resolved {
        (None, Vec::new())
    } else {
        match failure::classify(&signals) {
            Some((p, s)) => (Some(p), s),
            // Resolved disagreement (e.g. behavior-rule enforcement): default.
            None => (Some(FailureClass::PublicTestFailure), Vec::new()),
        }
    };
    // M16: refine the primary class with repair-loop signals. The original
    // class is demoted to a secondary label so existing taxonomy is preserved.
    // Provider/harness failures are not agent failures and are never refined.
    let infra_failure = matches!(
        failure_class,
        Some(FailureClass::ProviderError | FailureClass::HarnessError)
    );
    if !resolved && !infra_failure {
        let refined = repair_refined_class(&repair_metrics);
        if let Some(refined) = refined {
            if let Some(prev) = failure_class {
                if prev != refined && !secondary_failures.contains(&prev) {
                    secondary_failures.push(prev);
                }
            }
            failure_class = Some(refined);
        }
    }

    // A budget stop is the primary failure; the signal-derived class is kept
    // as a secondary label.
    if budget_breach_code.is_some() && !resolved {
        if let Some(prev) = failure_class {
            if prev != FailureClass::BudgetExceeded && !secondary_failures.contains(&prev) {
                secondary_failures.push(prev);
            }
        }
        failure_class = Some(FailureClass::BudgetExceeded);
    }

    let turns_used = count_turns(&events);
    let tool_calls = count_tool_calls(&events);
    let (usage, model_round_trips) = usage_from_events(&events);
    let cost = cost_of(config, usage.as_ref());
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
        provider_errored,
        harness_errored,
        budget_breach: budget_breach_code.clone(),
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
    result.repair = repair_metrics;
    result.usage = usage;
    result.model_round_trips = model_round_trips;
    result.cost_usd = cost.cost_usd;
    result.cost_unknown_reason = cost.unknown_reason.map(|r| r.key().to_string());
    result.budget_exceeded = budget_breach_code;
    stamp_v2(&mut result, task, config, &prompt);

    let _ = artifacts.write_json("result.json", &result);
    result
}

/// Map repair-loop metrics to a refined failure class for an unresolved task.
/// Returns `None` when no repair-specific signal applies (keep the original
/// class). Priority: repeated-failure > limit-exceeded > unrecovered family >
/// unvalidated completion.
fn repair_refined_class(m: &crate::repair::RepairMetrics) -> Option<FailureClass> {
    if m.same_error_repeated && m.repair_attempt_count > 0 {
        return Some(FailureClass::RepeatedSameFailure);
    }
    if m.repair_attempt_limit_exceeded {
        return Some(FailureClass::RepairAttemptLimitExceeded);
    }
    if m.compiler_error_recovered == Some(false) {
        return Some(FailureClass::UnrecoveredCompileError);
    }
    if m.test_failure_recovered == Some(false) {
        return Some(FailureClass::UnrecoveredTestFailure);
    }
    if m.final_answer_without_validation {
        return Some(FailureClass::UnvalidatedCompletion);
    }
    None
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
    let _ = a.write_jsonl(
        "security.jsonl",
        &crate::security::security_records(&w.outcome.events),
    );
    let _ = a.write_json(
        "security-summary.json",
        &crate::security::summarize(&w.outcome.events),
    );

    let _ = crate::artifacts::write_redacted(&a.final_patch_path(), &w.patch.unified_diff);
    let _ = crate::artifacts::write_redacted(&a.final_diff_path(), &w.patch.stat);
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
    let _ = artifacts.write_json("provider.json", &provider_metadata(config));
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
    prompt: &str,
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
    stamp_v2(&mut r, task, config, prompt);
    let _ = artifacts.write_json("result.json", &r);
    r
}

/// Run a selection of tasks sequentially, returning their results.
pub async fn run_tasks(
    tasks: &[EvalTask],
    run_dir: &RunDir,
    config: &RunConfig,
) -> Vec<EvalResult> {
    run_tasks_metered(tasks, run_dir, config, &mut SpendLedger::default()).await
}

/// Nominal spend across a whole `crom eval run` (all repetitions).
#[derive(Debug, Default)]
struct SpendLedger {
    spent: f64,
    /// First task whose cost is unknown, with the reason.
    unknown: Option<(String, String)>,
}

impl SpendLedger {
    fn record(&mut self, r: &EvalResult) {
        if r.status == EvalStatus::Skipped {
            return;
        }
        match r.cost_usd {
            Some(c) => self.spent += c,
            None => {
                self.unknown.get_or_insert_with(|| {
                    (
                        r.task_id.clone(),
                        r.cost_unknown_reason.clone().unwrap_or_default(),
                    )
                });
            }
        }
    }

    /// Why the run-level cap forbids starting another task, if it does.
    /// Fail-closed: an unpriced model or an earlier task with unknown cost
    /// stops scheduling, since the cap could not be checked.
    fn stop_reason(&self, config: &RunConfig, cap: f64) -> Option<String> {
        let (label, model) = (config.provider.label(), provider_model(config));
        if config.pricing.lookup(&label, &model).is_none() {
            return Some(format!(
                "budget: no pricing entry for {label}/{model}; --max-run-cost-usd cannot be enforced"
            ));
        }
        if let Some((task, reason)) = &self.unknown {
            return Some(format!(
                "budget: cost of {task} is unknown ({reason}); --max-run-cost-usd cannot be enforced"
            ));
        }
        if self.spent >= cap {
            return Some(format!(
                "budget: run spend ${:.4} reached --max-run-cost-usd ${cap:.4}",
                self.spent
            ));
        }
        None
    }
}

async fn run_tasks_metered(
    tasks: &[EvalTask],
    run_dir: &RunDir,
    config: &RunConfig,
    ledger: &mut SpendLedger,
) -> Vec<EvalResult> {
    let mut config = config.clone();
    if config.environment.is_none() {
        config.environment = Some(RunEnvironment::gather());
    }
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        let Some(cap) = config.max_run_cost_usd else {
            results.push(run_task(task, run_dir, &config).await);
            continue;
        };
        if let Some(reason) = ledger.stop_reason(&config, cap) {
            results.push(budget_skipped(task, run_dir, &config, reason));
            continue;
        }
        let mut task_config = config.clone();
        task_config.run_cost_remaining = Some(cap - ledger.spent);
        let result = run_task(task, run_dir, &task_config).await;
        ledger.record(&result);
        results.push(result);
    }
    results
}

/// A `skipped(budget)` result for a task the run-level cap kept from starting.
fn budget_skipped(
    task: &EvalTask,
    run_dir: &RunDir,
    config: &RunConfig,
    reason: String,
) -> EvalResult {
    let started = Instant::now();
    let prompt = task.resolved_prompt(config.prompt_variant.as_deref(), config.seed);
    match run_dir.task(&task.id) {
        Ok(artifacts) => skipped_result(task, &artifacts, config, reason, started, &prompt),
        Err(_) => {
            let dir = run_dir.root.join("tasks").join(&task.id);
            let mut r = default_result(task, config, EvalStatus::Skipped, &dir);
            r.score = EvalScore::zero(format!("skipped: {reason}"));
            r.skipped_reason = Some(reason);
            stamp_v2(&mut r, task, config, &prompt);
            r
        }
    }
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
    // `--max-run-cost-usd` caps the whole run, across repetitions.
    let mut ledger = SpendLedger::default();
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
        let results = run_tasks_metered(tasks, &sub, &rep_config, &mut ledger).await;
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
            schema_version: RESULT_SCHEMA_VERSION,
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
            cost_unknown_reason: None,
            budget_exceeded: None,
            repair: crate::repair::RepairMetrics::default(),
            usage: None,
            model_round_trips: 0,
            outcome: None,
            repro: None,
            kind: None,
            level: None,
        }
    }
}

#[cfg(test)]
mod provider_metadata_tests {
    use super::*;

    /// run.json v2 records schema version, seed, budgets, git, host, and build version.
    #[test]
    fn run_metadata_is_v2_with_reproducibility_fields() {
        let config = RunConfig {
            provider: ProviderMode::Golden,
            seed: 42,
            environment: Some(RunEnvironment {
                git: crate::result_schema::GitMeta {
                    sha: Some("abc".into()),
                    dirty: true,
                },
                host: crate::result_schema::HostInfo::gather(),
                crom_version: "9.9.9".into(),
            }),
            ..RunConfig::default()
        };
        let meta = run_metadata(&config, 3);
        assert_eq!(meta["schema_version"], RESULT_SCHEMA_VERSION);
        assert_eq!(meta["seed"], 42);
        assert_eq!(meta["repetitions"], 3);
        assert_eq!(meta["model"], "golden");
        assert_eq!(meta["git"]["sha"], "abc");
        assert_eq!(meta["git"]["dirty"], true);
        assert_eq!(meta["crom_version"], "9.9.9");
        assert!(meta["host"]["cpus"].as_u64().unwrap() > 0);
        assert_eq!(
            meta["pricing_source_version"],
            crom_core::PricingTable::builtin().version.as_str()
        );
        assert!(meta["max_run_cost_usd"].is_null());
        assert_eq!(meta["repair"]["enabled"], false);
    }

    /// provider.json for a live profile records the endpoint host and model, never a key or full URL.
    #[test]
    fn live_provider_metadata_records_host_only() {
        let mut config = RunConfig {
            provider: ProviderMode::Live("deepseek".into()),
            ..RunConfig::default()
        };
        config.provider_profiles.insert(
            "deepseek".into(),
            serde_json::from_value(serde_json::json!({
                "base_url": "https://user:pw@ds.example:8443/proxy",
                "default_model": "deepseek-chat"
            }))
            .unwrap(),
        );
        let meta = provider_metadata(&config);
        assert_eq!(meta["provider"], "deepseek");
        assert_eq!(meta["endpoint_host"], "ds.example:8443");
        let text = meta.to_string();
        assert!(!text.contains("pw") && !text.contains("/proxy"), "{text}");
    }
}

#[cfg(test)]
mod sandbox_policy_tests {
    use super::*;
    use crom_core::SandboxMode;

    fn task(requires_network: bool) -> EvalTask {
        let mut t = EvalTask::minimal(
            "t",
            "t",
            "p",
            crate::task::OracleSpec::default(),
            PathBuf::from("."),
        );
        t.requires_network = requires_network;
        t
    }

    fn repro(config: &RunConfig) -> ReproMetadata {
        ReproMetadata::build(ReproInputs {
            task: &task(false),
            config,
            env: &RunEnvironment::gather(),
            prompt: "p",
            model: "m".into(),
            base_url_host: None,
            builtin_tools: serde_json::Value::Null,
            side_effects: serde_json::Value::Null,
            approvals: "allow_all",
        })
    }

    // The run's sandbox mode reaches the agent tool policy; network stays off unless granted.
    #[test]
    fn tool_policy_carries_required_sandbox() {
        let config = RunConfig {
            sandbox: SandboxMode::Required,
            ..RunConfig::default()
        };
        let policy = tool_policy_for(&task(false), &config);
        assert_eq!(policy.sandbox.mode, SandboxMode::Required);
        assert!(!policy.sandbox.network);
    }

    // A task that requires network, or a run with --network, grants sandbox networking.
    #[test]
    fn tool_policy_sandbox_network_follows_task_and_run() {
        let config = RunConfig {
            sandbox: SandboxMode::Required,
            ..RunConfig::default()
        };
        assert!(tool_policy_for(&task(true), &config).sandbox.network);
        let networked = RunConfig {
            network: true,
            ..config
        };
        assert!(tool_policy_for(&task(false), &networked).sandbox.network);
    }

    // The default run config leaves agent tools unsandboxed (offline providers).
    #[test]
    fn tool_policy_sandbox_defaults_off() {
        let policy = tool_policy_for(&task(false), &RunConfig::default());
        assert_eq!(policy.sandbox.mode, SandboxMode::Off);
    }

    // Results record the agent sandbox backend and the measured per-call overhead.
    #[test]
    fn repro_records_sandbox_and_overhead() {
        let config = RunConfig {
            sandbox: SandboxMode::Required,
            sandbox_overhead_ms: Some(2.5),
            ..RunConfig::default()
        };
        let r = repro(&config);
        assert_eq!(r.security.agent_sandbox, "landlock");
        assert_eq!(r.security.agent_sandbox_overhead_ms, Some(2.5));
        let off = repro(&RunConfig::default());
        assert_eq!(off.security.agent_sandbox, "off");
        assert_eq!(off.security.agent_sandbox_overhead_ms, None);
    }

    // run.json records the agent sandbox mode and overhead.
    #[test]
    fn run_metadata_records_sandbox() {
        let config = RunConfig {
            sandbox: SandboxMode::Required,
            sandbox_overhead_ms: Some(1.25),
            environment: Some(RunEnvironment::gather()),
            ..RunConfig::default()
        };
        let meta = run_metadata(&config, 1);
        assert_eq!(meta["agent_sandbox"]["mode"], "required");
        assert_eq!(meta["agent_sandbox"]["overhead_ms"], 1.25);
    }
}
