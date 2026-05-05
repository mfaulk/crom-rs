//! Long-lived `AgentClient`: an interactive runtime for the agent loop.
//!
//! The client owns an in-memory conversation, a queue of pending user
//! messages, an active-run cancellation flag, and a background task that
//! drives runs sequentially. Multiple user prompts can be sent into the
//! same session; queued messages are processed once the current run
//! completes. Interrupting the current run does not lose queued messages.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::stream::Stream;
use tokio::sync::{mpsc, Mutex};
use tokio_stream::wrappers::ReceiverStream;

use crate::agent::{run_loop, Agent, AgentBuilder, RunConfig, SessionSink, TelemetrySink};
use crate::conversation::{Message, MessageRole};
use crate::errors::AgentError;
use crate::events::{AgentEvent, AgentResult, ResultSubtype};
use crate::hooks::Hooks;
use crate::options::{AgentOptions, SessionId};
use crate::permission::PermissionMode;
use crate::policy::ApprovalMemory;
use crate::provider::ModelProvider;
use crate::run_id::RunId;
use crate::telemetry::ModelPricing;
use crate::tool::ToolRegistry;

/// How the client should start the session.
#[derive(Debug, Clone, Default)]
pub enum SessionStart {
    #[default]
    /// Brand-new session.
    New,
    /// Resume the named session id (caller must supply prior history via the
    /// builder).
    Resume(SessionId),
    /// Continue the latest session for the cwd (caller resolves the id and
    /// supplies prior history via the builder).
    ContinueLatestForCwd,
    /// Fork from another session id (caller copies history via the builder).
    Fork(SessionId),
}

/// Builder for `AgentClient`.
#[derive(Default)]
pub struct AgentClientBuilder {
    cwd: Option<PathBuf>,
    model: Option<String>,
    permission_mode: Option<PermissionMode>,
    allowed_tools: Vec<String>,
    options: Option<AgentOptions>,
    provider: Option<Arc<dyn ModelProvider>>,
    tools: Option<ToolRegistry>,
    session_sink: Option<Arc<dyn SessionSink>>,
    telemetry_sink: Option<Arc<dyn TelemetrySink>>,
    initial_history: Vec<Message>,
    hooks: Hooks,
    pricing: Option<ModelPricing>,
    session_start: SessionStart,
    forced_session_id: Option<SessionId>,
    source_session_id: Option<SessionId>,
}

impl AgentClientBuilder {
    pub fn new() -> Self {
        Self {
            session_start: SessionStart::New,
            ..Default::default()
        }
    }

    pub fn cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = Some(cwd);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn permission_mode(mut self, mode: PermissionMode) -> Self {
        self.permission_mode = Some(mode);
        self
    }

    pub fn allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_tools = tools.into_iter().map(Into::into).collect();
        self
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

    pub fn session_start(mut self, start: SessionStart) -> Self {
        self.session_start = start;
        self
    }

    /// Override the session id (e.g. when an external session store has
    /// already minted one).
    pub fn session_id(mut self, id: SessionId) -> Self {
        self.forced_session_id = Some(id);
        self
    }

    /// Record a "forked from" reference in events for an explicit fork.
    pub fn source_session_id(mut self, id: SessionId) -> Self {
        self.source_session_id = Some(id);
        self
    }

    pub async fn build(self) -> Result<AgentClient, AgentError> {
        let provider = self
            .provider
            .ok_or_else(|| AgentError::InvalidConfig("ModelProvider is required".into()))?;
        let tools = self.tools.unwrap_or_default();

        let mut options = self.options.unwrap_or_default();
        if let Some(c) = self.cwd {
            options.cwd = c;
        }
        if let Some(m) = self.model {
            options.model = m;
        }
        if let Some(pm) = self.permission_mode {
            options.permission_mode = pm;
        }
        if !self.allowed_tools.is_empty() {
            options.allowed_tools = self.allowed_tools;
        }

        let session_id = self
            .forced_session_id
            .or_else(|| options.session_id.clone())
            .unwrap_or_default();
        options.session_id = Some(session_id.clone());

        let approval_memory = ApprovalMemory::new();

        let inner = ClientInner {
            options: Mutex::new(options),
            provider,
            tools,
            session_sink: self.session_sink,
            telemetry_sink: self.telemetry_sink,
            initial_history: Mutex::new(self.initial_history),
            hooks: self.hooks,
            pricing: self.pricing,
            queue: Mutex::new(VecDeque::new()),
            cancel: Mutex::new(None),
            approval_memory,
            session_id: session_id.clone(),
        };

        let inner = Arc::new(inner);
        let (event_tx, event_rx) = mpsc::channel::<AgentEvent>(128);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCommand>(32);
        let runtime_inner = inner.clone();
        let handle = tokio::spawn(async move {
            ClientRuntime {
                inner: runtime_inner,
                event_tx,
                cmd_rx,
                shutting_down: false,
            }
            .run()
            .await;
        });

        Ok(AgentClient {
            inner,
            cmd_tx,
            event_rx,
            session_id,
            handle: Some(handle),
            source_session_id: self.source_session_id,
            session_start: self.session_start,
        })
    }
}

