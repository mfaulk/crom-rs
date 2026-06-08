//! The built-in `Agent` tool plus the `SubagentRuntime` it delegates to.
//!
//! The Agent tool is registered with the parent agent's `ToolRegistry` only
//! when:
//! 1. `AgentRuntimeOptions::enabled` is true, and
//! 2. The parent's `allowed_tools` (or an explicit allow rule) includes
//!    the `Agent` name.
//!
//! When the parent invokes the Agent tool, this module:
//! - Validates the requested subagent exists.
//! - Fires `SubagentStart` hooks (which can deny / modify the task).
//! - Acquires a slot from a bounded `Semaphore`.
//! - Spawns a fresh `Agent` (from `crate::agent`) with isolated state.
//! - Streams events into a per-subagent session sink (and optionally
//!   forwards them to the parent stream when the visibility setting allows).
//! - Returns a concise `AgentToolResult` to the parent.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::time::timeout;

use crate::agent::{run_loop, AgentBuilder, RunConfig, SessionSink, TelemetrySink};
use crate::errors::ToolError;
use crate::events::{
    AgentEvent, SubagentCancelled, SubagentEvent as SubagentForwardedEvent, SubagentFailed,
    SubagentStart, SubagentStop, SubagentTimedOut,
};
use crate::hooks::{aggregate, HookAggregateDecision, HookEventName, HookInput, Hooks};
use crate::options::{AgentOptions, SessionId};
use crate::permission::{ApprovalMemoryMode, PermissionKind, PermissionMode};
use crate::policy::ApprovalMemory;
use crate::provider::ModelProvider;
use crate::run_id::RunId;
use crate::telemetry::{AgentTelemetryEvent, ModelPricing, TelemetryOutcome};
use crate::tool::{Tool, ToolContext, ToolRegistry, ToolResult};
use crate::tool_rule::ToolRule;
use crate::usage::TokenUsage;

use super::definition::AgentDefinition;
use super::registry::AgentRegistry;
use super::runtime::{
    AgentRuntimeOptions, SubagentCheckpointPolicy, SubagentEventVisibility, SubagentStatus,
};

/// The canonical name of the built-in Agent tool. Subagent definitions are
/// rejected if they reference this name (no recursion in M6).
pub const SUBAGENT_TOOL_NAME: &str = "Agent";

/// Input shape accepted by the Agent tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolInput {
    pub agent: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
}

/// Result returned to the parent for one subagent invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolResult {
    pub agent_name: String,
    pub agent_run_id: RunId,
    pub subagent_session_id: SessionId,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
    pub tool_calls: u32,
    pub duration_ms: u64,
    pub status: SubagentStatus,
}

/// Bookkeeping for a single subagent invocation. Useful for tests and the
/// CLI's "agents inspect" command. Held in a mutex inside the runtime so
/// the parent can list children currently in flight.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentRunHandle {
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: String,
    pub subagent_session_id: SessionId,
    pub subagent_run_id: RunId,
    pub agent_name: String,
    pub status: SubagentStatus,
}

/// Channel-backed sink that captures per-child events for routing into
/// either a session sink (always) or a parent event stream (only in
/// `StreamChildEvents` mode).
struct ChildSessionSink {
    inner: Option<Arc<dyn SessionSink>>,
    parent_tx: Option<mpsc::Sender<AgentEvent>>,
    visibility: SubagentEventVisibility,
    agent_name: String,
    counter_tool_calls: Arc<AtomicU32>,
    last_text: Arc<Mutex<Option<String>>>,
    usage: Arc<Mutex<Option<TokenUsage>>>,
    /// The terminal `AgentResult::subtype` reported by the child loop.
    /// Used to derive `SubagentStatus` accurately — `JoinHandle` completion
    /// alone does not distinguish success from internal failure because the
    /// child loop catches and emits provider/tool errors as Result events.
    final_subtype: Arc<Mutex<Option<crate::events::ResultSubtype>>>,
}

#[async_trait]
impl SessionSink for ChildSessionSink {
    async fn record(
        &self,
        session_id: &SessionId,
        event: &AgentEvent,
    ) -> Result<(), crate::errors::AgentError> {
        // Tally bookkeeping for the result that will be returned to the
        // parent.
        match event {
            AgentEvent::ToolCallCompleted(_) => {
                self.counter_tool_calls.fetch_add(1, Ordering::Release);
            }
            AgentEvent::AssistantMessage(m) => {
                let mut buf = String::new();
                for block in &m.content {
                    if let crate::conversation::AssistantContent::Text { text } = block {
                        if !buf.is_empty() {
                            buf.push('\n');
                        }
                        buf.push_str(text);
                    }
                }
                if !buf.is_empty() {
                    *self.last_text.lock().await = Some(buf);
                }
            }
            AgentEvent::Result(r) => {
                if let Some(u) = &r.usage {
                    *self.usage.lock().await = Some(u.clone());
                }
                if let Some(text) = &r.result {
                    if !text.is_empty() {
                        *self.last_text.lock().await = Some(text.clone());
                    }
                }
                *self.final_subtype.lock().await = Some(r.subtype);
            }
            _ => {}
        }
        if let Some(inner) = &self.inner {
            inner.record(session_id, event).await?;
        }
        if matches!(self.visibility, SubagentEventVisibility::StreamChildEvents) {
            if let Some(tx) = &self.parent_tx {
                let forwarded = SubagentForwardedEvent {
                    agent_name: self.agent_name.clone(),
                    subagent_session_id: session_id.clone(),
                    event: Box::new(event.clone()),
                };
                let _ = tx.send(AgentEvent::SubagentEvent(forwarded)).await;
            }
        }
        Ok(())
    }
}

/// Compact bundle of identifiers + agent metadata an implementor of
/// [`ChildSessionSinkFactory`] may want to record at the head of the
/// child session JSONL (M6 `SubagentMetadata` payload). Optional —
/// implementors that do not persist linkage records can ignore it.
#[derive(Debug, Clone)]
pub struct ChildSessionMetadata {
    pub parent_session_id: SessionId,
    pub parent_run_id: RunId,
    pub parent_tool_call_id: String,
    pub agent_name: String,
}

/// Factory used by [`SubagentRuntime`] to mint a session sink for one
/// child run. Implementations route writes to a child-specific JSONL file
/// (e.g. `.agent/sessions/children/<parent>/<child>.jsonl`).
pub trait ChildSessionSinkFactory: Send + Sync {
    fn child_sink(
        &self,
        parent_session_id: &SessionId,
        child_session_id: &SessionId,
    ) -> Arc<dyn SessionSink>;

