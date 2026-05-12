use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::checkpoint::CheckpointHandle;
use crate::conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
use crate::errors::{AgentError, ToolError};
use crate::events::{
    AgentEvent, AgentResult, HookEvaluated, HookOutcomeKind, InterruptedEvent, PolicyDecisionEvent,
    ResultSubtype, ToolCallCompleted, ToolCallDenied, ToolCallStarted, ToolCatalogBuilt,
    ToolContextBudgetUpdated, ToolDeferred, ToolDisabled, ToolLoaded, ToolLoadingModeResolved,
    ToolSearchCompleted, ToolSearchFailed, ToolSearchStarted, UserToolResult,
};
use crate::hooks::{aggregate, HookAggregateDecision, HookEventName, HookInput, Hooks, StopReason};
use crate::mcp::{
    mcp_completion_from_result, mcp_failure_from_error, register_mcp_tools, tool_metadata,
    McpRegistrationReport,
};
use crate::options::{AgentOptions, SessionId};
use crate::permission::PermissionMode;
use crate::policy::{ApprovalMemory, PolicyConfig, PolicyEngine, PolicyOutcome, PolicyToolContext};
use crate::provider::{ModelEvent, ModelProvider, ModelRequest, ToolSchema};
use crate::run_id::RunId;
use crate::telemetry::{AgentTelemetryEvent, ModelPricing, TelemetryOutcome};
use crate::todo::{TodoItem, TodoList};
use crate::tool::{Tool, ToolContext, ToolRegistry};
use crate::tool_catalog::{
    ResolvedToolLoadingMode, SearchToolsDefinitionTool, SearchToolsTool, ToolCatalog,
    ToolCatalogBuilder, ToolLoadPolicy, ToolLoadReason, ToolSearchMatch, ToolSearchMatchSummary,
    ToolSearchProviderConfig, ToolSearchRuntime, ToolSourceFilter, SEARCH_TOOLS_NAME,
};
use crate::usage::TokenUsage;

/// Convert an elapsed duration to milliseconds, saturating at u64::MAX.
/// Intentional saturation handles the extremely unlikely case of timing
/// durations exceeding u64::MAX milliseconds (~584 million years).
fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

/// Trait for a session-store sink the agent can write records to without
/// taking a hard dependency on the session crate.
#[async_trait]
pub trait SessionSink: Send + Sync {
    async fn record(&self, session_id: &SessionId, event: &AgentEvent) -> Result<(), AgentError>;
}

/// Trait for a structured-telemetry sink. Default implementation is a no-op,
/// so callers can opt in to logging without re-emitting through the session
/// JSONL.
#[async_trait]
pub trait TelemetrySink: Send + Sync {
    async fn record(&self, event: &AgentTelemetryEvent);
}

/// Builder/runner for an agent. `query()` is a convenience over this.
pub struct Agent {
    options: AgentOptions,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    session_sink: Option<Arc<dyn SessionSink>>,
    telemetry_sink: Option<Arc<dyn TelemetrySink>>,
    initial_history: Vec<Message>,
    hooks: Hooks,
    pricing: Option<ModelPricing>,
    approval_memory: ApprovalMemory,
    checkpoint_handle: Option<Arc<dyn CheckpointHandle>>,
    child_session_sink_factory: Option<Arc<dyn crate::agents::ChildSessionSinkFactory>>,
}

#[derive(Default)]
pub struct AgentBuilder {
    options: Option<AgentOptions>,
    provider: Option<Arc<dyn ModelProvider>>,
    tools: Option<ToolRegistry>,
    session_sink: Option<Arc<dyn SessionSink>>,
    telemetry_sink: Option<Arc<dyn TelemetrySink>>,
    initial_history: Vec<Message>,
    hooks: Hooks,
    pricing: Option<ModelPricing>,
    approval_memory: Option<ApprovalMemory>,
    checkpoint_handle: Option<Arc<dyn CheckpointHandle>>,
    child_session_sink_factory: Option<Arc<dyn crate::agents::ChildSessionSinkFactory>>,
}

impl AgentBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn options(mut self, options: AgentOptions) -> Self {
        self.options = Some(options);
        self
    }

    pub fn provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn session_sink(mut self, sink: Arc<dyn SessionSink>) -> Self {
        self.session_sink = Some(sink);
        self
    }

    pub fn telemetry_sink(mut self, sink: Arc<dyn TelemetrySink>) -> Self {
        self.telemetry_sink = Some(sink);
        self
    }

    pub fn initial_history(mut self, history: Vec<Message>) -> Self {
        self.initial_history = history;
        self
    }

    pub fn hooks(mut self, hooks: Hooks) -> Self {
        self.hooks = hooks;
        self
    }

    pub fn pricing(mut self, pricing: ModelPricing) -> Self {
        self.pricing = Some(pricing);
        self
    }

    pub fn approval_memory(mut self, memory: ApprovalMemory) -> Self {
        self.approval_memory = Some(memory);
        self
    }

    pub fn checkpoint_handle(mut self, handle: Arc<dyn CheckpointHandle>) -> Self {
        self.checkpoint_handle = Some(handle);
        self
    }

    /// Attach a factory that mints session sinks for child subagent runs.
    /// Optional; when omitted, child events go to the parent's sink under
    /// the parent's session id (still safe, but not separated by file).
    pub fn child_session_sink_factory(
        mut self,
        factory: Arc<dyn crate::agents::ChildSessionSinkFactory>,
    ) -> Self {
        self.child_session_sink_factory = Some(factory);
        self
    }

    pub fn build(self) -> Result<Agent, AgentError> {
        let options = self
            .options
            .ok_or_else(|| AgentError::InvalidConfig("AgentOptions are required".into()))?;
        let provider = self
            .provider
            .ok_or_else(|| AgentError::InvalidConfig("ModelProvider is required".into()))?;
        let tools = self.tools.unwrap_or_default();
        Ok(Agent {
            options,
            provider,
            tools,
            session_sink: self.session_sink,
            telemetry_sink: self.telemetry_sink,
            initial_history: self.initial_history,
            hooks: self.hooks,
            pricing: self.pricing,
            approval_memory: self.approval_memory.unwrap_or_default(),
            checkpoint_handle: self.checkpoint_handle,
            child_session_sink_factory: self.child_session_sink_factory,
        })
    }
}

impl Agent {
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    pub fn options(&self) -> &AgentOptions {
        &self.options
    }

    /// Run the agent against a single user prompt. Streams `AgentEvent`s.
    pub fn run(
        mut self,
        prompt: impl Into<String>,
    ) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        let prompt = prompt.into();
        if self.options.session_id.is_none() {
            self.options.session_id = Some(SessionId::new());
        }
        let session_id = self.options.session_id.clone().unwrap();
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(async move {
            let cfg = RunConfig {
                run_id: RunId::new(),
                cancel,
                cancel_notify,
                emit_system_init: true,
                resumed_session: !self.initial_history.is_empty(),
            };
            if let Err(e) = run_loop(self, prompt, tx.clone(), cfg).await {
                let _ = tx
                    .send(AgentEvent::Result(AgentResult {
                        subtype: ResultSubtype::ErrorDuringExecution,
                        result: Some(format!("agent error: {e}")),
                        session_id,
                        run_id: None,
                        num_turns: 0,
                        usage: None,
                        total_cost_usd: None,
                    }))
                    .await;
            }
        });
        Box::pin(ReceiverStream::new(rx))
    }
}

/// Per-run configuration the AgentClient threads into the loop.
pub struct RunConfig {
    pub run_id: RunId,
    pub cancel: Arc<AtomicBool>,
    pub cancel_notify: Arc<tokio::sync::Notify>,
    pub emit_system_init: bool,
    pub resumed_session: bool,
}

/// Convenience: run an agent with an Anthropic-shaped registration of all
/// built-in tools is the caller's responsibility. This function only requires
/// a `ModelProvider`. Tools and session sinks are configured via the builder.
pub async fn query_with(
    agent: Agent,
    prompt: impl Into<String>,
) -> Result<Pin<Box<dyn Stream<Item = AgentEvent> + Send>>, AgentError> {
    Ok(agent.run(prompt))
}