/// Top-level long-lived client.
pub struct AgentClient {
    // Held for the runtime's lifetime so the inner state stays alive even
    // after the AgentClient handle goes out of scope before the runtime
    // task settles.
    #[allow(dead_code)]
    inner: Arc<ClientInner>,
    cmd_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::Receiver<AgentEvent>,
    session_id: SessionId,
    handle: Option<tokio::task::JoinHandle<()>>,
    source_session_id: Option<SessionId>,
    session_start: SessionStart,
}

impl AgentClient {
    pub fn builder() -> AgentClientBuilder {
        AgentClientBuilder::new()
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn source_session_id(&self) -> Option<&SessionId> {
        self.source_session_id.as_ref()
    }

    pub fn session_start(&self) -> &SessionStart {
        &self.session_start
    }

    /// Send a user message into the live session. If the runtime is busy the
    /// message is queued and processed after the current run completes.
    pub async fn send_user_message(&mut self, prompt: impl Into<String>) -> Result<(), AgentError> {
        let prompt = prompt.into();
        self.cmd_tx
            .send(ClientCommand::UserMessage(prompt))
            .await
            .map_err(|_| AgentError::other("client runtime ended"))
    }

    /// Receive the next event from the live session, awaiting if needed.
    pub async fn next_event(&mut self) -> Result<Option<AgentEvent>, AgentError> {
        Ok(self.event_rx.recv().await)
    }

    /// Take the event stream out of the client. Subsequent `next_event` calls
    /// will return None.
    pub fn into_event_stream(mut self) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>> {
        let (_dummy_tx, dummy_rx) = mpsc::channel::<AgentEvent>(1);
        let rx = std::mem::replace(&mut self.event_rx, dummy_rx);
        Box::pin(ReceiverStream::new(rx))
    }

    /// Cancel the current run cleanly. Queued messages are preserved; the
    /// next `send_user_message` (or already-queued message) will start a new
    /// run as soon as the current one tears down.
    pub async fn interrupt(&mut self) -> Result<(), AgentError> {
        self.cmd_tx
            .send(ClientCommand::Interrupt)
            .await
            .map_err(|_| AgentError::other("client runtime ended"))
    }

    /// Change the permission mode for subsequent runs.
    pub async fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<(), AgentError> {
        self.cmd_tx
            .send(ClientCommand::SetPermissionMode(mode))
            .await
            .map_err(|_| AgentError::other("client runtime ended"))
    }

    /// Shut down the runtime. Pending queued messages are dropped.
    pub async fn shutdown(mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(ClientCommand::Shutdown).await;
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
        Ok(())
    }
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            // Best-effort: tell the runtime to exit. We don't await the join
            // handle in Drop because it'd be unsound to block here.
            let _ = self.cmd_tx.try_send(ClientCommand::Shutdown);
            h.abort();
        }
    }
}

struct ClientInner {
    options: Mutex<AgentOptions>,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    session_sink: Option<Arc<dyn SessionSink>>,
    telemetry_sink: Option<Arc<dyn TelemetrySink>>,
    initial_history: Mutex<Vec<Message>>,
    hooks: Hooks,
    pricing: Option<ModelPricing>,
    queue: Mutex<VecDeque<String>>,
    cancel: Mutex<Option<CancelHandle>>,
    approval_memory: ApprovalMemory,
    session_id: SessionId,
}