    /// Optional hook: persist a linkage record (e.g. JSONL line of
    /// `SubagentMetadata`) at the head of the child session file. The
    /// default is a no-op so non-filesystem implementations don't have
    /// to care.
    fn write_metadata(
        &self,
        _parent_session_id: &SessionId,
        _child_session_id: &SessionId,
        _metadata: &ChildSessionMetadata,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(), crate::errors::AgentError>> + Send + '_>,
    > {
        Box::pin(async { Ok(()) })
    }
}

/// Shared dependencies the orchestrator needs to spin up subagents. Held by
/// the AgentTool as `Arc<SubagentRuntime>` and configured by the
/// `AgentClientBuilder` / `AgentBuilder`.
pub struct SubagentRuntime {
    pub registry: Arc<AgentRegistry>,
    pub provider: Arc<dyn ModelProvider>,
    pub base_tools: ToolRegistry,
    pub parent_options: AgentOptions,
    pub parent_hooks: Hooks,
    pub session_sink: Option<Arc<dyn SessionSink>>,
    pub child_sink_factory: Option<Arc<dyn ChildSessionSinkFactory>>,
    pub telemetry_sink: Option<Arc<dyn TelemetrySink>>,
    pub pricing: Option<ModelPricing>,
    /// Parent's checkpoint handle. When set, the runtime can forward it to
    /// child runs per [`SubagentCheckpointPolicy`] so mutating subagents
    /// participate in the parent's rewind lineage.
    pub parent_checkpoint_handle: Option<Arc<dyn crate::checkpoint::CheckpointHandle>>,
    pub runtime_options: AgentRuntimeOptions,
    pub semaphore: Arc<Semaphore>,
    pub parent_event_tx: Mutex<Option<mpsc::Sender<AgentEvent>>>,
    /// `std::sync::Mutex` (not tokio) so `ActiveEntryGuard::drop` can
    /// lock-and-remove synchronously without spawning a detached task that
    /// may never run during runtime shutdown (#215). The critical sections
    /// are tiny (push/retain/clone) and never held across `.await`.
    pub active: std::sync::Mutex<Vec<SubagentRunHandle>>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl SubagentRuntime {
    pub fn new(
        registry: Arc<AgentRegistry>,
        provider: Arc<dyn ModelProvider>,
        base_tools: ToolRegistry,
        parent_options: AgentOptions,
        parent_hooks: Hooks,
        runtime_options: AgentRuntimeOptions,
    ) -> Arc<Self> {
        let permits = runtime_options.max_concurrent_subagents.max(1);
        Arc::new(Self {
            registry,
            provider,
            base_tools,
            parent_options,
            parent_hooks,
            session_sink: None,
            child_sink_factory: None,
            telemetry_sink: None,
            pricing: None,
            parent_checkpoint_handle: None,
            runtime_options,
            semaphore: Arc::new(Semaphore::new(permits)),
            parent_event_tx: Mutex::new(None),
            active: std::sync::Mutex::new(Vec::new()),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Attach the parent's checkpoint handle so mutating subagents can
    /// participate in the parent's rewind lineage when
    /// [`SubagentCheckpointPolicy::InheritParent`] or
    /// [`SubagentCheckpointPolicy::RequiredForMutatingTools`] is configured.
    pub fn with_checkpoint_handle(
        mut self: Arc<Self>,
        handle: Arc<dyn crate::checkpoint::CheckpointHandle>,
    ) -> Arc<Self> {
        if let Some(this) = Arc::get_mut(&mut self) {
            this.parent_checkpoint_handle = Some(handle);
        }
        self
    }

    /// Replace shared sinks/pricing on an existing runtime (used after
    /// `Arc::new` when these are decided later in the builder).
    pub fn with_sinks(
        mut self: Arc<Self>,
        session_sink: Option<Arc<dyn SessionSink>>,
        telemetry_sink: Option<Arc<dyn TelemetrySink>>,
        pricing: Option<ModelPricing>,
    ) -> Arc<Self> {
        // Cheap-clone an Arc<Self> that we own exclusively; if there are
        // outstanding references this falls back to making a fresh runtime.
        let owned = Arc::get_mut(&mut self);
        if let Some(this) = owned {
            this.session_sink = session_sink;
            this.telemetry_sink = telemetry_sink;
            this.pricing = pricing;
            self
        } else {
            // Should not happen during normal construction.
            self
        }
    }

    /// Set the factory used to mint a per-child session sink. Optional;
    /// when unset, child events go to the parent's sink under the parent's
    /// session id, which is suboptimal but never lossy.
    pub fn with_child_sink_factory(
        mut self: Arc<Self>,
        factory: Arc<dyn ChildSessionSinkFactory>,
    ) -> Arc<Self> {
        if let Some(this) = Arc::get_mut(&mut self) {
            this.child_sink_factory = Some(factory);
        }
        self
    }

    /// Set the parent's event sender so subagent lifecycle events can stream
    /// into the parent UI.
    pub async fn set_parent_event_tx(self: &Arc<Self>, tx: Option<mpsc::Sender<AgentEvent>>) {
        *self.parent_event_tx.lock().await = tx;
    }

    /// Snapshot the list of currently active subagents.
    pub async fn active_runs(self: &Arc<Self>) -> Vec<SubagentRunHandle> {
        self.active.lock().expect("active mutex poisoned").clone()
    }

    /// Trip the global cancel flag. Newly spawned children check this at
    /// startup; in-flight children are still cancelled by the per-run
    /// cancel notification provided to `run_loop`.
    pub async fn cancel_all(self: &Arc<Self>, reason: &str) {
        self.cancel_flag.store(true, Ordering::Release);
        let _ = reason;
    }
}

/// The built-in Agent tool.
pub struct AgentTool {
    runtime: Arc<SubagentRuntime>,
}

impl AgentTool {
    pub fn new(runtime: Arc<SubagentRuntime>) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        SUBAGENT_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Delegate a focused subtask to a registered specialist subagent. \
         The subagent runs in a fresh isolated conversation with its own \
         scoped tools and returns only its final result. Subagents cannot \
         invoke `Agent` recursively."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Registered subagent name (e.g. \"security-reviewer\").",
                },
                "task": {
                    "type": "string",
                    "description": "The task prompt the subagent should focus on.",
                },
                "context": {
                    "type": "object",
                    "description": "Optional structured context (file paths, notes) the parent passes verbatim to the subagent.",
                },
                "max_turns": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 50,
                    "description": "Optional override of the subagent's max_turns (capped at 50).",
                },
            },
            "required": ["agent", "task"],
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::Orchestration
    }

    fn approval_summary(&self, input: &Value) -> String {
        let agent = input
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let task = input
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("(no task)");
        let truncated: String = task.chars().take(120).collect();
        format!("Agent({}, task=\"{}\")", agent, truncated)
    }