/// Top-level convenience that constructs an `Agent` using the supplied parts.
pub async fn query(
    prompt: impl Into<String>,
    options: AgentOptions,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
) -> Result<Pin<Box<dyn Stream<Item = AgentEvent> + Send>>, AgentError> {
    let agent = AgentBuilder::new()
        .options(options)
        .provider(provider)
        .tools(tools)
        .build()?;
    Ok(agent.run(prompt))
}

/// Internal trait for the loop to update todos in shared state.
pub trait TodoSink: Send + Sync {
    fn replace(&self, todos: Vec<TodoItem>);
    fn snapshot(&self) -> TodoList;
}

pub(crate) async fn run_loop(
    agent: Agent,
    prompt: String,
    tx: mpsc::Sender<AgentEvent>,
    cfg: RunConfig,
) -> Result<(), AgentError> {
    let Agent {
        options,
        provider,
        tools,
        session_sink,
        telemetry_sink,
        initial_history,
        hooks,
        pricing,
        approval_memory,
        checkpoint_handle,
        child_session_sink_factory,
    } = agent;

    let session_id = options
        .session_id
        .clone()
        .unwrap_or_else(SessionId::default);

    let mut tools = tools;
    let mcp_report = register_mcp_tools(&mut tools, &options.mcp).await;
    if options.mcp.enabled {
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::McpConfigLoaded {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                server_count: options.mcp.servers.len(),
            },
        )
        .await;
    }
    for status in &mcp_report.statuses {
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::McpServerConnecting {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                server_name: status.name.clone(),
                transport: status.transport,
            },
        )
        .await;
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::McpServerConnecting(code_agent_mcp::McpServerConnecting {
                server_name: status.name.clone(),
                transport: status.transport,
            }),
        )
        .await;
        match status.status {
            code_agent_mcp::McpConnectionStatus::Connected => {
                emit_telemetry(
                    &telemetry_sink,
                    AgentTelemetryEvent::McpServerConnected {
                        session_id: session_id.clone(),
                        run_id: cfg.run_id.clone(),
                        server_name: status.name.clone(),
                        transport: status.transport,
                        tool_count: status.tool_count,
                        outcome: TelemetryOutcome::Ok,
                    },
                )
                .await;
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::McpServerConnected(code_agent_mcp::McpServerConnected {
                        server_name: status.name.clone(),
                        transport: status.transport,
                        protocol_version: status.protocol_version.clone().unwrap_or_default(),
                        tool_count: status.tool_count,
                        resource_count: status.resource_count,
                        prompt_count: status.prompt_count,
                        server_info: status.server_info.clone(),
                    }),
                )
                .await;
            }
            code_agent_mcp::McpConnectionStatus::Failed => {
                emit_telemetry(
                    &telemetry_sink,
                    AgentTelemetryEvent::McpServerFailed {
                        session_id: session_id.clone(),
                        run_id: cfg.run_id.clone(),
                        server_name: status.name.clone(),
                        transport: status.transport,
                        outcome: TelemetryOutcome::Error,
                        error_category: "mcp_server".into(),
                    },
                )
                .await;
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::McpServerFailed(code_agent_mcp::McpServerFailed {
                        server_name: status.name.clone(),
                        transport: status.transport,
                        error: status.error.clone().unwrap_or_else(|| "unknown".into()),
                    }),
                )
                .await;
            }
            _ => {}
        }
    }
    for discovered in &mcp_report.discovered {
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::McpToolsDiscovered {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                server_name: discovered.server_name.clone(),
                tool_count: discovered.tools.len(),
            },
        )
        .await;
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::McpToolsDiscovered(discovered.clone()),
        )
        .await;
    }
    for (server_name, log) in &mcp_report.logs {
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::McpServerLog(code_agent_mcp::McpServerLog {
                server_name: server_name.clone(),
                stream: log.stream,
                message: log.message.clone(),
                truncated: log.truncated,
            }),
        )
        .await;
    }
    if options.mcp.strict
        && mcp_report
            .statuses
            .iter()
            .any(|s| matches!(s.status, code_agent_mcp::McpConnectionStatus::Failed))
    {
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::Result(AgentResult {
                subtype: ResultSubtype::ErrorDuringExecution,
                result: Some("strict MCP mode: one or more MCP servers failed".into()),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                num_turns: 0,
                usage: None,
                total_cost_usd: None,
            }),
        )
        .await;
        return Ok(());
    }

    // M6: Register the built-in `Agent` tool when subagents are configured
    // and the parent has been granted access to it. The SubagentRuntime
    // holds shared state (provider, base tool registry, hooks, sinks) used
    // to spawn isolated child runs.
    let agent_tool_allowed = options.allowed_tools.iter().any(|n| n == "Agent")
        || options.allowed_tool_rules.iter().any(|r| {
            matches!(r,
                crate::tool_rule::ToolRule::ExactTool(n) if n == "Agent"
            )
        })
        || options
            .allowed_tool_rules
            .iter()
            .any(|r| matches!(r, crate::tool_rule::ToolRule::AgentNameGlob { .. }));
    let subagent_runtime = if options.agent_runtime.enabled
        && !options.agents.is_empty()
        && agent_tool_allowed
    {
        let runtime = crate::agents::SubagentRuntime::new(
            Arc::new(options.agents.clone()),
            provider.clone(),
            tools.clone(),
            options.clone(),
            hooks.clone(),
            options.agent_runtime.clone(),
        );
        let mut runtime = runtime.with_sinks(session_sink.clone(), telemetry_sink.clone(), pricing);
        if let Some(factory) = child_session_sink_factory.clone() {
            runtime = runtime.with_child_sink_factory(factory);
        }
        if let Some(handle) = checkpoint_handle.clone() {
            runtime = runtime.with_checkpoint_handle(handle);
        }
        runtime.set_parent_event_tx(Some(tx.clone())).await;
        let agent_tool = Arc::new(crate::agents::AgentTool::new(runtime.clone())) as Arc<dyn Tool>;
        tools.register(agent_tool);

        // Emit a one-shot AgentRegistryLoaded event so the CLI/UI can
        // show which subagents were registered.
        let summaries = options.agents.list();
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::AgentRegistryLoaded {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                agent_count: summaries.len(),
            },
        )
        .await;
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::AgentRegistryLoaded(crate::events::AgentRegistryLoaded {
                agents: summaries,
            }),
        )
        .await;
        Some(runtime)
    } else {
        None
    };
    let _ = subagent_runtime;

    // M7: Register the built-in `Skill` tool when Skills are configured
    // and the parent has been granted access to it. Skills are
    // never auto-approved by `acceptEdits`; the tool's permission kind is
    // `Orchestration`.
    let loaded_skills_state = crate::skills::LoadedSkillsState::new();
    let skill_tool_allowed = options
        .allowed_tools
        .iter()
        .any(|n| n == crate::skills::SKILL_TOOL_NAME)
        || options.allowed_tool_rules.iter().any(|r| {
            matches!(r,
                crate::tool_rule::ToolRule::ExactTool(n) if n == crate::skills::SKILL_TOOL_NAME
            )
        });
    if !options.skills.is_empty() && skill_tool_allowed {
        let registry_arc = Arc::new(options.skills.clone());
        let skill_tool = Arc::new(crate::skills::SkillTool::new(
            registry_arc.clone(),
            options.skill_options.clone(),
            loaded_skills_state.clone(),
        )) as Arc<dyn Tool>;
        tools.register(skill_tool);
        let summaries = registry_arc.list();
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::SkillRegistryLoaded(crate::events::SkillRegistryLoaded {
                skills: summaries,
            }),
        )
        .await;
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::SkillRegistryLoaded {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                skill_count: registry_arc.len(),
            },
        )
        .await;
    }

    if !options.slash_commands.list().is_empty() {
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::SlashCommandRegistryLoaded(crate::events::SlashCommandRegistryLoaded {
                commands: options.slash_commands.list(),
            }),
        )
        .await;
    }
    let _ = loaded_skills_state;

    // Build the catalog before the first model request so tool loading can
    // resolve deterministically from the complete local + MCP tool set.
    let provisional_allowed_tools = if options.allowed_tools.is_empty() {
        tools.clone()
    } else {
        tools.filter(&options.allowed_tools)
    };
    let provisional_catalog = build_tool_catalog(
        &provisional_allowed_tools,
        &options.tool_search,
        &mcp_report,
    );
    let loading_decision = provisional_catalog.resolve_loading_mode(&options.tool_search);

    let mut allowed_tool_patterns = options.allowed_tools.clone();
    if matches!(
        loading_decision.resolved_mode,
        ResolvedToolLoadingMode::Search
    ) && !allowed_tool_patterns.is_empty()
        && !allowed_tool_patterns
            .iter()
            .any(|name| name == SEARCH_TOOLS_NAME)
    {
        allowed_tool_patterns.push(SEARCH_TOOLS_NAME.into());
    }

    if matches!(
        loading_decision.resolved_mode,
        ResolvedToolLoadingMode::Search
    ) && tools.get(SEARCH_TOOLS_NAME).is_none()
    {
        tools.register(Arc::new(SearchToolsDefinitionTool::new(
            options.tool_search.max_results,
        )) as Arc<dyn Tool>);
    }

    // Build effective tool registry by allowed names.
    let mut allowed_tools = if allowed_tool_patterns.is_empty() {
        tools.clone()
    } else {
        tools.filter(&allowed_tool_patterns)
    };

    let catalog = build_tool_catalog(&allowed_tools, &options.tool_search, &mcp_report);
    let loading_decision = catalog.resolve_loading_mode(&options.tool_search);
    let mut loaded_state = catalog.initial_loaded_state(&options.tool_search);
    loaded_state.discovered_this_session.extend(
        options
            .tool_search
            .preloaded_discovered_tools
            .iter()
            .cloned(),
    );
    let visibility_context = options.tool_search.visibility_context(
        options.permission_mode,
        options.allowed_tool_rules.clone(),
        options.disallowed_tool_rules.clone(),
        options.allowed_tools.clone(),
        options.mcp.external_effect_policy,
    );
    let search_runtime = if matches!(
        loading_decision.resolved_mode,
        ResolvedToolLoadingMode::Search
    ) {
        let runtime = Arc::new(ToolSearchRuntime::new(
            catalog.clone(),
            loaded_state,
            options.tool_search.clone(),
            visibility_context,
        ));
        allowed_tools.register(Arc::new(SearchToolsTool::new(runtime.clone())) as Arc<dyn Tool>);
        Some(runtime)
    } else {
        None
    };

    emit_tool_catalog_events(
        &tx,
        &session_sink,
        &telemetry_sink,
        &session_id,
        &cfg.run_id,
        &catalog,
        &loading_decision,
        &search_runtime,
        &allowed_tools,
        &options,
    )
    .await;

    if cfg.emit_system_init {
        let init_tool_names =
            loaded_tool_names_for_turn(&allowed_tools, search_runtime.as_ref()).await;
        let init = SystemInit {
            session_id: session_id.clone(),
            cwd: options.cwd.clone(),
            model: options.model.clone(),
            tools: init_tool_names.into_iter().collect(),
            permission_mode: options.permission_mode,
            mcp_servers: mcp_report.statuses.clone(),
            skills: options.skills.list(),
            slash_commands: options.slash_commands.list(),
            plugins: options.plugins.summaries(),
        };
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::SystemInit(init),
        )
        .await;
    }

    // Construct system prompt.
    let mut system_text = String::new();
    if let Some(s) = &options.system_prompt {
        system_text.push_str(s);
    }
    if let Some(p) = &options.project_instructions {
        if !system_text.is_empty() {
            system_text.push_str("\n\n");
        }
        system_text.push_str("## Project Instructions (AGENTS.md)\n");
        system_text.push_str(p);
    }
    let mut system_text_owned = system_text;

    // Build conversation: prior history (from resume) plus the new user prompt.
    let mut conversation = Conversation::new();
    for m in initial_history {
        conversation.push(m);
    }

    // 0. UserPromptSubmit hooks: aggregate AddContext + Deny.
    let policy = PolicyEngine::new(
        PolicyConfig {
            allowed_tools: options.allowed_tool_rules.clone(),
            // The simple-name `allowed_tools` list is a registry filter for
            // most modes, but `dontAsk` historically also treats it as
            // implicit allow (M1 behavior). Pass it through so the engine
            // can honor that contract without affecting other modes.
            allowed_tool_names: options.allowed_tools.clone(),
            disallowed_tools: options.disallowed_tool_rules.clone(),
            permission_mode: options.permission_mode,
            callback: options.permission_callback.clone(),
            memory: approval_memory.clone(),
            memory_mode: options.approval_memory,
            mcp_external_effect_policy: options.mcp.external_effect_policy,
        },
        hooks.clone(),
    );

    let (decision, added_ctx) = policy
        .evaluate_user_prompt(&session_id, &cfg.run_id, &options.cwd, &prompt)
        .await;
    emit_hook_telemetry(
        &telemetry_sink,
        &session_id,
        &cfg.run_id,
        0,
        HookEventName::UserPromptSubmit,
        None,
        &decision,
    )
    .await;
    let _ = emit(
        &tx,
        &session_sink,
        &session_id,
        AgentEvent::HookEvaluated(HookEvaluated {
            event: HookEventName::UserPromptSubmit,
            tool_name: None,
            outcome: hook_outcome_kind(&decision, !added_ctx.0.is_empty()),
            reason: hook_decision_reason(&decision),
        }),
    )
    .await;

    if let HookAggregateDecision::Deny { reason } = &decision {
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::Result(AgentResult {
                subtype: ResultSubtype::ErrorDuringExecution,
                result: Some(format!("user prompt blocked: {reason}")),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                num_turns: 0,
                usage: None,
                total_cost_usd: None,
            }),
        )
        .await;
        // Stop hook with Error reason.
        run_stop_hook(
            &hooks,
            &telemetry_sink,
            &tx,
            &session_sink,
            &session_id,
            &cfg.run_id,
            &options.cwd,
            0,
            StopReason::Error,
        )
        .await;
        return Ok(());
    }

    if !added_ctx.0.is_empty() {
        if !system_text_owned.is_empty() {
            system_text_owned.push_str("\n\n");
        }
        system_text_owned.push_str("## Added Context (UserPromptSubmit hooks)\n");
        system_text_owned.push_str(&added_ctx.0);
    }

    if search_runtime.is_some() && options.tool_search.include_tool_category_hints {
        if !system_text_owned.is_empty() {
            system_text_owned.push_str("\n\n");
        }
        system_text_owned.push_str("## Tool Catalog Summary\n");
        system_text_owned.push_str(&catalog.compact_summary_text());
    }

    let system = if system_text_owned.is_empty() {
        None
    } else {
        Some(system_text_owned)
    };

    conversation.push(Message::user_text(prompt.clone()));

    let max_turns = options.max_turns.unwrap_or(8);

    let mut turns: u32 = 0;
    let mut total_usage = TokenUsage::default();
    let mut last_result_text: Option<String> = None;

    loop {
        if cfg.cancel.load(Ordering::Acquire) {
            handle_interrupt(
                &tx,
                &session_sink,
                &telemetry_sink,
                &session_id,
                &cfg.run_id,
                turns,
                last_result_text.clone(),
                total_usage.clone(),
                pricing.as_ref(),
                &hooks,
                &options.cwd,
            )
            .await;
            return Ok(());
        }

        if turns >= max_turns {
            let cost = pricing.as_ref().map(|p| p.estimate(&total_usage));
            let result = AgentResult {
                subtype: ResultSubtype::ErrorMaxTurns,
                result: last_result_text.clone(),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: cost,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
            run_stop_hook(
                &hooks,
                &telemetry_sink,
                &tx,
                &session_sink,
                &session_id,
                &cfg.run_id,
                &options.cwd,
                turns,
                StopReason::MaxTurns,
            )
            .await;
            return Ok(());
        }
        turns += 1;

        let turn_loaded_names =
            loaded_tool_names_for_turn(&allowed_tools, search_runtime.as_ref()).await;
        let tool_schemas = tool_schemas_for_names(&allowed_tools, &turn_loaded_names);
        let deferred_names = if let Some(runtime) = search_runtime.as_ref() {
            runtime.deferred_tool_names().await
        } else {
            BTreeSet::new()
        };
        let deferred_tool_schemas = tool_schemas_for_names(&allowed_tools, &deferred_names);
        emit_tool_budget_event(
            &tx,
            &session_sink,
            &telemetry_sink,
            &session_id,
            &cfg.run_id,
            turns,
            &catalog,
            &turn_loaded_names,
            &deferred_names,
            &options,
        )
        .await;

        let req = ModelRequest {
            model: options.model.clone(),
            system: system.clone(),
            messages: conversation.messages.clone(),
            tools: tool_schemas,
            deferred_tools: deferred_tool_schemas,
            tool_search_config: search_runtime.as_ref().map(|_| ToolSearchProviderConfig {
                native_mode: options.tool_search.native_provider_mode,
                local_search_enabled: true,
                strategy: options.tool_search.strategy,
            }),
            max_tokens: options.max_tokens,
        };

        let model_started = Instant::now();
        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::ModelRequestStarted {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                turn_index: turns,
                model: options.model.clone(),
            },
        )
        .await;

        let stream_result = tokio::select! {
            r = provider.stream_turn(req) => r,
            _ = cfg.cancel_notify.notified() => {
                handle_interrupt(
                    &tx,
                    &session_sink,
                    &telemetry_sink,
                    &session_id,
                    &cfg.run_id,
                    turns,
                    last_result_text.clone(),
                    total_usage.clone(),
                    pricing.as_ref(),
                    &hooks,
                    &options.cwd,
                )
                .await;
                return Ok(());
            }
        };
        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                emit_telemetry(
                    &telemetry_sink,
                    AgentTelemetryEvent::ModelRequestCompleted {
                        session_id: session_id.clone(),
                        run_id: cfg.run_id.clone(),
                        turn_index: turns,
                        model: options.model.clone(),
                        duration_ms: elapsed_ms(model_started),
                        usage: None,
                        cost_usd: None,
                        outcome: TelemetryOutcome::Error,
                        error_category: Some("provider".into()),
                    },
                )
                .await;
                let cost = pricing.as_ref().map(|p| p.estimate(&total_usage));
                let result = AgentResult {
                    subtype: ResultSubtype::ErrorDuringExecution,
                    result: Some(format!("provider error: {e}")),
                    session_id: session_id.clone(),
                    run_id: Some(cfg.run_id.clone()),
                    num_turns: turns,
                    usage: Some(total_usage.clone()),
                    total_cost_usd: cost,
                };
                emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
                run_stop_hook(
                    &hooks,
                    &telemetry_sink,
                    &tx,
                    &session_sink,
                    &session_id,
                    &cfg.run_id,
                    &options.cwd,
                    turns,
                    StopReason::Error,
                )
                .await;
                return Ok(());
            }
        };
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason: Option<String> = None;
        let mut turn_usage = TokenUsage::default();

        loop {
            let next = tokio::select! {
                e = stream.next() => e,
                _ = cfg.cancel_notify.notified() => {
                    cfg.cancel.store(true, Ordering::Release);
                    None
                }
            };
            let ev = match next {
                Some(e) => e,
                None => break,
            };
            if cfg.cancel.load(Ordering::Acquire) {
                break;
            }
            match ev {
                Ok(ModelEvent::MessageStart) => {}
                Ok(ModelEvent::TextDelta { text }) => {
                    text_buf.push_str(&text);
                    let _ = tx.send(AgentEvent::AssistantTextDelta(text)).await;
                }
                Ok(ModelEvent::ToolCall(c)) => {
                    tool_calls.push(c);
                }
                Ok(ModelEvent::MessageStop {
                    stop_reason: sr,
                    usage,
                }) => {
                    stop_reason = sr;
                    if let Some(u) = usage {
                        total_usage.add(&u);
                        turn_usage.add(&u);
                    }
                }
                Err(e) => {
                    emit_telemetry(
                        &telemetry_sink,
                        AgentTelemetryEvent::ModelRequestCompleted {
                            session_id: session_id.clone(),
                            run_id: cfg.run_id.clone(),
                            turn_index: turns,
                            model: options.model.clone(),
                            duration_ms: elapsed_ms(model_started),
                            usage: Some(total_usage.clone()),
                            cost_usd: pricing.as_ref().map(|p| p.estimate(&total_usage)),
                            outcome: TelemetryOutcome::Error,
                            error_category: Some("stream".into()),
                        },
                    )
                    .await;
                    let cost = pricing.as_ref().map(|p| p.estimate(&total_usage));
                    let result = AgentResult {
                        subtype: ResultSubtype::ErrorDuringExecution,
                        result: Some(format!("provider error: {e}")),
                        session_id: session_id.clone(),
                        run_id: Some(cfg.run_id.clone()),
                        num_turns: turns,
                        usage: Some(total_usage.clone()),
                        total_cost_usd: cost,
                    };
                    emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
                    run_stop_hook(
                        &hooks,
                        &telemetry_sink,
                        &tx,
                        &session_sink,
                        &session_id,
                        &cfg.run_id,
                        &options.cwd,
                        turns,
                        StopReason::Error,
                    )
                    .await;
                    return Ok(());
                }
            }
        }

        emit_telemetry(
            &telemetry_sink,
            AgentTelemetryEvent::ModelRequestCompleted {
                session_id: session_id.clone(),
                run_id: cfg.run_id.clone(),
                turn_index: turns,
                model: options.model.clone(),
                duration_ms: elapsed_ms(model_started),
                usage: Some(turn_usage.clone()),
                cost_usd: pricing.as_ref().map(|p| p.estimate(&turn_usage)),
                outcome: TelemetryOutcome::Ok,
                error_category: None,
            },
        )
        .await;

        if cfg.cancel.load(Ordering::Acquire) {
            handle_interrupt(
                &tx,
                &session_sink,
                &telemetry_sink,
                &session_id,
                &cfg.run_id,
                turns,
                last_result_text.clone(),
                total_usage.clone(),
                pricing.as_ref(),
                &hooks,
                &options.cwd,
            )
            .await;
            return Ok(());
        }

        // Build assistant message and append to conversation.
        let mut assistant_blocks: Vec<AssistantContent> = Vec::new();
        if !text_buf.is_empty() {
            assistant_blocks.push(AssistantContent::Text {
                text: text_buf.clone(),
            });
        }
        for c in &tool_calls {
            assistant_blocks.push(AssistantContent::ToolUse(c.clone()));
        }
        let assistant_msg = AssistantMessage {
            content: assistant_blocks.clone(),
            stop_reason: stop_reason.clone(),
        };
        emit(
            &tx,
            &session_sink,
            &session_id,
            AgentEvent::AssistantMessage(assistant_msg.clone()),
        )
        .await;

        let conv_blocks: Vec<ContentBlock> = assistant_blocks
            .iter()
            .map(|b| match b {
                AssistantContent::Text { text } => ContentBlock::Text { text: text.clone() },
                AssistantContent::ToolUse(c) => ContentBlock::ToolUse(c.clone()),
            })
            .collect();
        conversation.push(Message {
            role: MessageRole::Assistant,
            content: conv_blocks,
        });

        if !text_buf.is_empty() {
            last_result_text = Some(text_buf.clone());
        }

        // Plan mode: never execute tools. Treat first turn output as the final result.
        if matches!(options.permission_mode, PermissionMode::Plan) {
            let cost = pricing.as_ref().map(|p| p.estimate(&total_usage));
            let result = AgentResult {
                subtype: ResultSubtype::Success,
                result: last_result_text.clone().or(Some(String::new())),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: cost,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
            run_stop_hook(
                &hooks,
                &telemetry_sink,
                &tx,
                &session_sink,
                &session_id,
                &cfg.run_id,
                &options.cwd,
                turns,
                StopReason::Success,
            )
            .await;
            return Ok(());
        }

        if tool_calls.is_empty() {
            // Final answer.
            let cost = pricing.as_ref().map(|p| p.estimate(&total_usage));
            let result = AgentResult {
                subtype: ResultSubtype::Success,
                result: last_result_text.clone(),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: cost,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
            run_stop_hook(
                &hooks,
                &telemetry_sink,
                &tx,
                &session_sink,
                &session_id,
                &cfg.run_id,
                &options.cwd,
                turns,
                StopReason::Success,
            )
            .await;
            return Ok(());
        }

        // Execute tool calls in order. Append a single user message containing all tool_results.
        let mut tool_result_blocks: Vec<ContentBlock> = Vec::new();
        for call in &tool_calls {
            if cfg.cancel.load(Ordering::Acquire) {
                handle_interrupt(
                    &tx,
                    &session_sink,
                    &telemetry_sink,
                    &session_id,
                    &cfg.run_id,
                    turns,
                    last_result_text.clone(),
                    total_usage.clone(),
                    pricing.as_ref(),
                    &hooks,
                    &options.cwd,
                )
                .await;
                return Ok(());
            }

            emit(
                &tx,
                &session_sink,
                &session_id,
                AgentEvent::ToolCallStarted(ToolCallStarted { call: call.clone() }),
            )
            .await;

            if search_runtime.is_some() && !turn_loaded_names.contains(&call.name) {
                let reason = format!("tool not loaded: {}", call.name);
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::ToolCallDenied(ToolCallDenied {
                        tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        reason: reason.clone(),
                    }),
                )
                .await;
                let body = format!("Tool denied: {}", reason);
                push_tool_result_user(
                    &tx,
                    &session_sink,
                    &session_id,
                    &mut tool_result_blocks,
                    call,
                    &body,
                    true,
                )
                .await;
                continue;
            }

            let tool = match allowed_tools.get(&call.name) {
                Some(t) => t,
                None => {
                    let reason = format!("tool not allowed: {}", call.name);
                    emit(
                        &tx,
                        &session_sink,
                        &session_id,
                        AgentEvent::ToolCallDenied(ToolCallDenied {
                            tool_use_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            reason: reason.clone(),
                        }),
                    )
                    .await;
                    let body = format!("Tool denied: {}", reason);
                    push_tool_result_user(
                        &tx,
                        &session_sink,
                        &session_id,
                        &mut tool_result_blocks,
                        call,
                        &body,
                        true,
                    )
                    .await;
                    continue;
                }
            };

            let kind = tool.permission_kind();
            let tool_summary = tool.approval_summary(&call.input);
            let pctx = PolicyToolContext {
                session_id: &session_id,
                run_id: &cfg.run_id,
                turn_index: turns,
                cwd: &options.cwd,
                tool_name: &call.name,
                permission_kind: kind,
                tool_input: call.input.clone(),
                tool_summary,
                metadata: tool_metadata(&call.name, None),
            };
            let pdec = policy.evaluate_tool(pctx).await;

            // Emit policy decision event + telemetry.
            emit(
                &tx,
                &session_sink,
                &session_id,
                AgentEvent::PolicyDecision(PolicyDecisionEvent {
                    tool_name: call.name.clone(),
                    outcome: pdec.outcome,
                    source: pdec.source,
                    reason: pdec.reason.clone(),
                }),
            )
            .await;
            emit_telemetry(
                &telemetry_sink,
                AgentTelemetryEvent::PermissionEvaluated {
                    session_id: session_id.clone(),
                    run_id: cfg.run_id.clone(),
                    turn_index: turns,
                    tool_name: call.name.clone(),
                    outcome: pdec.outcome,
                    source: pdec.source,
                    reason: pdec.reason.clone(),
                },
            )
            .await;

            let effective_input = pdec
                .updated_tool_input
                .clone()
                .unwrap_or_else(|| call.input.clone());

            if !matches!(pdec.outcome, PolicyOutcome::Allow) {
                let reason = pdec.reason.clone().unwrap_or_else(|| "denied".to_string());
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::ToolCallDenied(ToolCallDenied {
                        tool_use_id: call.id.clone(),
                        tool_name: call.name.clone(),
                        reason: reason.clone(),
                    }),
                )
                .await;
                let body = format!("Tool denied: {}", reason);
                push_tool_result_user(
                    &tx,
                    &session_sink,
                    &session_id,
                    &mut tool_result_blocks,
                    call,
                    &body,
                    true,
                )
                .await;
                continue;
            }

            // M3: capture pre-mutation file state. The checkpoint handle is
            // free to also surface the Bash-not-tracked warning here.
            if let Some(ckpt) = &checkpoint_handle {
                if call.name == "Bash" {
                    // Read the command from `effective_input` (the
                    // policy- or hook-rewritten input that will actually
                    // execute), not from the raw `call.input`. Otherwise
                    // a hook that rewrites `cargo test` to `rm -rf .`
                    // would bypass the checkpoint Bash policy.
                    let cmd = effective_input
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    // "Explicit" means the user, or a stored user
                    // decision, said yes to this exact command. A broad
                    // allow rule (e.g. `Bash` on the simple-name list)
                    // doesn't count — that's the case M3 wants to flag.
                    let explicit = matches!(
                        pdec.source,
                        crate::policy::PolicyDecisionSource::ApprovalCallback
                            | crate::policy::PolicyDecisionSource::ApprovalMemory
                    );
                    let action = ckpt
                        .before_bash(&session_id, &cfg.run_id, cmd, explicit)
                        .await;
                    for ev in action.events {
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::CheckpointEvent(ev),
                        )
                        .await;
                    }
                    if let Some(reason) = action.deny {
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::ToolCallDenied(ToolCallDenied {
                                tool_use_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                reason: reason.clone(),
                            }),
                        )
                        .await;
                        let body = format!("Tool denied: {}", reason);
                        push_tool_result_user(
                            &tx,
                            &session_sink,
                            &session_id,
                            &mut tool_result_blocks,
                            call,
                            &body,
                            true,
                        )
                        .await;
                        continue;
                    }
                } else {
                    let result = ckpt
                        .capture_for_tool(&session_id, &cfg.run_id, &call.name, &effective_input)
                        .await;
                    for ev in result.events {
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::CheckpointEvent(ev),
                        )
                        .await;
                    }
                    if let Some(reason) = result.deny {
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::ToolCallDenied(ToolCallDenied {
                                tool_use_id: call.id.clone(),
                                tool_name: call.name.clone(),
                                reason: reason.clone(),
                            }),
                        )
                        .await;
                        let body = format!("Tool denied: checkpoint capture failed ({})", reason);
                        push_tool_result_user(
                            &tx,
                            &session_sink,
                            &session_id,
                            &mut tool_result_blocks,
                            call,
                            &body,
                            true,
                        )
                        .await;
                        continue;
                    }
                }
            }

            let ctx = ToolContext {
                cwd: options.cwd.clone(),
                session_id: session_id.clone(),
                run_id: Some(cfg.run_id.clone()),
                turn_index: turns,
                tool_call_id: Some(call.id.clone()),
            };
            let started = Instant::now();
            emit_telemetry(
                &telemetry_sink,
                AgentTelemetryEvent::ToolExecutionStarted {
                    session_id: session_id.clone(),
                    run_id: cfg.run_id.clone(),
                    turn_index: turns,
                    tool_name: call.name.clone(),
                },
            )
            .await;
            if let Some(parts) = code_agent_mcp::parse_agent_tool_name(&call.name) {
                emit_telemetry(
                    &telemetry_sink,
                    AgentTelemetryEvent::McpToolCallStarted {
                        session_id: session_id.clone(),
                        run_id: cfg.run_id.clone(),
                        server_name: parts.server_name.clone(),
                        tool_name: call.name.clone(),
                    },
                )
                .await;
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::McpToolCallStarted(code_agent_mcp::McpToolCallStarted {
                        server_name: parts.server_name,
                        remote_tool_name: parts.remote_tool_name,
                        agent_tool_name: call.name.clone(),
                    }),
                )
                .await;
            }
            if call.name == SEARCH_TOOLS_NAME {
                if let Some(started) =
                    tool_search_started_from_input(&effective_input, options.tool_search.strategy)
                {
                    emit_telemetry(
                        &telemetry_sink,
                        AgentTelemetryEvent::ToolSearchStarted {
                            session_id: session_id.clone(),
                            run_id: cfg.run_id.clone(),
                            turn_index: turns,
                            query: started.query.clone(),
                            strategy: started.strategy,
                            limit: started.limit,
                        },
                    )
                    .await;
                    emit(
                        &tx,
                        &session_sink,
                        &session_id,
                        AgentEvent::ToolSearchStarted(started),
                    )
                    .await;
                }
            }
            let exec = tool.execute(effective_input.clone(), ctx).await;
            let duration_ms = elapsed_ms(started);
            let (text, is_error, tool_result_obj) = match &exec {
                Ok(res) => (res.flatten_text(), res.is_error, Some(res.clone())),
                Err(ToolError::PermissionDenied(msg)) => {
                    emit(
                        &tx,
                        &session_sink,
                        &session_id,
                        AgentEvent::ToolCallDenied(ToolCallDenied {
                            tool_use_id: call.id.clone(),
                            tool_name: call.name.clone(),
                            reason: msg.clone(),
                        }),
                    )
                    .await;
                    (format!("Permission denied: {msg}"), true, None)
                }
                Err(e) => (format!("Tool error: {e}"), true, None),
            };

            match &exec {
                Ok(res) => {
                    if let Some(ev) = mcp_completion_from_result(&call.name, res, duration_ms) {
                        emit_telemetry(
                            &telemetry_sink,
                            AgentTelemetryEvent::McpToolCallCompleted {
                                session_id: session_id.clone(),
                                run_id: cfg.run_id.clone(),
                                server_name: ev.server_name.clone(),
                                tool_name: ev.agent_tool_name.clone(),
                                duration_ms: ev.duration_ms,
                                result_bytes: ev.result_bytes,
                                outcome: if ev.is_error {
                                    TelemetryOutcome::Error
                                } else {
                                    TelemetryOutcome::Ok
                                },
                            },
                        )
                        .await;
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::McpToolCallCompleted(ev),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    if let Some(ev) = mcp_failure_from_error(&call.name, &e.to_string()) {
                        emit_telemetry(
                            &telemetry_sink,
                            AgentTelemetryEvent::McpToolCallFailed {
                                session_id: session_id.clone(),
                                run_id: cfg.run_id.clone(),
                                server_name: ev.server_name.clone(),
                                tool_name: ev.agent_tool_name.clone(),
                                error_category: "mcp_tool".into(),
                            },
                        )
                        .await;
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::McpToolCallFailed(ev),
                        )
                        .await;
                    }
                }
            }

            if call.name == SEARCH_TOOLS_NAME {
                emit_search_tools_result_events(
                    &tx,
                    &session_sink,
                    &telemetry_sink,
                    &session_id,
                    &cfg.run_id,
                    turns,
                    &catalog,
                    &effective_input,
                    &exec,
                    options.tool_search.strategy,
                )
                .await;
            }

            // If the tool was TodoWrite, broadcast a TodoUpdate.
            if call.name == "TodoWrite" {
                if let Some(res) = &tool_result_obj {
                    if let Some(updated) = res
                        .metadata
                        .get("todo_list")
                        .and_then(|v| serde_json::from_value::<TodoList>(v.clone()).ok())
                    {
                        emit(
                            &tx,
                            &session_sink,
                            &session_id,
                            AgentEvent::TodoUpdate(crate::events::TodoUpdate { list: updated }),
                        )
                        .await;
                    }
                }
            }

            emit(
                &tx,
                &session_sink,
                &session_id,
                AgentEvent::ToolCallCompleted(ToolCallCompleted {
                    tool_use_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    is_error,
                    duration_ms,
                }),
            )
            .await;
            emit_telemetry(
                &telemetry_sink,
                AgentTelemetryEvent::ToolExecutionCompleted {
                    session_id: session_id.clone(),
                    run_id: cfg.run_id.clone(),
                    turn_index: turns,
                    tool_name: call.name.clone(),
                    duration_ms,
                    outcome: if is_error {
                        TelemetryOutcome::Error
                    } else {
                        TelemetryOutcome::Ok
                    },
                    error_category: if is_error { Some("tool".into()) } else { None },
                },
            )
            .await;

            // Post-tool-use hooks (success or failure).
            let post_event = if is_error {
                HookEventName::PostToolUseFailure
            } else {
                HookEventName::PostToolUse
            };
            if !hooks.is_empty() {
                let mut hi = HookInput::new(
                    post_event,
                    session_id.clone(),
                    options.cwd.clone(),
                    cfg.run_id.clone(),
                );
                hi.turn_index = turns;
                hi.tool_name = Some(call.name.clone());
                hi.tool_input = Some(effective_input.clone());
                hi.tool_result = tool_result_obj.clone();
                hi.metadata = tool_metadata(&call.name, None);
                let records = hooks.dispatch(&hi).await;
                let (agg, _) = aggregate(&records);
                emit(
                    &tx,
                    &session_sink,
                    &session_id,
                    AgentEvent::HookEvaluated(HookEvaluated {
                        event: post_event,
                        tool_name: Some(call.name.clone()),
                        outcome: hook_outcome_kind(&agg, false),
                        reason: hook_decision_reason(&agg),
                    }),
                )
                .await;
                emit_hook_telemetry(
                    &telemetry_sink,
                    &session_id,
                    &cfg.run_id,
                    turns,
                    post_event,
                    Some(&call.name),
                    &agg,
                )
                .await;
            }

            push_tool_result_user(
                &tx,
                &session_sink,
                &session_id,
                &mut tool_result_blocks,
                call,
                &text,
                is_error,
            )
            .await;
        }

        // PostToolBatch hook.
        if !hooks.is_empty() {
            let mut hi = HookInput::new(
                HookEventName::PostToolBatch,
                session_id.clone(),
                options.cwd.clone(),
                cfg.run_id.clone(),
            );
            hi.turn_index = turns;
            let records = hooks.dispatch(&hi).await;
            let (agg, _) = aggregate(&records);
            emit(
                &tx,
                &session_sink,
                &session_id,
                AgentEvent::HookEvaluated(HookEvaluated {
                    event: HookEventName::PostToolBatch,
                    tool_name: None,
                    outcome: hook_outcome_kind(&agg, false),
                    reason: hook_decision_reason(&agg),
                }),
            )
            .await;
            emit_hook_telemetry(
                &telemetry_sink,
                &session_id,
                &cfg.run_id,
                turns,
                HookEventName::PostToolBatch,
                None,
                &agg,
            )
            .await;
        }

        // Append the user message containing all tool results, then loop.
        conversation.push(Message {
            role: MessageRole::User,
            content: tool_result_blocks,
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_interrupt(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    sid: &SessionId,
    run_id: &RunId,
    turns: u32,
    last_result_text: Option<String>,
    usage: TokenUsage,
    pricing: Option<&ModelPricing>,
    hooks: &Hooks,
    cwd: &std::path::Path,
) {
    emit(
        tx,
        sink,
        sid,
        AgentEvent::Interrupted(InterruptedEvent {
            session_id: sid.clone(),
            run_id: Some(run_id.clone()),
            turn_index: turns,
        }),
    )
    .await;
    emit_telemetry(
        telemetry,
        AgentTelemetryEvent::AgentInterrupted {
            session_id: sid.clone(),
            run_id: run_id.clone(),
            turn_index: turns,
        },
    )
    .await;
    let cost = pricing.map(|p| p.estimate(&usage));
    emit(
        tx,
        sink,
        sid,
        AgentEvent::Result(AgentResult {
            subtype: ResultSubtype::Cancelled,
            result: last_result_text,
            session_id: sid.clone(),
            run_id: Some(run_id.clone()),
            num_turns: turns,
            usage: Some(usage),
            total_cost_usd: cost,
        }),
    )
    .await;
    run_stop_hook(
        hooks,
        telemetry,
        tx,
        sink,
        sid,
        run_id,
        cwd,
        turns,
        StopReason::Cancelled,
    )
    .await;
}

fn build_tool_catalog(
    registry: &ToolRegistry,
    options: &crate::tool_catalog::ToolSearchOptions,
    mcp_report: &McpRegistrationReport,
) -> ToolCatalog {
    let mut builder = ToolCatalogBuilder::new(options.clone()).ingest_registry(registry);
    for status in &mcp_report.statuses {
        builder = builder.mcp_transport(status.name.clone(), status.transport);
    }
    builder.build()
}

async fn loaded_tool_names_for_turn(
    registry: &ToolRegistry,
    runtime: Option<&Arc<ToolSearchRuntime>>,
) -> BTreeSet<String> {
    if let Some(runtime) = runtime {
        runtime.loaded_tool_names().await
    } else {
        registry.names().into_iter().collect()
    }
}

fn tool_schemas_for_names(registry: &ToolRegistry, names: &BTreeSet<String>) -> Vec<ToolSchema> {
    names
        .iter()
        .filter_map(|name| registry.get(name))
        .map(|tool| ToolSchema {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
async fn emit_tool_catalog_events(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    session_id: &SessionId,
    run_id: &RunId,
    catalog: &ToolCatalog,
    decision: &crate::tool_catalog::ToolLoadingDecision,
    runtime: &Option<Arc<ToolSearchRuntime>>,
    registry: &ToolRegistry,
    options: &AgentOptions,
) {
    let loaded = loaded_tool_names_for_turn(registry, runtime.as_ref()).await;
    let deferred = if let Some(runtime) = runtime {
        runtime.deferred_tool_names().await
    } else {
        BTreeSet::new()
    };
    let built = ToolCatalogBuilt {
        total_tools: catalog.len(),
        eager_tools: catalog
            .descriptors()
            .filter(|d| d.load_policy == ToolLoadPolicy::Always)
            .count(),
        deferred_tools: catalog
            .descriptors()
            .filter(|d| d.load_policy == ToolLoadPolicy::Deferred)
            .count(),
        disabled_tools: catalog
            .descriptors()
            .filter(|d| d.load_policy == ToolLoadPolicy::Disabled)
            .count(),
        estimated_total_tokens: catalog.estimated_total_tokens(),
    };
    emit(
        tx,
        sink,
        session_id,
        AgentEvent::ToolCatalogBuilt(built.clone()),
    )
    .await;
    emit_telemetry(
        telemetry,
        AgentTelemetryEvent::ToolCatalogBuilt {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            total_tool_count: built.total_tools,
            loaded_tool_count: loaded.len(),
            deferred_tool_count: deferred.len(),
            estimated_total_tool_tokens: built.estimated_total_tokens,
            outcome: TelemetryOutcome::Ok,
        },
    )
    .await;
    emit(
        tx,
        sink,
        session_id,
        AgentEvent::ToolLoadingModeResolved(ToolLoadingModeResolved {
            configured_mode: decision.configured_mode.clone(),
            resolved_mode: decision.resolved_mode,
            reason: decision.reason.clone(),
        }),
    )
    .await;
    emit_telemetry(
        telemetry,
        AgentTelemetryEvent::ToolLoadingModeResolved {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            configured_mode: decision.configured_mode.clone(),
            resolved_mode: decision.resolved_mode,
            reason: decision.reason.clone(),
        },
    )
    .await;
    for descriptor in catalog.descriptors() {
        match descriptor.load_policy {
            ToolLoadPolicy::Deferred => {
                emit(
                    tx,
                    sink,
                    session_id,
                    AgentEvent::ToolDeferred(ToolDeferred {
                        tool_name: descriptor.name.clone(),
                        source: descriptor.source.clone(),
                        estimated_tokens: descriptor.estimated_tokens,
                    }),
                )
                .await;
                emit_telemetry(
                    telemetry,
                    AgentTelemetryEvent::ToolDeferred {
                        session_id: session_id.clone(),
                        run_id: run_id.clone(),
                        tool_name: descriptor.name.clone(),
                        estimated_tokens: descriptor.estimated_tokens,
                    },
                )
                .await;
            }
            ToolLoadPolicy::Disabled => {
                emit(
                    tx,
                    sink,
                    session_id,
                    AgentEvent::ToolDisabled(ToolDisabled {
                        tool_name: descriptor.name.clone(),
                        source: descriptor.source.clone(),
                    }),
                )
                .await;
                emit_telemetry(
                    telemetry,
                    AgentTelemetryEvent::ToolDisabled {
                        session_id: session_id.clone(),
                        run_id: run_id.clone(),
                        tool_name: descriptor.name.clone(),
                    },
                )
                .await;
            }
            ToolLoadPolicy::Always => {
                if matches!(
                    decision.resolved_mode,
                    crate::tool_catalog::ResolvedToolLoadingMode::Search
                ) {
                    emit(
                        tx,
                        sink,
                        session_id,
                        AgentEvent::ToolLoaded(ToolLoaded {
                            tool_name: descriptor.name.clone(),
                            source: descriptor.source.clone(),
                            reason: ToolLoadReason::AlwaysLoaded,
                        }),
                    )
                    .await;
                }
            }
        }
    }
    if matches!(
        decision.resolved_mode,
        crate::tool_catalog::ResolvedToolLoadingMode::Search
    ) {
        tracing::info!(
            total_tools = built.total_tools,
            loaded_tools = loaded.len(),
            deferred_tools = deferred.len(),
            max_results = options.tool_search.max_results,
            strategy = %options.tool_search.strategy,
            "tool search enabled"
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_tool_budget_event(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    session_id: &SessionId,
    run_id: &RunId,
    turn_index: u32,
    catalog: &ToolCatalog,
    loaded_names: &BTreeSet<String>,
    deferred_names: &BTreeSet<String>,
    options: &AgentOptions,
) {
    let loaded_tokens = catalog.estimated_tokens_for(loaded_names);
    let total_tokens = catalog.estimated_total_tokens();
    let event = ToolContextBudgetUpdated {
        estimated_total_tool_tokens: total_tokens,
        estimated_loaded_tool_tokens: loaded_tokens,
        estimated_deferred_tool_tokens: total_tokens.saturating_sub(loaded_tokens),
        loaded_tool_count: loaded_names.len(),
        deferred_tool_count: deferred_names.len(),
    };
    emit(
        tx,
        sink,
        session_id,
        AgentEvent::ToolContextBudgetUpdated(event.clone()),
    )
    .await;
    emit_telemetry(
        telemetry,
        AgentTelemetryEvent::ToolContextBudgetUpdated {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_index,
            loaded_tool_count: event.loaded_tool_count,
            deferred_tool_count: event.deferred_tool_count,
            estimated_total_tool_tokens: event.estimated_total_tool_tokens,
            estimated_loaded_tool_tokens: event.estimated_loaded_tool_tokens,
            estimated_deferred_tool_tokens: event.estimated_deferred_tool_tokens,
        },
    )
    .await;
    let _ = options;
}

fn tool_search_started_from_input(
    input: &serde_json::Value,
    strategy: crate::tool_catalog::ToolSearchStrategy,
) -> Option<ToolSearchStarted> {
    let query = input.get("query").and_then(|v| v.as_str())?.to_string();
    let limit = input
        .get("limit")
        .and_then(|v| v.as_u64())
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(5);
    let source_filter = input
        .get("source_filter")
        .and_then(|v| v.as_str())
        .and_then(|s| ToolSourceFilter::parse(s).ok());
    Some(ToolSearchStarted {
        query,
        strategy,
        limit,
        source_filter,
    })
}

#[allow(clippy::too_many_arguments)]
async fn emit_search_tools_result_events(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    session_id: &SessionId,
    run_id: &RunId,
    turn_index: u32,
    catalog: &ToolCatalog,
    input: &serde_json::Value,
    exec: &Result<crate::tool::ToolResult, ToolError>,
    strategy: crate::tool_catalog::ToolSearchStrategy,
) {
    let query = input
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match exec {
        Ok(result) if !result.is_error => {
            let Some(metadata) = result.metadata.get("tool_search") else {
                return;
            };
            let matches: Vec<ToolSearchMatch> = metadata
                .get("matches")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let loaded: Vec<String> = metadata
                .get("loaded")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let duration_ms = metadata
                .get("duration_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            emit(
                tx,
                sink,
                session_id,
                AgentEvent::ToolSearchCompleted(ToolSearchCompleted {
                    query: query.clone(),
                    matches: matches.iter().map(ToolSearchMatchSummary::from).collect(),
                    duration_ms,
                }),
            )
            .await;
            emit_telemetry(
                telemetry,
                AgentTelemetryEvent::ToolSearchCompleted {
                    session_id: session_id.clone(),
                    run_id: run_id.clone(),
                    turn_index,
                    query: query.clone(),
                    strategy,
                    result_count: matches.len(),
                    loaded_tool_count: loaded.len(),
                    duration_ms,
                    outcome: TelemetryOutcome::Ok,
                },
            )
            .await;
            for tool_name in loaded {
                if let Some(descriptor) = catalog.get(&tool_name) {
                    emit(
                        tx,
                        sink,
                        session_id,
                        AgentEvent::ToolLoaded(ToolLoaded {
                            tool_name: tool_name.clone(),
                            source: descriptor.source.clone(),
                            reason: ToolLoadReason::SearchResult,
                        }),
                    )
                    .await;
                    emit_telemetry(
                        telemetry,
                        AgentTelemetryEvent::ToolLoaded {
                            session_id: session_id.clone(),
                            run_id: run_id.clone(),
                            turn_index: Some(turn_index),
                            tool_name,
                            reason: "search_result".into(),
                        },
                    )
                    .await;
                }
            }
        }
        Ok(result) => {
            emit_search_failed(
                tx,
                sink,
                telemetry,
                session_id,
                run_id,
                turn_index,
                &query,
                strategy,
                &result.flatten_text(),
            )
            .await;
        }
        Err(err) => {
            emit_search_failed(
                tx,
                sink,
                telemetry,
                session_id,
                run_id,
                turn_index,
                &query,
                strategy,
                &err.to_string(),
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_search_failed(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    session_id: &SessionId,
    run_id: &RunId,
    turn_index: u32,
    query: &str,
    strategy: crate::tool_catalog::ToolSearchStrategy,
    error: &str,
) {
    emit(
        tx,
        sink,
        session_id,
        AgentEvent::ToolSearchFailed(ToolSearchFailed {
            query: query.to_string(),
            error: error.to_string(),
        }),
    )
    .await;
    emit_telemetry(
        telemetry,
        AgentTelemetryEvent::ToolSearchFailed {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_index,
            query: query.to_string(),
            strategy,
            error_category: "tool_search".into(),
        },
    )
    .await;
}

#[allow(clippy::too_many_arguments)]
async fn run_stop_hook(
    hooks: &Hooks,
    telemetry: &Option<Arc<dyn TelemetrySink>>,
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    sid: &SessionId,
    run_id: &RunId,
    cwd: &std::path::Path,
    turns: u32,
    reason: StopReason,
) {
    if hooks.is_empty() {
        return;
    }
    let mut input = HookInput::new(
        HookEventName::Stop,
        sid.clone(),
        cwd.to_path_buf(),
        run_id.clone(),
    );
    input.turn_index = turns;
    input.stop_reason = Some(reason);
    let records = hooks.dispatch(&input).await;
    let (agg, _) = aggregate(&records);
    emit(
        tx,
        sink,
        sid,
        AgentEvent::HookEvaluated(HookEvaluated {
            event: HookEventName::Stop,
            tool_name: None,
            outcome: hook_outcome_kind(&agg, false),
            reason: hook_decision_reason(&agg),
        }),
    )
    .await;
    emit_hook_telemetry(
        telemetry,
        sid,
        run_id,
        turns,
        HookEventName::Stop,
        None,
        &agg,
    )
    .await;
}

fn hook_outcome_kind(d: &HookAggregateDecision, has_added_context: bool) -> HookOutcomeKind {
    match d {
        HookAggregateDecision::Allow { .. } => HookOutcomeKind::Allow,
        HookAggregateDecision::Deny { .. } => HookOutcomeKind::Deny,
        HookAggregateDecision::Ask { .. } => HookOutcomeKind::Ask,
        HookAggregateDecision::Continue if has_added_context => HookOutcomeKind::AddContext,
        HookAggregateDecision::Continue => HookOutcomeKind::Continue,
    }
}

fn hook_decision_reason(d: &HookAggregateDecision) -> Option<String> {
    match d {
        HookAggregateDecision::Allow { .. } => Some("allow".into()),
        HookAggregateDecision::Deny { reason } => Some(reason.clone()),
        HookAggregateDecision::Ask { reason } => Some(reason.clone()),
        HookAggregateDecision::Continue => None,
    }
}

async fn emit(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    sid: &SessionId,
    ev: AgentEvent,
) {
    if let Some(s) = sink {
        // Persistence errors must not crash the run, but they must not
        // disappear silently either — a buggy/internal caller could supply
        // an invalid `SessionId` (the public `From<String>` constructor
        // accepts arbitrary strings) and we'd otherwise complete a run
        // with no on-disk record. Surface the failure on the tracing
        // channel so it shows up in operator logs.
        if let Err(e) = s.record(sid, &ev).await {
            tracing::warn!(
                session_id = %sid,
                error = %e,
                "session sink failed to record event; continuing without persistence"
            );
        }
    }
    let _ = tx.send(ev).await;
}

async fn emit_telemetry(sink: &Option<Arc<dyn TelemetrySink>>, ev: AgentTelemetryEvent) {
    if let Some(s) = sink {
        s.record(&ev).await;
    }
}

async fn emit_hook_telemetry(
    sink: &Option<Arc<dyn TelemetrySink>>,
    session_id: &SessionId,
    run_id: &RunId,
    turn_index: u32,
    event: HookEventName,
    tool_name: Option<&str>,
    decision: &HookAggregateDecision,
) {
    let outcome = match decision {
        HookAggregateDecision::Allow { .. } => TelemetryOutcome::Allow,
        HookAggregateDecision::Deny { .. } => TelemetryOutcome::Deny,
        HookAggregateDecision::Ask { .. } => TelemetryOutcome::Ask,
        HookAggregateDecision::Continue => TelemetryOutcome::Continue,
    };
    let reason = match decision {
        HookAggregateDecision::Allow { .. } | HookAggregateDecision::Continue => None,
        HookAggregateDecision::Deny { reason } => Some(reason.clone()),
        HookAggregateDecision::Ask { reason } => Some(reason.clone()),
    };
    emit_telemetry(
        sink,
        AgentTelemetryEvent::HookEvaluated {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_index,
            hook_event: event,
            tool_name: tool_name.map(|s| s.to_string()),
            outcome,
            reason,
        },
    )
    .await;
}

async fn push_tool_result_user(
    tx: &mpsc::Sender<AgentEvent>,
    sink: &Option<Arc<dyn SessionSink>>,
    sid: &SessionId,
    blocks: &mut Vec<ContentBlock>,
    call: &ToolCall,
    text: &str,
    is_error: bool,
) {
    let result = ConvToolResult {
        tool_use_id: call.id.clone(),
        content: vec![ToolResultContent::Text {
            text: text.to_string(),
        }],
        is_error,
    };
    blocks.push(ContentBlock::ToolResult(result));

    let ev = AgentEvent::UserToolResult(UserToolResult {
        tool_use_id: call.id.clone(),
        tool_name: call.name.clone(),
        content: text.to_string(),
        is_error,
    });
    emit(tx, sink, sid, ev).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ToolError;
    use crate::permission::PermissionKind;
    use crate::tool::{ToolContext, ToolResult};

    struct StubTool {
        name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text(""))
        }
    }

    fn stub(name: &str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name: name.to_string(),
        })
    }

    /// tool_schemas_for_names yields schemas in lexicographic name order when
    /// driven through the same `registry.names()` -> `BTreeSet` -> schema
    /// pipeline the agent loop uses to build `ModelRequest.tools`. The
    /// Anthropic API caches the tools array as a prompt prefix, so any
    /// regression that desorts this path would silently invalidate the cache.
    #[tokio::test]
    async fn tool_schemas_for_names_matches_agent_request_path() {
        let mut registry = ToolRegistry::new();
        for n in ["zebra", "alpha", "mango", "bravo", "yak", "charlie"] {
            registry.register(stub(n));
        }

        // Mirror the agent loop: with no ToolSearchRuntime, loaded names come
        // from `registry.names()` collected into a BTreeSet, then passed to
        // `tool_schemas_for_names`. This is the exact call shape that
        // populates `ModelRequest.tools`.
        let loaded = loaded_tool_names_for_turn(&registry, None).await;
        let schemas = tool_schemas_for_names(&registry, &loaded);

        let observed: Vec<String> = schemas.iter().map(|s| s.name.clone()).collect();
        let mut sorted = observed.clone();
        sorted.sort();
        assert_eq!(
            observed, sorted,
            "ModelRequest.tools schema order must be lexicographic so the \
             Anthropic prompt prefix cache stays stable across resumed/forked sessions"
        );
        assert_eq!(
            observed,
            vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "charlie".to_string(),
                "mango".to_string(),
                "yak".to_string(),
                "zebra".to_string(),
            ]
        );
    }
}
