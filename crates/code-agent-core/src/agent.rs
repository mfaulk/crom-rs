use std::pin::Pin;
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
    AgentEvent, AgentResult, ResultSubtype, ToolCallCompleted, ToolCallDenied, ToolCallStarted,
    UserToolResult,
};
use crate::options::{AgentOptions, SessionId};
use crate::permission::{
    AlwaysDeny, PermissionCallback, PermissionDecision, PermissionKind, PermissionMode,
    PermissionRequest,
};
use crate::provider::{ModelEvent, ModelProvider, ModelRequest, ToolSchema};
use crate::tool::{ToolContext, ToolRegistry};
use crate::usage::TokenUsage;

/// Trait for a session-store sink the agent can write records to without
/// taking a hard dependency on the session crate.
#[async_trait]
pub trait SessionSink: Send + Sync {
    async fn record(&self, session_id: &SessionId, event: &AgentEvent) -> Result<(), AgentError>;
}

/// Builder/runner for an agent. `query()` is a convenience over this.
pub struct Agent {
    options: AgentOptions,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    session_sink: Option<Arc<dyn SessionSink>>,
    initial_history: Vec<Message>,
}

#[derive(Default)]
pub struct AgentBuilder {
    options: Option<AgentOptions>,
    provider: Option<Arc<dyn ModelProvider>>,
    tools: Option<ToolRegistry>,
    session_sink: Option<Arc<dyn SessionSink>>,
    initial_history: Vec<Message>,
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

    pub fn initial_history(mut self, history: Vec<Message>) -> Self {
        self.initial_history = history;
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
            initial_history: self.initial_history,
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
        // Pin a session id before spawning so the error fallback below uses
        // the same id the loop reports on success. Without this, a failure
        // before the loop assigned its own id would surface a fresh random
        // id and break log correlation.
        if self.options.session_id.is_none() {
            self.options.session_id = Some(SessionId::new());
        }
        let session_id = self.options.session_id.clone().unwrap();
        let (tx, rx) = mpsc::channel::<AgentEvent>(64);
        tokio::spawn(async move {
            if let Err(e) = run_loop(self, prompt, tx.clone()).await {
                let _ = tx
                    .send(AgentEvent::Result(AgentResult {
                        subtype: ResultSubtype::ErrorDuringExecution,
                        result: Some(format!("agent error: {e}")),
                        session_id,
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

async fn run_loop(
    agent: Agent,
    prompt: String,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<(), AgentError> {
    let Agent {
        options,
        provider,
        tools,
        session_sink,
        initial_history,
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
    let system = if system_text.is_empty() {
        None
    } else {
        Some(system_text)
    };

    // Build conversation: prior history (from resume) plus the new user prompt.
    let mut conversation = Conversation::new();
    for m in initial_history {
        conversation.push(m);
    }
    conversation.push(Message::user_text(prompt));

    let tool_schemas: Vec<ToolSchema> = allowed_tools
        .iter()
        .map(|t| ToolSchema {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        })
        .collect();

    let max_turns = options.max_turns.unwrap_or(8);
    let permission_callback: Arc<dyn PermissionCallback> = options
        .permission_callback
        .clone()
        .unwrap_or_else(|| Arc::new(AlwaysDeny));

    let mut turns: u32 = 0;
    let mut total_usage = TokenUsage::default();
    let mut last_result_text: Option<String> = None;

    loop {
        if turns >= max_turns {
            let result = AgentResult {
                subtype: ResultSubtype::ErrorMaxTurns,
                result: last_result_text.clone(),
                session_id: session_id.clone(),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: None,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
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

        let mut stream = provider.stream_turn(req).await?;
        let mut text_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason: Option<String> = None;

        while let Some(ev) = stream.next().await {
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
                    }
                }
                Err(e) => {
                    let result = AgentResult {
                        subtype: ResultSubtype::ErrorDuringExecution,
                        result: Some(format!("provider error: {e}")),
                        session_id: session_id.clone(),
                        num_turns: turns,
                        usage: Some(total_usage.clone()),
                        total_cost_usd: None,
                    };
                    emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
                    return Ok(());
                }
            }
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

        // Mirror assistant blocks into the conversation as a Message.
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
            let result = AgentResult {
                subtype: ResultSubtype::Success,
                result: last_result_text.clone().or(Some(String::new())),
                session_id: session_id.clone(),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: None,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
            return Ok(());
        }

        if tool_calls.is_empty() {
            // Final answer.
            let result = AgentResult {
                subtype: ResultSubtype::Success,
                result: last_result_text.clone(),
                session_id: session_id.clone(),
                num_turns: turns,
                usage: Some(total_usage.clone()),
                total_cost_usd: None,
            };
            emit(&tx, &session_sink, &session_id, AgentEvent::Result(result)).await;
            return Ok(());
        }

        // Execute tool calls in order. Append a single user message containing all tool_results.
        let mut tool_result_blocks: Vec<ContentBlock> = Vec::new();
        for call in &tool_calls {
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
            let needs_approval =
                needs_approval(options.permission_mode, kind, &options, &call.name);
            if needs_approval {
                let req = PermissionRequest {
                    tool_name: call.name.clone(),
                    permission_kind: kind,
                    input: call.input.clone(),
                    summary: tool.approval_summary(&call.input),
                };
                let decision = permission_callback.approve(req).await;
                if let PermissionDecision::Deny { reason } = decision {
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
                    let body = format!("Tool denied by user: {}", reason);
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

            let ctx = ToolContext {
                cwd: options.cwd.clone(),
                session_id: session_id.clone(),
            };
            let started = Instant::now();
            let exec = tool.execute(call.input.clone(), ctx).await;
            let duration_ms = started.elapsed().as_millis() as u64;
            let (text, is_error) = match exec {
                Ok(res) => (res.flatten_text(), res.is_error),
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
                    (format!("Permission denied: {msg}"), true)
                }
                Err(e) => (format!("Tool error: {e}"), true),
            };

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

        // Append the user message containing all tool results, then loop.
        conversation.push(Message {
            role: MessageRole::User,
            content: tool_result_blocks,
        });
    }
}

fn needs_approval(
    mode: PermissionMode,
    kind: PermissionKind,
    options: &AgentOptions,
    tool_name: &str,
) -> bool {
    match mode {
        PermissionMode::Default => !matches!(kind, PermissionKind::ReadOnly),
        PermissionMode::AcceptEdits => matches!(kind, PermissionKind::Execution),
        PermissionMode::Plan => true,
        PermissionMode::DontAsk => !options.allowed_tools.iter().any(|n| n == tool_name),
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