    async fn execute(&self, input: Value, ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let parsed: AgentToolInput = serde_json::from_value(input.clone())
            .map_err(|e| ToolError::InvalidInput(format!("invalid Agent input: {e}")))?;
        if !self.runtime.runtime_options.enabled {
            return Ok(ToolResult::error_text(
                "Agent tool is disabled (agent_runtime.enabled = false)",
            ));
        }
        let definition = match self.runtime.registry.get(&parsed.agent) {
            Some(d) => d.clone(),
            None => {
                return Ok(ToolResult::error_text(format!(
                    "unknown subagent {:?} — registered: {}",
                    parsed.agent,
                    self.runtime.registry.names().join(", ")
                )));
            }
        };
        let parent_run_id = ctx
            .run_id
            .clone()
            .ok_or_else(|| ToolError::other("Agent tool requires a parent run"))?;
        let parent_session_id = ctx.session_id.clone();
        let parent_tool_call_id = ctx
            .tool_call_id
            .clone()
            .ok_or_else(|| ToolError::other("Agent tool requires a parent tool_call_id"))?;
        run_subagent(
            self.runtime.clone(),
            parent_session_id,
            parent_run_id,
            parent_tool_call_id,
            ctx.turn_index,
            ctx.cwd.clone(),
            definition,
            parsed,
        )
        .await
    }
}

/// RAII cleanup for the per-run entry in `SubagentRuntime::active`. The
/// entry is pushed before `run_subagent` builds the child; any `?` between
/// the push and the explicit success-path removal at step 11 would
/// otherwise leak a `Running` entry. Because `active` is a synchronous
/// `std::sync::Mutex` we remove the entry inline in `Drop`, so cleanup is
/// immediate and reliable even while the tokio runtime is shutting down
/// (a detached `tokio::spawn` could be dropped without running). (#162, #215)
struct ActiveEntryGuard {
    runtime: Arc<SubagentRuntime>,
    subagent_session_id: SessionId,
}

impl Drop for ActiveEntryGuard {
    fn drop(&mut self) {
        let sid = &self.subagent_session_id;
        // Recover from a poisoned lock: a panic elsewhere must not strand
        // a stale `Running` entry, and the data is a plain Vec.
        let mut active = self
            .runtime
            .active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        active.retain(|h| &h.subagent_session_id != sid);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_subagent(
    runtime: Arc<SubagentRuntime>,
    parent_session_id: SessionId,
    parent_run_id: RunId,
    parent_tool_call_id: String,
    parent_turn_index: u32,
    cwd: PathBuf,
    definition: AgentDefinition,
    invocation: AgentToolInput,
) -> Result<ToolResult, ToolError> {
    let queue_started = Instant::now();
    let agent_name = definition.name.clone();
    // Mint a child session id up front so it can appear in the
    // SubagentStart hook input and event payloads.
    let subagent_session_id = SessionId::new();
    let subagent_run_id = RunId::new();
    let task_preview: String = invocation.task.chars().take(160).collect();

    // 1. Fire SubagentStart hooks BEFORE acquiring a slot so a hook deny
    //    cannot block the queue.
    let mut effective_task = invocation.task.clone();
    let mut hook_added_context = String::new();
    {
        let mut hi = HookInput::new(
            HookEventName::SubagentStart,
            parent_session_id.clone(),
            cwd.clone(),
            parent_run_id.clone(),
        );
        hi.turn_index = parent_turn_index;
        hi.tool_name = Some(agent_name.clone());
        hi.tool_input = Some(json!({
            "agent": agent_name,
            "task": effective_task,
            "context": invocation.context,
            "subagent_session_id": subagent_session_id,
            "parent_tool_call_id": parent_tool_call_id,
        }));
        let records = runtime.parent_hooks.dispatch(&hi).await;
        let (decision, added_ctx) = aggregate(&records);
        match decision {
            HookAggregateDecision::Deny { reason } => {
                emit_subagent_failed(
                    &runtime,
                    &parent_session_id,
                    &parent_run_id,
                    &agent_name,
                    &subagent_session_id,
                    &reason,
                )
                .await;
                return Ok(ToolResult::error_text(format!(
                    "subagent {agent_name:?} denied by hook: {reason}"
                )));
            }
            HookAggregateDecision::Allow { updated_input } => {
                if let Some(updated) = updated_input {
                    if let Some(t) = updated.get("task").and_then(|v| v.as_str()) {
                        effective_task = t.to_string();
                    }
                }
            }
            HookAggregateDecision::Ask { reason } => {
                emit_subagent_failed(
                    &runtime,
                    &parent_session_id,
                    &parent_run_id,
                    &agent_name,
                    &subagent_session_id,
                    &format!("hook returned Ask which is not supported for subagents: {reason}"),
                )
                .await;
                return Ok(ToolResult::error_text(format!(
                    "subagent {agent_name:?}: hook returned Ask (not supported in M6): {reason}"
                )));
            }
            HookAggregateDecision::Continue => {}
        }
        if !added_ctx.0.is_empty() {
            hook_added_context = added_ctx.0;
        }
    }

    // 2. Acquire a slot from the bounded semaphore.
    let permit = match runtime.semaphore.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            emit_subagent_failed(
                &runtime,
                &parent_session_id,
                &parent_run_id,
                &agent_name,
                &subagent_session_id,
                "subagent semaphore closed",
            )
            .await;
            return Ok(ToolResult::error_text(
                "subagent slot semaphore closed".to_string(),
            ));
        }
    };
    let queue_duration_ms = u64::try_from(queue_started.elapsed().as_millis()).unwrap_or(u64::MAX);

    // 3. Build child options. Tool scoping is the most important step.
    let scoped_tools = scope_child_tools(&runtime.base_tools, &definition);
    let scoped_options = build_child_options(
        &runtime.parent_options,
        &runtime.runtime_options,
        &definition,
        invocation.max_turns,
        subagent_session_id.clone(),
        cwd.clone(),
        &effective_task,
        invocation.context.as_ref(),
        &hook_added_context,
    );
    let child_hooks = if runtime.runtime_options.context_policy.inherit_hooks {
        runtime.parent_hooks.clone()
    } else {
        Hooks::new()
    };

    // Wrap the parent's session sink in a ChildSessionSink that also
    // tallies bookkeeping for the result we return to the parent.
    let counter_tool_calls = Arc::new(AtomicU32::new(0));
    let last_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let usage: Arc<Mutex<Option<TokenUsage>>> = Arc::new(Mutex::new(None));
    let final_subtype: Arc<Mutex<Option<crate::events::ResultSubtype>>> =
        Arc::new(Mutex::new(None));
    let parent_event_tx = runtime.parent_event_tx.lock().await.clone();
    let inner_persist: Option<Arc<dyn SessionSink>> =
        if runtime.runtime_options.persist_subagent_transcripts {
            if let Some(factory) = &runtime.child_sink_factory {
                // Write a SubagentMetadata record once at the head of the
                // child JSONL so audits can recover parent linkage from
                // the child file alone (without scanning every parent
                // session). Errors are non-fatal — the run still goes
                // forward and the linkage will only be reconstructable by
                // scanning parent sessions.
                let md = ChildSessionMetadata {
                    parent_session_id: parent_session_id.clone(),
                    parent_run_id: parent_run_id.clone(),
                    parent_tool_call_id: parent_tool_call_id.clone(),
                    agent_name: agent_name.clone(),
                };
                if let Err(e) = factory
                    .write_metadata(&parent_session_id, &subagent_session_id, &md)
                    .await
                {
                    tracing::warn!(
                        agent = %agent_name,
                        child = %subagent_session_id,
                        "child session metadata write failed: {e}"
                    );
                }
                Some(factory.child_sink(&parent_session_id, &subagent_session_id))
            } else {
                runtime.session_sink.clone()
            }
        } else {
            None
        };
    let child_sink: Arc<dyn SessionSink> = Arc::new(ChildSessionSink {
        inner: inner_persist,
        parent_tx: parent_event_tx.clone(),
        visibility: runtime.runtime_options.event_visibility,
        agent_name: agent_name.clone(),
        counter_tool_calls: counter_tool_calls.clone(),
        last_text: last_text.clone(),
        usage: usage.clone(),
        final_subtype: final_subtype.clone(),
    });

    // 4. Emit telemetry + Start event.
    if let Some(t) = &runtime.telemetry_sink {
        t.record(&AgentTelemetryEvent::SubagentQueued {
            session_id: parent_session_id.clone(),
            run_id: parent_run_id.clone(),
            agent_name: agent_name.clone(),
            subagent_session_id: subagent_session_id.clone(),
            queue_duration_ms,
        })
        .await;
        t.record(&AgentTelemetryEvent::SubagentStart {
            session_id: parent_session_id.clone(),
            run_id: parent_run_id.clone(),
            agent_name: agent_name.clone(),
            subagent_session_id: subagent_session_id.clone(),
            subagent_run_id: subagent_run_id.clone(),
            model: scoped_options.model.clone(),
        })
        .await;
    }
    let scoped_tool_names: Vec<String> = scoped_tools.names();
    if let Some(parent_tx) = &parent_event_tx {
        let _ = parent_tx
            .send(AgentEvent::SubagentStart(SubagentStart {
                agent_name: agent_name.clone(),
                parent_session_id: parent_session_id.clone(),
                parent_run_id: parent_run_id.clone(),
                parent_tool_call_id: parent_tool_call_id.clone(),
                subagent_session_id: subagent_session_id.clone(),
                task_preview: task_preview.clone(),
                tools: scoped_tool_names.clone(),
                model: scoped_options.model.clone(),
            }))
            .await;
    }