#[derive(Clone)]
struct CancelHandle {
    flag: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl CancelHandle {
    fn fire(&self) {
        self.flag.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

enum ClientCommand {
    UserMessage(String),
    Interrupt,
    SetPermissionMode(PermissionMode),
    Shutdown,
}

struct ClientRuntime {
    inner: Arc<ClientInner>,
    event_tx: mpsc::Sender<AgentEvent>,
    cmd_rx: mpsc::Receiver<ClientCommand>,
    shutting_down: bool,
}

impl ClientRuntime {
    async fn run(mut self) {
        let mut emitted_init = false;
        loop {
            if self.shutting_down {
                return;
            }
            let cmd = match self.cmd_rx.recv().await {
                Some(c) => c,
                None => return,
            };
            match cmd {
                ClientCommand::UserMessage(p) => {
                    self.inner.queue.lock().await.push_back(p);
                    self.drain_queue(&mut emitted_init).await;
                    if self.shutting_down {
                        return;
                    }
                }
                ClientCommand::Interrupt => {
                    if let Some(c) = self.inner.cancel.lock().await.as_ref() {
                        c.fire();
                    }
                }
                ClientCommand::SetPermissionMode(mode) => {
                    self.inner.options.lock().await.permission_mode = mode;
                }
                ClientCommand::Shutdown => {
                    if let Some(c) = self.inner.cancel.lock().await.as_ref() {
                        c.fire();
                    }
                    return;
                }
            }
        }
    }

    async fn drain_queue(&mut self, emitted_init: &mut bool) {
        loop {
            if self.shutting_down {
                return;
            }
            let prompt = {
                let mut q = self.inner.queue.lock().await;
                match q.pop_front() {
                    Some(p) => p,
                    None => return,
                }
            };
            self.run_one(prompt, emitted_init).await;

            // Between runs, eagerly drain any non-message commands the user
            // sent (interrupt during idle, set_permission_mode, shutdown).
            while let Ok(cmd) = self.cmd_rx.try_recv() {
                match cmd {
                    ClientCommand::UserMessage(p) => {
                        self.inner.queue.lock().await.push_back(p);
                    }
                    ClientCommand::Interrupt => {
                        // No active run; nothing to interrupt.
                    }
                    ClientCommand::SetPermissionMode(mode) => {
                        self.inner.options.lock().await.permission_mode = mode;
                    }
                    ClientCommand::Shutdown => {
                        self.shutting_down = true;
                        return;
                    }
                }
            }
        }
    }

    async fn run_one(&mut self, prompt: String, emitted_init: &mut bool) {
        let cancel_handle = CancelHandle {
            flag: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        *self.inner.cancel.lock().await = Some(cancel_handle.clone());

        let options_snapshot = self.inner.options.lock().await.clone();
        let initial_history = self.inner.initial_history.lock().await.clone();

        let agent_result = AgentBuilder::new()
            .options(options_snapshot.clone())
            .provider(self.inner.provider.clone())
            .tools(self.inner.tools.clone())
            .hooks(self.inner.hooks.clone())
            .approval_memory(self.inner.approval_memory.clone())
            .initial_history(initial_history.clone())
            .session_sink_opt(self.inner.session_sink.clone())
            .telemetry_sink_opt(self.inner.telemetry_sink.clone())
            .pricing_opt(self.inner.pricing)
            .build();
        let agent = match agent_result {
            Ok(a) => a,
            Err(e) => {
                let _ = self
                    .event_tx
                    .send(AgentEvent::Result(AgentResult {
                        subtype: ResultSubtype::ErrorDuringExecution,
                        result: Some(format!("agent build failed: {e}")),
                        session_id: self.inner.session_id.clone(),
                        run_id: None,
                        num_turns: 0,
                        usage: None,
                        total_cost_usd: None,
                    }))
                    .await;
                return;
            }
        };

        let (run_tx, mut run_rx) = mpsc::channel::<AgentEvent>(64);
        let cfg = RunConfig {
            run_id: RunId::new(),
            cancel: cancel_handle.flag.clone(),
            cancel_notify: cancel_handle.notify.clone(),
            emit_system_init: !*emitted_init,
            resumed_session: !initial_history.is_empty(),
        };
        *emitted_init = true;
        // Append the user prompt to history BEFORE spawning the run so that
        // assistant/tool-result events the loop emits during the run land
        // after the prompt in the conversation log. (Otherwise we'd persist
        // out-of-order history and corrupt context for later prompts.)
        {
            let mut hist = self.inner.initial_history.lock().await;
            hist.push(Message::user_text(prompt.clone()));
        }
        let task = tokio::spawn(async move {
            // Discard error: the loop already emits its own Result event.
            let _ = run_loop(agent, prompt, run_tx, cfg).await;
        });

        // Forward events. We also collect the final assistant turn's content
        // into initial_history (via the AgentEvent::AssistantMessage stream)
        // so subsequent runs in the same session keep context.
        //
        // While the run is in flight we also poll command channel so the
        // user can interrupt, change permission mode, or queue another
        // message without waiting for this run to finish.
        loop {
            let ev = tokio::select! {
                e = run_rx.recv() => match e {
                    Some(e) => e,
                    None => break,
                },
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(ClientCommand::Interrupt) => {
                        if let Some(c) = self.inner.cancel.lock().await.as_ref() {
                            c.fire();
                        }
                        continue;
                    }
                    Some(ClientCommand::UserMessage(p)) => {
                        self.inner.queue.lock().await.push_back(p);
                        continue;
                    }
                    Some(ClientCommand::SetPermissionMode(mode)) => {
                        self.inner.options.lock().await.permission_mode = mode;
                        continue;
                    }
                    Some(ClientCommand::Shutdown) => {
                        if let Some(c) = self.inner.cancel.lock().await.as_ref() {
                            c.fire();
                        }
                        self.shutting_down = true;
                        continue;
                    }
                    None => break,
                }
            };
            // Capture assistant message into initial_history.
            if let AgentEvent::AssistantMessage(am) = &ev {
                let mut hist = self.inner.initial_history.lock().await;
                let blocks: Vec<crate::conversation::ContentBlock> = am
                    .content
                    .iter()
                    .map(|b| match b {
                        crate::conversation::AssistantContent::Text { text } => {
                            crate::conversation::ContentBlock::Text { text: text.clone() }
                        }
                        crate::conversation::AssistantContent::ToolUse(c) => {
                            crate::conversation::ContentBlock::ToolUse(c.clone())
                        }
                    })
                    .collect();
                hist.push(Message {
                    role: MessageRole::Assistant,
                    content: blocks,
                });
            }
            if let AgentEvent::UserToolResult(r) = &ev {
                let mut hist = self.inner.initial_history.lock().await;
                // Append a user message containing this tool result. We
                // coalesce consecutive tool-result user messages so the
                // structure mirrors the live conversation.
                let new_block = crate::conversation::ContentBlock::ToolResult(
                    crate::conversation::ToolResult {
                        tool_use_id: r.tool_use_id.clone(),
                        content: vec![crate::conversation::ToolResultContent::Text {
                            text: r.content.clone(),
                        }],
                        is_error: r.is_error,
                    },
                );
                let coalesced = if let Some(last) = hist.last_mut() {
                    if matches!(last.role, MessageRole::User)
                        && last
                            .content
                            .iter()
                            .all(|b| matches!(b, crate::conversation::ContentBlock::ToolResult(_)))
                    {
                        last.content.push(new_block.clone());
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if !coalesced {
                    hist.push(Message {
                        role: MessageRole::User,
                        content: vec![new_block],
                    });
                }
                drop(hist);
                // Still forward the event so AgentClient consumers can see
                // tool-result payloads, not just the started/completed pair.
            }
            let _ = self.event_tx.send(ev).await;
        }
        let _ = task.await;
        *self.inner.cancel.lock().await = None;
    }
}

// Helper traits to allow optional sink configuration on AgentBuilder via a
// fluent API in the runtime — the public AgentBuilder doesn't have these
// `_opt` methods, so we add them here as a private extension trait.
trait AgentBuilderExt {
    fn session_sink_opt(self, sink: Option<Arc<dyn SessionSink>>) -> Self;
    fn telemetry_sink_opt(self, sink: Option<Arc<dyn TelemetrySink>>) -> Self;
    fn pricing_opt(self, pricing: Option<ModelPricing>) -> Self;
}

impl AgentBuilderExt for AgentBuilder {
    fn session_sink_opt(self, sink: Option<Arc<dyn SessionSink>>) -> Self {
        if let Some(s) = sink {
            self.session_sink(s)
        } else {
            self
        }
    }
    fn telemetry_sink_opt(self, sink: Option<Arc<dyn TelemetrySink>>) -> Self {
        if let Some(s) = sink {
            self.telemetry_sink(s)
        } else {
            self
        }
    }
    fn pricing_opt(self, pricing: Option<ModelPricing>) -> Self {
        if let Some(p) = pricing {
            self.pricing(p)
        } else {
            self
        }
    }
}

// The private re-export above makes `Agent` reachable from this module.
#[allow(dead_code)]
fn _agent_used(_a: Agent) {}
