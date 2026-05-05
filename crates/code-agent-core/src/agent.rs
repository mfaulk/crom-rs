use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::stream::Stream;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::conversation::{
    AssistantContent, AssistantMessage, ContentBlock, Conversation, Message, MessageRole,
    SystemInit, ToolCall, ToolResult as ConvToolResult, ToolResultContent,
};
use crate::errors::{AgentError, ToolError};
use crate::events::{
    AgentEvent, AgentResult, HookEvaluated, HookOutcomeKind, InterruptedEvent, PolicyDecisionEvent,
    ResultSubtype, ToolCallCompleted, ToolCallDenied, ToolCallStarted, UserToolResult,
};
use crate::hooks::{aggregate, HookAggregateDecision, HookEventName, HookInput, Hooks, StopReason};
use crate::options::{AgentOptions, SessionId};
use crate::permission::PermissionMode;
use crate::policy::{ApprovalMemory, PolicyConfig, PolicyEngine, PolicyOutcome, PolicyToolContext};
use crate::provider::{ModelEvent, ModelProvider, ModelRequest, ToolSchema};
use crate::run_id::RunId;
use crate::telemetry::{AgentTelemetryEvent, ModelPricing, TelemetryOutcome};
use crate::todo::{TodoItem, TodoList};
use crate::tool::{ToolContext, ToolRegistry};
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
    } = agent;

    let session_id = options
        .session_id
        .clone()
        .unwrap_or_else(SessionId::default);

    // Build effective tool registry by allowed names.
    let allowed_tools = if options.allowed_tools.is_empty() {
        tools.clone()
    } else {
        tools.filter(&options.allowed_tools)
    };

    if cfg.emit_system_init {
        let init = SystemInit {
            session_id: session_id.clone(),
            cwd: options.cwd.clone(),
            model: options.model.clone(),
            tools: allowed_tools.names(),
            permission_mode: options.permission_mode,
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

    let system = if system_text_owned.is_empty() {
        None
    } else {
        Some(system_text_owned)
    };

    conversation.push(Message::user_text(prompt.clone()));

    let tool_schemas: Vec<ToolSchema> = allowed_tools
        .iter()
        .map(|t| ToolSchema {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        })
        .collect();

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

        let req = ModelRequest {
            model: options.model.clone(),
            system: system.clone(),
            messages: conversation.messages.clone(),
            tools: tool_schemas.clone(),
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

            let ctx = ToolContext {
                cwd: options.cwd.clone(),
                session_id: session_id.clone(),
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
        let _ = s.record(sid, &ev).await;
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