    // 5. Track this run. The RAII guard removes the entry on Drop so any
    //    `?` between here and the explicit removal at step 11 cannot leave
    //    a ghost `Running` entry in `runtime.active`. (#162)
    {
        let mut active = runtime.active.lock().expect("active mutex poisoned");
        active.push(SubagentRunHandle {
            parent_session_id: parent_session_id.clone(),
            parent_run_id: parent_run_id.clone(),
            parent_tool_call_id: parent_tool_call_id.clone(),
            subagent_session_id: subagent_session_id.clone(),
            subagent_run_id: subagent_run_id.clone(),
            agent_name: agent_name.clone(),
            status: SubagentStatus::Running,
        });
    }
    let _active_guard = ActiveEntryGuard {
        runtime: runtime.clone(),
        subagent_session_id: subagent_session_id.clone(),
    };

    // 6. Build and run the child agent with timeout. Decide whether the
    // child should participate in the parent's checkpoint lineage based on
    // the per-definition policy (falling back to the runtime default).
    let effective_ckpt_policy = definition
        .checkpointing
        .unwrap_or(runtime.runtime_options.checkpoint_policy);
    let inherit_ckpt = match effective_ckpt_policy {
        SubagentCheckpointPolicy::Disabled => false,
        SubagentCheckpointPolicy::InheritParent
        | SubagentCheckpointPolicy::RequiredForMutatingTools => true,
    };
    let mut child_builder = AgentBuilder::new()
        .options(scoped_options.clone())
        .provider(runtime.provider.clone())
        .tools(scoped_tools.clone())
        .hooks(child_hooks)
        .approval_memory(
            if runtime
                .runtime_options
                .context_policy
                .inherit_approval_memory
            {
                // Approval memory is not exposed as a clone yet, so we still
                // build a fresh one but inheriting it would replace this.
                ApprovalMemory::new()
            } else {
                ApprovalMemory::new()
            },
        )
        .session_sink(child_sink);
    if inherit_ckpt {
        if let Some(handle) = runtime.parent_checkpoint_handle.clone() {
            child_builder = child_builder.checkpoint_handle(handle);
        }
    }
    let agent = child_builder
        .build()
        .map_err(|e| ToolError::other(format!("subagent build failed: {e}")))?;

    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_notify = Arc::new(tokio::sync::Notify::new());
    let cfg = RunConfig {
        run_id: subagent_run_id.clone(),
        cancel: cancel.clone(),
        cancel_notify: cancel_notify.clone(),
        emit_system_init: true,
        resumed_session: false,
    };

    let (child_event_tx, mut child_event_rx) = mpsc::channel::<AgentEvent>(64);
    let cancel_for_task = cancel.clone();
    let notify_for_task = cancel_notify.clone();
    let task_prompt = effective_task.clone();
    let agent_handle = tokio::spawn(async move {
        let _ = run_loop(agent, task_prompt, child_event_tx, cfg).await;
        // Drop the permit when the child finishes.
        drop(permit);
        // Make sure cancel/notify references stay alive.
        let _ = cancel_for_task;
        let _ = notify_for_task;
    });
    // Capture an `AbortHandle` so we can forcibly stop the child on
    // timeout. Without this the timeout only sets the cooperative cancel
    // flag; an unresponsive child (e.g. blocked on a provider call) keeps
    // running and burning tokens even after this function returns.
    let abort_handle = agent_handle.abort_handle();
    // Drain the child events so the channel doesn't fill (the side effects
    // happen in the ChildSessionSink already; this loop is just to keep
    // the channel flowing).
    let timeout_dur = runtime.runtime_options.subagent_timeout;
    let started_at = Instant::now();
    let global_cancel = runtime.cancel_flag.clone();
    let cancel_for_drain = cancel.clone();
    let notify_for_drain = cancel_notify.clone();
    let drain_cancel = global_cancel.clone();
    let drain_handle = tokio::spawn(async move {
        // Periodically check the global cancel; otherwise just drain.
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    if drain_cancel.load(Ordering::Acquire) {
                        cancel_for_drain.store(true, Ordering::Release);
                        notify_for_drain.notify_waiters();
                    }
                }
                ev = child_event_rx.recv() => {
                    if ev.is_none() {
                        return;
                    }
                }
            }
        }
    });
    // Map JoinHandle / timeout outcome to a coarse status. This only knows
    // about timeout vs. ran-to-end; the *child run's own* terminal Result
    // event (captured in `final_subtype`) is the authoritative signal for
    // success vs. internal failure.
    let join_outcome = match timeout(timeout_dur, agent_handle).await {
        Ok(Ok(())) => None,
        Ok(Err(join_err)) => {
            // Distinguish panic (real failure) from abort (the parent
            // aborted us — already accounted for upstream). For an
            // unsolicited cancellation here, treat as failure.
            if join_err.is_cancelled() {
                Some(SubagentStatus::Cancelled)
            } else {
                Some(SubagentStatus::Failed)
            }
        }
        Err(_elapsed) => {
            // First raise the cooperative cancel signal so anything that
            // checks it can drop the request mid-call, then forcibly abort
            // the JoinHandle so the child cannot keep burning tokens past
            // the timeout window.
            cancel.store(true, Ordering::Release);
            cancel_notify.notify_waiters();
            abort_handle.abort();
            Some(SubagentStatus::TimedOut)
        }
    };
    drain_handle.abort();
    let duration_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

    let final_text = last_text.lock().await.clone().unwrap_or_default();
    let final_usage = usage.lock().await.clone();
    let tool_call_count = counter_tool_calls.load(Ordering::Acquire);
    let final_subtype_value = *final_subtype.lock().await;

    // 7. Determine final status. A child that produced a terminal Result is
    // classified by that result, never overridden by a cancel flag — reading
    // the shared `runtime.cancel_flag` as an override would misclassify a child
    // that ran to completion as Cancelled the moment a sibling (or the parent,
    // after this child finished) tripped the shared flag, discarding a good
    // result (#43). For a child that produced *no* terminal result, the per-run
    // `cancel` (this child's timeout, or a parent cancel the drain task copied
    // in) and the runtime-wide `cancel_flag` (a parent cancel the 100ms drain
    // poll had not yet propagated) are both consulted, so a parent-cancelled
    // child is not silently reported Succeeded (review: codex finding).
    let per_run_cancelled = cancel.load(Ordering::Acquire);
    let parent_cancelled = runtime.cancel_flag.load(Ordering::Acquire);
    let status = classify_status(
        join_outcome,
        per_run_cancelled,
        parent_cancelled,
        final_subtype_value,
    );

    // 8. Trim the result preview. `String::truncate` panics if the byte index
    // is not on a UTF-8 char boundary. LLM output regularly contains
    // multi-byte characters (emoji, non-ASCII), so back up to the nearest
    // boundary before truncating.
    let preview_max = runtime.runtime_options.max_result_preview_bytes.max(64);
    let mut result_preview = final_text.clone();
    if result_preview.len() > preview_max {
        let mut end = preview_max;
        while end > 0 && !result_preview.is_char_boundary(end) {
            end -= 1;
        }
        result_preview.truncate(end);
        result_preview.push_str("\n…[truncated]");
    }

    // 9. Fire SubagentStop hooks (which can transform the preview).
    let mut adjusted_preview = result_preview.clone();
    {
        let mut hi = HookInput::new(
            HookEventName::SubagentStop,
            parent_session_id.clone(),
            cwd.clone(),
            parent_run_id.clone(),
        );
        hi.turn_index = parent_turn_index;
        hi.tool_name = Some(agent_name.clone());
        hi.metadata = json!({
            "subagent_session_id": subagent_session_id,
            "subagent_run_id": subagent_run_id,
            "status": status,
            "duration_ms": duration_ms,
            "tool_calls": tool_call_count,
        });
        hi.tool_input = Some(json!({
            "agent": agent_name,
            "task": effective_task,
            "result_preview": result_preview,
        }));
        let records = runtime.parent_hooks.dispatch(&hi).await;
        let (decision, _) = aggregate(&records);
        if let HookAggregateDecision::Allow {
            updated_input: Some(updated),
        } = decision
        {
            if let Some(rewritten) = updated.get("result_preview").and_then(|v| v.as_str()) {
                adjusted_preview = rewritten.to_string();
            }
        }
    }

    // 10. Emit terminal event.
    let cost_usd = runtime
        .pricing
        .as_ref()
        .zip(final_usage.as_ref())
        .map(|(p, u)| p.estimate(u));
    if let Some(t) = &runtime.telemetry_sink {
        match status {
            SubagentStatus::Succeeded => {
                t.record(&AgentTelemetryEvent::SubagentStop {
                    session_id: parent_session_id.clone(),
                    run_id: parent_run_id.clone(),
                    agent_name: agent_name.clone(),
                    subagent_session_id: subagent_session_id.clone(),
                    subagent_run_id: subagent_run_id.clone(),
                    outcome: TelemetryOutcome::Ok,
                    duration_ms,
                    tool_calls: tool_call_count,
                    usage: final_usage.clone(),
                    cost_usd,
                })
                .await;
            }
            SubagentStatus::Failed => {
                t.record(&AgentTelemetryEvent::SubagentFailed {
                    session_id: parent_session_id.clone(),
                    run_id: parent_run_id.clone(),
                    agent_name: agent_name.clone(),
                    subagent_session_id: subagent_session_id.clone(),
                    error_category: "subagent".into(),
                })
                .await;
            }
            SubagentStatus::Cancelled => {
                t.record(&AgentTelemetryEvent::SubagentCancelled {
                    session_id: parent_session_id.clone(),
                    run_id: parent_run_id.clone(),
                    agent_name: agent_name.clone(),
                    subagent_session_id: subagent_session_id.clone(),
                    reason: "cancelled by parent".into(),
                })
                .await;
            }
            SubagentStatus::TimedOut => {
                t.record(&AgentTelemetryEvent::SubagentTimedOut {
                    session_id: parent_session_id.clone(),
                    run_id: parent_run_id.clone(),
                    agent_name: agent_name.clone(),
                    subagent_session_id: subagent_session_id.clone(),
                    timeout_ms: u64::try_from(timeout_dur.as_millis()).unwrap_or(u64::MAX),
                })
                .await;
            }
            SubagentStatus::Running => {}
        }
    }
    if let Some(parent_tx) = &parent_event_tx {
        match status {
            SubagentStatus::TimedOut => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentTimedOut(SubagentTimedOut {
                        agent_name: agent_name.clone(),
                        subagent_session_id: subagent_session_id.clone(),
                        timeout_ms: u64::try_from(timeout_dur.as_millis()).unwrap_or(u64::MAX),
                    }))
                    .await;
            }
            SubagentStatus::Cancelled => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentCancelled(SubagentCancelled {
                        agent_name: agent_name.clone(),
                        subagent_session_id: subagent_session_id.clone(),
                        reason: "cancelled by parent".into(),
                    }))
                    .await;
            }
            SubagentStatus::Failed => {
                let _ = parent_tx
                    .send(AgentEvent::SubagentFailed(SubagentFailed {
                        agent_name: agent_name.clone(),
                        subagent_session_id: subagent_session_id.clone(),
                        error: "subagent run failed".into(),
                    }))
                    .await;
            }
            _ => {}
        }
        let _ = parent_tx
            .send(AgentEvent::SubagentStop(SubagentStop {
                agent_name: agent_name.clone(),
                subagent_session_id: subagent_session_id.clone(),
                status,
                duration_ms,
                usage: final_usage.clone(),
                result_preview: adjusted_preview.clone(),
                tool_calls: tool_call_count,
            }))
            .await;
    }

    // 11. Removal from `runtime.active` is handled by `_active_guard`'s
    // `Drop`, which now removes the entry synchronously. We keep the
    // explicit removal here as well so success paths see the entry gone
    // before the result is returned. The guard's drop is idempotent
    // (retain is a no-op when no entry matches).
    {
        let mut active = runtime.active.lock().expect("active mutex poisoned");
        active.retain(|h| h.subagent_session_id != subagent_session_id);
    }

    // 12. Build the structured result the parent sees.
    let result = AgentToolResult {
        agent_name: agent_name.clone(),
        agent_run_id: subagent_run_id,
        subagent_session_id: subagent_session_id.clone(),
        result: adjusted_preview.clone(),
        usage: final_usage,
        tool_calls: tool_call_count,
        duration_ms,
        status,
    };
    let metadata = serde_json::to_value(&result).unwrap_or(Value::Null);
    let body = match status {
        SubagentStatus::Succeeded => format!(
            "[{}] (status=succeeded, tool_calls={}, child_session={})\n{}",
            agent_name, tool_call_count, subagent_session_id, adjusted_preview
        ),
        SubagentStatus::Failed => format!(
            "[{}] (status=failed) the subagent failed during execution. child_session={}",
            agent_name, subagent_session_id
        ),
        SubagentStatus::Cancelled => format!(
            "[{}] (status=cancelled) the subagent was cancelled. child_session={}",
            agent_name, subagent_session_id
        ),
        SubagentStatus::TimedOut => format!(
            "[{}] (status=timed_out) the subagent exceeded its {}-second timeout. \
             child_session={}",
            agent_name,
            timeout_dur.as_secs(),
            subagent_session_id
        ),
        SubagentStatus::Running => format!("[{agent_name}] (status=running)"),
    };
    let is_error = matches!(
        status,
        SubagentStatus::Failed | SubagentStatus::Cancelled | SubagentStatus::TimedOut
    );
    Ok(ToolResult {
        content: vec![crate::tool::ToolContent::Text(body)],
        is_error,
        metadata,
    })
}

/// Classify a child subagent's final [`SubagentStatus`] from the three
/// signals available at the end of `run_subagent`. Pure so it can be unit
/// tested across the full matrix.
///
/// Precedence:
///   1. `join_outcome` — `JoinHandle` timeout/panic/abort. An unconditional
///      override produced by the timeout/join machinery (a timed-out child is
///      aborted, so it never also presents a terminal `Success`).
///   2. `final_subtype` — the child run's OWN terminal Result subtype, and it
///      is authoritative: a child that produced a `Success`/error result did
///      its work, and a cancel flag must NEVER discard it. Checking the cancel
///      flags before the subtype is exactly the bug #43 fixes — a child that
///      ran to completion is reported by its result even when the runtime-wide
///      `cancel_flag` (or its own per-run signal, drain-copied from the parent)
///      was tripped by a sibling, or by the parent after this child finished.
///   3. Fallback for a child that produced *no* terminal Result: if it was
///      cancelled — by its own `per_run_cancelled` signal (this child's timeout
///      or a parent cancel the drain task already copied in) OR by a
///      `parent_cancelled` (runtime-wide) cancel the 100ms drain poll had not
///      yet propagated — classify `Cancelled` rather than defaulting to
///      success. Both flags are consulted only here, so a parent-cancelled
///      child that never produced a result (e.g. queued on the concurrency
///      semaphore) is not silently reported `Succeeded` (review finding).
fn classify_status(
    join_outcome: Option<SubagentStatus>,
    per_run_cancelled: bool,
    parent_cancelled: bool,
    final_subtype: Option<crate::events::ResultSubtype>,
) -> SubagentStatus {
    match join_outcome {
        Some(s) => s,
        None => match final_subtype {
            // A terminal result is authoritative and is never overridden by a
            // cancel flag (#43).
            Some(crate::events::ResultSubtype::Success) => SubagentStatus::Succeeded,
            Some(crate::events::ResultSubtype::ErrorMaxTurns)
            | Some(crate::events::ResultSubtype::ErrorMaxToolCalls)
            | Some(crate::events::ResultSubtype::ErrorDuringExecution) => SubagentStatus::Failed,
            Some(crate::events::ResultSubtype::Cancelled) => SubagentStatus::Cancelled,
            // No terminal result: a child cancelled by itself or the parent is
            // Cancelled; otherwise it defensively succeeded.
            None if per_run_cancelled || parent_cancelled => SubagentStatus::Cancelled,
            None => SubagentStatus::Succeeded,
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn build_child_options(
    parent: &AgentOptions,
    runtime_options: &AgentRuntimeOptions,
    def: &AgentDefinition,
    invocation_max_turns: Option<u32>,
    child_session: SessionId,
    cwd: PathBuf,
    task: &str,
    context: Option<&Value>,
    hook_added_context: &str,
) -> AgentOptions {
    let mut opts = AgentOptions {
        cwd: cwd.clone(),
        model: def.model.clone().unwrap_or_else(|| parent.model.clone()),
        allowed_tools: Vec::new(),
        allowed_tool_rules: def.tools.clone().unwrap_or_default(),
        disallowed_tool_rules: def.disallowed_tools.clone(),
        permission_mode: def.permission_mode.unwrap_or(parent.permission_mode),
        // The invocation may *lower* max_turns but cannot *raise* it past
        // the definition's cap (or the hard limit). The parent LLM cannot
        // bypass an admin-set cost ceiling by passing a higher value via
        // the Agent tool input.
        max_turns: Some({
            let cap = def
                .effective_max_turns()
                .min(super::definition::MAX_TURNS_HARD_LIMIT);
            match invocation_max_turns {
                Some(n) => n.min(cap),
                None => cap,
            }
        }),
        // A subagent inherits the parent's tool-call ceiling so it cannot be
        // used to bypass the run's `max_tool_calls` budget.
        max_tool_calls: parent.max_tool_calls,
        session_id: Some(child_session),
        permission_callback: parent.permission_callback.clone(),
        system_prompt: None,
        project_instructions: if runtime_options.context_policy.inherit_project_instructions {
            parent.project_instructions.clone()
        } else {
            None
        },
        max_tokens: parent.max_tokens,
        approval_memory: if runtime_options.context_policy.inherit_approval_memory {
            parent.approval_memory
        } else {
            ApprovalMemoryMode::None
        },
        mcp: if runtime_options.context_policy.inherit_mcp_servers {
            scope_child_mcp(parent.mcp.clone(), def.mcp_servers.as_deref())
        } else {
            scope_child_mcp(empty_mcp_options(parent), def.mcp_servers.as_deref())
        },
        tool_search: def.tool_search.clone().unwrap_or_else(|| {
            if runtime_options.context_policy.inherit_tool_search_state {
                parent.tool_search.clone()
            } else {
                let mut t = parent.tool_search.clone();
                t.preloaded_discovered_tools.clear();
                t
            }
        }),
        // Subagents never have their own subagents in M6.
        agents: super::registry::AgentRegistry::default(),
        agent_runtime: AgentRuntimeOptions {
            enabled: false,
            ..AgentRuntimeOptions::default()
        },
        // Subagents inherit the parent's Skill registry so a subagent
        // working on a release-pr task can still load the same Skill if
        // it has been granted the Skill tool. The slash-command registry
        // is parent-only — subagents do not parse slash commands.
        skills: parent.skills.clone(),
        skill_options: parent.skill_options.clone(),
        slash_commands: crate::slash::SlashCommandRegistry::new(false),
        // Subagents inherit the parent's plugin summaries (they can see
        // plugin-derived skills/agents/MCP servers through the registries
        // above) but do not own a separate plugin registry.
        plugins: parent.plugins.clone(),
    };

    // Build the system prompt.
    let mut sys = String::new();
    sys.push_str(&def.prompt);
    sys.push_str("\n\n## Task\n");
    sys.push_str(task);
    if let Some(c) = context {
        if !c.is_null() {
            sys.push_str("\n\n## Context\n");
            sys.push_str(&serde_json::to_string_pretty(c).unwrap_or_default());
        }
    }
    if !hook_added_context.is_empty() {
        sys.push_str("\n\n## Added Context (SubagentStart hooks)\n");
        sys.push_str(hook_added_context);
    }
    opts.system_prompt = Some(sys);
    opts
}

fn empty_mcp_options(parent: &AgentOptions) -> code_agent_mcp::McpOptions {
    let mut m = parent.mcp.clone();
    m.enabled = false;
    m.servers.clear();
    m
}

fn scope_child_mcp(
    mut mcp: code_agent_mcp::McpOptions,
    allowed: Option<&[String]>,
) -> code_agent_mcp::McpOptions {
    if let Some(allowed) = allowed {
        let allowed: BTreeSet<&str> = allowed.iter().map(String::as_str).collect();
        mcp.servers
            .retain(|name, _| allowed.contains(name.as_str()));
        mcp.enabled = !mcp.servers.is_empty();
    }
    mcp
}

/// Build the child's tool registry by intersecting the base registry with
/// the subagent's `tools` allow list and removing anything in
/// `disallowed_tools`. Always strips the `Agent` tool (no recursion).
///
/// Rule kinds:
/// - `ExactTool(name)` and `ToolGlob(pattern)` are passed to the registry
///   filter as name patterns (the registry filter understands `*`/`?`).
/// - `McpServerGlob { server, tool_glob }` is converted to the canonical
///   `mcp__<server>__<glob>` shape so the registry filter matches namespaced
///   MCP tool names.
/// - `BashCommandRegex` and `FilePathGlob` constrain a tool's *input*, not
///   the registry membership, so they don't restrict membership here. They
///   still apply at policy-evaluation time inside the child run.
/// - `AgentNameGlob` rules are dropped here because subagents never have
///   the `Agent` tool registered (recursion forbidden).
///
/// When the definition has only input-shape rules (no name/glob/MCP), keep
/// the full inherited registry rather than collapsing to empty — otherwise
/// a definition like `tools: [{tool: "Bash", command_regex: "^cargo$"}]`
/// would silently disable all tools.
pub fn scope_child_tools(base: &ToolRegistry, def: &AgentDefinition) -> ToolRegistry {
    let mut filtered = if let Some(rules) = &def.tools {
        let mut patterns: Vec<String> = Vec::new();
        let mut had_name_or_input_rule = false;
        for rule in rules {
            had_name_or_input_rule = true;
            match rule {
                ToolRule::ExactTool(n) => patterns.push(n.clone()),
                ToolRule::ToolGlob(g) => patterns.push(g.clone()),
                ToolRule::McpServerGlob { server, tool_glob } => {
                    patterns.push(format!("mcp__{}__{}", server, tool_glob));
                }
                // Input-shape rules don't restrict registry membership.
                // Treat them as "this tool name is allowed" using the
                // tool field — for Bash that's "Bash", for FilePathGlob
                // that's the named tool.
                ToolRule::BashCommandRegex(_) => patterns.push("Bash".into()),
                ToolRule::FilePathGlob { tool, .. } => patterns.push(tool.clone()),
                // Subagents never have access to Agent regardless of the
                // glob pattern; intentionally skip these rules here. The
                // unconditional `Agent` strip below handles the policy
                // contract.
                ToolRule::AgentNameGlob { .. } => {}
            }
        }
        if patterns.is_empty() {
            if had_name_or_input_rule {
                // The definition only had AgentNameGlob entries (which we
                // intentionally dropped). Honor the explicit allow-set
                // intent: an empty registry rather than full inheritance.
                ToolRegistry::new()
            } else {
                base.clone()
            }
        } else {
            base.filter(&patterns)
        }
    } else {
        base.clone()
    };
    // Apply disallowed_tools.
    let mut to_remove: Vec<String> = Vec::new();
    for name in filtered.names() {
        for rule in &def.disallowed_tools {
            if rule.matches(&name, &Value::Null) {
                to_remove.push(name.clone());
                break;
            }
        }
    }
    if !to_remove.is_empty() {
        let keep: Vec<String> = filtered
            .names()
            .into_iter()
            .filter(|n| !to_remove.contains(n))
            .collect();
        filtered = filtered.filter(&keep);
    }
    // Always strip the Agent tool itself: no recursion.
    let stripped: Vec<String> = filtered
        .names()
        .into_iter()
        .filter(|n| n != SUBAGENT_TOOL_NAME)
        .collect();
    if stripped.len() != filtered.len() {
        filtered = filtered.filter(&stripped);
    }
    filtered
}

async fn emit_subagent_failed(
    runtime: &SubagentRuntime,
    parent_session_id: &SessionId,
    parent_run_id: &RunId,
    agent_name: &str,
    subagent_session_id: &SessionId,
    error: &str,
) {
    if let Some(t) = &runtime.telemetry_sink {
        t.record(&AgentTelemetryEvent::SubagentFailed {
            session_id: parent_session_id.clone(),
            run_id: parent_run_id.clone(),
            agent_name: agent_name.to_string(),
            subagent_session_id: subagent_session_id.clone(),
            error_category: "subagent".into(),
        })
        .await;
    }
    if let Some(tx) = runtime.parent_event_tx.lock().await.as_ref() {
        let _ = tx
            .send(AgentEvent::SubagentFailed(SubagentFailed {
                agent_name: agent_name.to_string(),
                subagent_session_id: subagent_session_id.clone(),
                error: error.to_string(),
            }))
            .await;
    }
}

/// Optional curated set of starter subagent definitions. The CLI / SDK
/// chooses whether to register these.
pub fn builtin_starter_definitions() -> Vec<AgentDefinition> {
    fn def(
        name: &str,
        description: &str,
        prompt: &str,
        tools: &[&str],
        disallowed: &[&str],
    ) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: description.to_string(),
            prompt: prompt.to_string(),
            tools: Some(
                tools
                    .iter()
                    .map(|t| ToolRule::ExactTool((*t).into()))
                    .collect(),
            ),
            disallowed_tools: disallowed
                .iter()
                .map(|t| ToolRule::ExactTool((*t).into()))
                .collect(),
            model: None,
            max_turns: Some(8),
            permission_mode: Some(PermissionMode::DontAsk),
            mcp_servers: Some(Vec::new()),
            tool_search: None,
            checkpointing: Some(SubagentCheckpointPolicy::Disabled),
            validation: None,
            output_mode: super::definition::AgentOutputMode::Markdown,
            concurrency: super::definition::AgentConcurrencyHint::ParallelSafe,
        }
    }
    vec![
        def(
            "general-purpose",
            "Broad research/delegation when no specialized agent matches.",
            "You are a careful general-purpose research subagent. Read files and search code as needed. Return a concise summary with concrete file paths.",
            &["Read", "Grep", "Glob"],
            &["Edit", "Write", "Bash", "Agent"],
        ),
        def(
            "code-reviewer",
            "Reviews implementation quality without editing code.",
            "You are a senior Rust code reviewer. Identify defects, complexity, and design issues. Return concise findings (severity, file:line, recommended fix).",
            &["Read", "Grep", "Glob"],
            &["Edit", "Write", "Bash", "Agent"],
        ),
        def(
            "test-analyst",
            "Runs and analyzes tests.",
            "You are a Rust test analyst. Identify gaps in test coverage, flaky tests, and missing edge cases. Return a short prioritized list.",
            &["Read", "Grep", "Glob", "Bash"],
            &["Edit", "Write", "Agent"],
        ),
        def(
            "security-reviewer",
            "Reviews code for security vulnerabilities and unsafe data handling.",
            "You are a security-focused Rust reviewer. Focus on input validation, path traversal, secret handling, unsafe code, dependency risk, and command-execution risk. Return findings with severity, file:line, and recommended fix.",
            &["Read", "Grep", "Glob"],
            &["Edit", "Write", "Bash", "Agent"],
        ),
        def(
            "docs-reviewer",
            "Inspects documentation and API clarity.",
            "You are a documentation reviewer. Identify missing docs, outdated examples, and unclear APIs. Return concise findings.",
            &["Read", "Grep", "Glob"],
            &["Edit", "Write", "Bash", "Agent"],
        ),
        def(
            "dependency-reviewer",
            "Inspects dependency and supply-chain risk.",
            "You are a dependency reviewer. Look for unpinned versions, abandoned crates, and unaudited transitives. Return concise findings.",
            &["Read", "Grep", "Glob"],
            &["Edit", "Write", "Bash", "Agent"],
        ),
    ]
}

#[cfg(test)]
mod active_guard_tests {
    //! Direct coverage of `ActiveEntryGuard`. Exercising the full
    //! `run_subagent` flow requires a `SubagentRuntime` with a real
    //! provider, registry, and hook tree, which is out of scope for a
    //! unit test. Instead we verify the guard's contract in isolation
    //! by constructing a minimal runtime with only the fields the
    //! guard touches.

    use super::*;
    use crate::agents::runtime::AgentRuntimeOptions;
    use crate::errors::ProviderError;
    use crate::provider::{ModelEvent as PModelEvent, ModelRequest};
    use futures::stream::BoxStream;
    use std::sync::atomic::AtomicBool;

    struct StubProvider;
    #[async_trait]
    impl ModelProvider for StubProvider {
        async fn stream_turn(
            &self,
            _: ModelRequest,
        ) -> Result<BoxStream<'static, Result<PModelEvent, ProviderError>>, ProviderError> {
            Err(ProviderError::Other("stub".into()))
        }
    }

    fn minimal_runtime() -> Arc<SubagentRuntime> {
        Arc::new(SubagentRuntime {
            registry: Arc::new(AgentRegistry::default()),
            provider: Arc::new(StubProvider),
            base_tools: ToolRegistry::default(),
            parent_options: AgentOptions::default(),
            parent_hooks: Hooks::default(),
            session_sink: None,
            child_sink_factory: None,
            telemetry_sink: None,
            pricing: None,
            parent_checkpoint_handle: None,
            runtime_options: AgentRuntimeOptions::default(),
            semaphore: Arc::new(Semaphore::new(1)),
            parent_event_tx: Mutex::new(None),
            active: std::sync::Mutex::new(Vec::new()),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        })
    }

    fn push_handle(_rt: &SubagentRuntime, sid: SessionId) -> SubagentRunHandle {
        SubagentRunHandle {
            parent_session_id: SessionId::new(),
            parent_run_id: RunId::new(),
            parent_tool_call_id: "tc".into(),
            subagent_session_id: sid,
            subagent_run_id: RunId::new(),
            agent_name: "X".into(),
            status: SubagentStatus::Running,
        }
    }

    /// Dropping the guard removes the matching entry from `runtime.active`
    /// synchronously — no task spawn, so the entry is gone the instant the
    /// guard's scope ends (#215).
    #[tokio::test]
    async fn guard_drop_removes_matching_entry() {
        let rt = minimal_runtime();
        let sid = SessionId::new();
        rt.active
            .lock()
            .unwrap()
            .push(push_handle(&rt, sid.clone()));
        assert_eq!(rt.active.lock().unwrap().len(), 1);
        {
            let _g = ActiveEntryGuard {
                runtime: rt.clone(),
                subagent_session_id: sid.clone(),
            };
        }
        // Removal is synchronous: no yielding required.
        assert!(
            rt.active.lock().unwrap().is_empty(),
            "guard did not clean up"
        );
    }

    /// If the explicit step-11 removal has already run, the guard's
    /// drop is a no-op (idempotent retain) — no panic, no other
    /// entries clobbered.
    #[tokio::test]
    async fn guard_drop_is_idempotent_after_explicit_removal() {
        let rt = minimal_runtime();
        let sid1 = SessionId::new();
        let sid2 = SessionId::new();
        rt.active
            .lock()
            .unwrap()
            .push(push_handle(&rt, sid1.clone()));
        rt.active
            .lock()
            .unwrap()
            .push(push_handle(&rt, sid2.clone()));
        // Simulate the explicit removal at step 11 for sid1.
        rt.active
            .lock()
            .unwrap()
            .retain(|h| h.subagent_session_id != sid1);
        // The guard for sid1 drops — should be a no-op, leaving sid2 alone.
        {
            let _g = ActiveEntryGuard {
                runtime: rt.clone(),
                subagent_session_id: sid1.clone(),
            };
        }
        let active = rt.active.lock().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].subagent_session_id, sid2);
    }
}

#[cfg(test)]
mod classify_status_tests {
    //! Coverage of `classify_status`, the pure status-classification helper
    //! extracted from `run_subagent`. The full `run_subagent` flow needs a
    //! real provider/registry/hook tree (out of scope here), so we exercise
    //! the classification matrix directly. The key regression (#43) is that a
    //! child that ran to completion with `Success` is classified `Succeeded`
    //! and its result preserved even when the runtime-wide shared cancel flag
    //! has been tripped by a sibling or the parent. A child that produced *no*
    //! terminal result, however, still honors a parent/runtime-wide cancel that
    //! the per-run drain had not yet propagated, so it is not reported
    //! `Succeeded` by default (review follow-up).

    use super::classify_status;
    use crate::agents::runtime::SubagentStatus;
    use crate::events::ResultSubtype;

    /// Regression for #43. A sibling (or the parent) trips the runtime-wide
    /// shared `cancel_flag` — modeled here as `parent_cancelled = true` — but
    /// THIS child already ran to completion with `ResultSubtype::Success`. The
    /// terminal result is authoritative, so it must classify as `Succeeded`
    /// (result preserved), NOT `Cancelled`.
    #[test]
    fn shared_cancel_does_not_discard_completed_success() {
        let status = classify_status(None, false, true, Some(ResultSubtype::Success));
        assert_eq!(status, SubagentStatus::Succeeded);
    }

    /// Also #43, the per-run variant the review surfaced: even if this child's
    /// OWN per-run signal was tripped (a parent cancel the drain task copied in)
    /// while it was racing to finish, a `Success` terminal result it managed to
    /// produce is still authoritative and preserved — a cancel flag must never
    /// discard a completed result.
    #[test]
    fn per_run_cancel_does_not_discard_completed_success() {
        let status = classify_status(None, true, false, Some(ResultSubtype::Success));
        assert_eq!(status, SubagentStatus::Succeeded);
    }

    /// Self-cancelled with no terminal subtype is `Cancelled`.
    #[test]
    fn self_cancelled_no_subtype_is_cancelled() {
        let status = classify_status(None, true, false, None);
        assert_eq!(status, SubagentStatus::Cancelled);
    }

    /// A child's own `ErrorDuringExecution` / `ErrorMaxTurns` terminal subtype
    /// maps to `Failed` when not cancelled.
    #[test]
    fn error_subtypes_map_to_failed() {
        assert_eq!(
            classify_status(
                None,
                false,
                false,
                Some(ResultSubtype::ErrorDuringExecution)
            ),
            SubagentStatus::Failed
        );
        assert_eq!(
            classify_status(None, false, false, Some(ResultSubtype::ErrorMaxTurns)),
            SubagentStatus::Failed
        );
    }

    /// The child loop's own terminal `Cancelled` subtype maps to `Cancelled`
    /// even if the per-run signal was not separately tripped.
    #[test]
    fn child_cancelled_subtype_is_cancelled() {
        let status = classify_status(None, false, false, Some(ResultSubtype::Cancelled));
        assert_eq!(status, SubagentStatus::Cancelled);
    }

    /// A completed loop that emitted no Result event, with no cancellation of
    /// any kind, defaults to `Succeeded`.
    #[test]
    fn no_subtype_defaults_to_succeeded() {
        let status = classify_status(None, false, false, None);
        assert_eq!(status, SubagentStatus::Succeeded);
    }

    /// Regression for the review finding. A child cancelled by the PARENT
    /// (runtime-wide `cancel_flag` set) that produced no terminal Result —
    /// because the 100ms drain poll had not yet copied the parent cancel into
    /// this child's per-run flag (e.g. the child was queued on the concurrency
    /// semaphore, or finished within a drain interval) — must classify as
    /// `Cancelled`, not default to `Succeeded`.
    #[test]
    fn parent_cancelled_no_subtype_is_cancelled() {
        let status = classify_status(None, false, true, None);
        assert_eq!(status, SubagentStatus::Cancelled);
    }

    /// `join_outcome` is an unconditional override: a JoinHandle-level
    /// timeout/panic/abort wins over both the per-run signal and the subtype.
    #[test]
    fn join_outcome_overrides_everything() {
        assert_eq!(
            classify_status(
                Some(SubagentStatus::TimedOut),
                true,
                true,
                Some(ResultSubtype::Success)
            ),
            SubagentStatus::TimedOut
        );
        assert_eq!(
            classify_status(
                Some(SubagentStatus::Failed),
                false,
                false,
                Some(ResultSubtype::Success)
            ),
            SubagentStatus::Failed
        );
    }
}
