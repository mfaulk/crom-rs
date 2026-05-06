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

use crate::agent::{run_loop, Agent, AgentBuilder, RunConfig, SessionSink, TelemetrySink};
use crate::agents::{AgentDefinition, AgentRegistry, AgentRuntimeOptions};
use crate::checkpoint::{CheckpointApi, CheckpointHandle};
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
use crate::tool_catalog::{
    LoadedToolState, SearchToolsDefinitionTool, ToolCatalog, ToolCatalogBuilder,
    ToolCatalogSummary, ToolContextBudget, ToolDescriptor, ToolDescriptorPreview, ToolListFilter,
    ToolLoadingMode, ToolSearchOptions, ToolSearchQuery, ToolSearchResult, SEARCH_TOOLS_NAME,
};

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
    checkpoint_handle: Option<Arc<dyn CheckpointHandle>>,
    checkpoint_api: Option<Arc<dyn CheckpointApi>>,
    mcp_options: Option<code_agent_mcp::McpOptions>,
    mcp_config_file: Option<PathBuf>,
    tool_search_options: Option<ToolSearchOptions>,
    tool_loading_mode: Option<ToolLoadingMode>,
    child_session_sink_factory: Option<Arc<dyn crate::agents::ChildSessionSinkFactory>>,
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

    /// Attach a checkpoint handle. The handle is consulted before each
    /// mutating tool call.
    pub fn checkpoint_handle(mut self, handle: Arc<dyn CheckpointHandle>) -> Self {
        self.checkpoint_handle = Some(handle);
        self
    }

    /// Attach a checkpoint API used by `AgentClient::create_checkpoint`,
    /// `list_checkpoints`, `diff_checkpoint`, and `rewind_files`.
    pub fn checkpoint_api(mut self, api: Arc<dyn CheckpointApi>) -> Self {
        self.checkpoint_api = Some(api);
        self
    }

    pub fn mcp_options(mut self, options: code_agent_mcp::McpOptions) -> Self {
        self.mcp_options = Some(options);
        self
    }

    pub fn mcp_config_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.mcp_config_file = Some(path.into());
        self
    }

    pub fn tool_search_options(mut self, options: ToolSearchOptions) -> Self {
        self.tool_search_options = Some(options);
        self
    }

    pub fn tool_loading_mode(mut self, mode: ToolLoadingMode) -> Self {
        self.tool_loading_mode = Some(mode);
        self
    }

    /// Register a single subagent definition. Returns an error if validation
    /// fails or a same-origin definition with that name already exists.
    pub fn agent(mut self, definition: AgentDefinition) -> Result<Self, AgentError> {
        let opts = self.options.get_or_insert_with(AgentOptions::default);
        opts.agents
            .register(definition)
            .map_err(|e| AgentError::InvalidConfig(e.to_string()))?;
        Ok(self)
    }

    /// Replace the entire agent registry. Convenient for callers that
    /// already have one (e.g. from `load_filesystem_agents`).
    pub fn agents(mut self, registry: AgentRegistry) -> Self {
        let opts = self.options.get_or_insert_with(AgentOptions::default);
        opts.agents = registry;
        self
    }

    /// Override the subagent runtime knobs.
    pub fn agent_runtime(mut self, options: AgentRuntimeOptions) -> Self {
        let opts = self.options.get_or_insert_with(AgentOptions::default);
        opts.agent_runtime = options;
        self
    }

    /// Attach a factory that mints session sinks for child subagent runs.
    pub fn child_session_sink_factory(
        mut self,
        factory: Arc<dyn crate::agents::ChildSessionSinkFactory>,
    ) -> Self {
        self.child_session_sink_factory = Some(factory);
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
        if let Some(mcp) = self.mcp_options {
            options.mcp = mcp;
        }
        if let Some(path) = self.mcp_config_file {
            options.mcp = code_agent_mcp::load_mcp_options(
                &options.cwd,
                None,
                Some(&path),
                Some(true),
                None,
                None,
            )
            .map_err(|e| AgentError::InvalidConfig(e.to_string()))?;
        }
        if let Some(tool_search) = self.tool_search_options {
            options.tool_search = tool_search;
        }
        if let Some(mode) = self.tool_loading_mode {
            options.tool_search.mode = mode;
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
            checkpoint_handle: self.checkpoint_handle,
            checkpoint_api: self.checkpoint_api,
            child_session_sink_factory: self.child_session_sink_factory,
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

    /// Take the event stream out of the client. The returned stream owns
    /// the runtime task and a clone of the command channel, so dropping
    /// the original `AgentClient` does NOT tear the runtime down. The
    /// runtime exits when the stream is dropped (cmd_tx hits its last
    /// reference) or when the runtime sees `Shutdown`.
    pub fn into_event_stream(mut self) -> AgentEventStream {
        let (_dummy_tx, dummy_rx) = mpsc::channel::<AgentEvent>(1);
        let rx = std::mem::replace(&mut self.event_rx, dummy_rx);
        let cmd_tx = self.cmd_tx.clone();
        // Take the handle so Drop on `AgentClient` doesn't abort the task
        // out from under the returned stream.
        let handle = self.handle.take();
        AgentEventStream {
            rx,
            _cmd_tx: cmd_tx,
            _handle: handle,
        }
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

    /// Create a manual checkpoint. Requires a `CheckpointApi` to have been
    /// attached at build time.
    pub async fn create_checkpoint(
        &mut self,
        description: impl Into<String>,
    ) -> Result<String, AgentError> {
        let api = self
            .inner
            .checkpoint_api
            .clone()
            .ok_or_else(|| AgentError::other("checkpointing not enabled"))?;
        api.create_checkpoint(Some(description.into())).await
    }

    /// List checkpoints for the active session. Returns JSON for cross-crate
    /// transport.
    pub async fn list_checkpoints(&self) -> Result<serde_json::Value, AgentError> {
        let api = self
            .inner
            .checkpoint_api
            .clone()
            .ok_or_else(|| AgentError::other("checkpointing not enabled"))?;
        api.list_checkpoints().await
    }

    /// Diff a checkpoint against the current workspace.
    pub async fn diff_checkpoint(
        &self,
        checkpoint_id: impl AsRef<str>,
    ) -> Result<serde_json::Value, AgentError> {
        let api = self
            .inner
            .checkpoint_api
            .clone()
            .ok_or_else(|| AgentError::other("checkpointing not enabled"))?;
        api.diff_checkpoint(checkpoint_id.as_ref()).await
    }

    /// Rewind workspace files to a checkpoint. Conversation history is not
    /// rewound.
    pub async fn rewind_files(
        &mut self,
        checkpoint_id: impl AsRef<str>,
    ) -> Result<serde_json::Value, AgentError> {
        let api = self
            .inner
            .checkpoint_api
            .clone()
            .ok_or_else(|| AgentError::other("checkpointing not enabled"))?;
        api.rewind(checkpoint_id.as_ref(), false, false).await
    }

    pub async fn connect_mcp_servers(&mut self) -> Result<(), AgentError> {
        let options = self.inner.options.lock().await.clone();
        let mut tools = ToolRegistry::new();
        let report = crate::mcp::register_mcp_tools(&mut tools, &options.mcp).await;
        if options.mcp.strict
            && report
                .statuses
                .iter()
                .any(|s| matches!(s.status, code_agent_mcp::McpConnectionStatus::Failed))
        {
            return Err(AgentError::InvalidConfig(
                "strict MCP mode: one or more MCP servers failed".into(),
            ));
        }
        Ok(())
    }

    pub async fn mcp_status(&self) -> Vec<code_agent_mcp::McpServerStatus> {
        let options = self.inner.options.lock().await.clone();
        connect_for_mcp_inspection(&options.mcp).await.0.statuses
    }

    pub async fn list_mcp_tools(
        &self,
        server: Option<&str>,
    ) -> Result<Vec<code_agent_mcp::McpToolSummary>, AgentError> {
        let options = self.inner.options.lock().await.clone();
        let (report, _) = connect_for_mcp_inspection(&options.mcp).await;
        let mut tools = Vec::new();
        for discovered in report.discovered {
            if server
                .map(|wanted| wanted == discovered.server_name)
                .unwrap_or(true)
            {
                tools.extend(discovered.tools);
            }
        }
        Ok(tools)
    }

    pub async fn call_mcp_tool_direct(
        &mut self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> Result<crate::ToolResult, AgentError> {
        let options = self.inner.options.lock().await.clone();
        let config = options
            .mcp
            .servers
            .get(server)
            .ok_or_else(|| AgentError::InvalidConfig(format!("unknown MCP server {server}")))?;
        let secrets = options.mcp.secret_values();
        let mut client =
            code_agent_mcp::McpClient::connect(server, config, options.mcp.limits(), secrets)
                .await
                .map_err(|e| AgentError::Other(e.to_string()))?;
        client
            .initialize()
            .await
            .map_err(|e| AgentError::Other(e.to_string()))?;
        let result = client
            .call_tool(tool, args)
            .await
            .map_err(|e| AgentError::Other(e.to_string()))?;
        let (text, metadata, _, _) = code_agent_mcp::mcp_result_to_text_and_metadata(
            &result,
            options.mcp.max_tool_result_bytes,
        )
        .map_err(|e| AgentError::Other(e.to_string()))?;
        Ok(crate::ToolResult {
            content: vec![crate::ToolContent::Text(text)],
            is_error: result.is_error,
            metadata,
        })
    }

    pub async fn tool_catalog_summary(&self) -> Result<ToolCatalogSummary, AgentError> {
        let (catalog, _, _) = self.build_tool_catalog_snapshot().await?;
        Ok(catalog.summary())
    }

    pub async fn search_tools(
        &mut self,
        query: ToolSearchQuery,
    ) -> Result<ToolSearchResult, AgentError> {
        let (catalog, mut state, options) = self.build_tool_catalog_snapshot().await?;
        let visibility = {
            let opts = self.inner.options.lock().await;
            options.visibility_context(
                opts.permission_mode,
                opts.allowed_tool_rules.clone(),
                opts.disallowed_tool_rules.clone(),
                opts.allowed_tools.clone(),
                opts.mcp.external_effect_policy,
            )
        };
        let (result, loaded) = catalog
            .search_and_mark_loaded(query, options.strategy, &visibility, &mut state, &options)
            .map_err(|e| AgentError::Other(e.to_string()))?;
        if !loaded.is_empty() {
            let mut options_guard = self.inner.options.lock().await;
            for name in loaded {
                if !options_guard
                    .tool_search
                    .preloaded_discovered_tools
                    .contains(&name)
                {
                    options_guard
                        .tool_search
                        .preloaded_discovered_tools
                        .push(name);
                }
            }
        }
        Ok(result)
    }

    pub async fn list_tools(
        &self,
        filter: ToolListFilter,
    ) -> Result<Vec<ToolDescriptorPreview>, AgentError> {
        let (catalog, state, options) = self.build_tool_catalog_snapshot().await?;
        Ok(catalog.list_previews(filter, &state, &options))
    }

    pub async fn show_tool(&self, tool_name: &str) -> Result<ToolDescriptor, AgentError> {
        let (catalog, _, _) = self.build_tool_catalog_snapshot().await?;
        catalog
            .get(tool_name)
            .cloned()
            .ok_or_else(|| AgentError::Other(format!("unknown tool {tool_name}")))
    }

    pub async fn tool_context_budget(&self) -> Result<ToolContextBudget, AgentError> {
        let (catalog, state, options) = self.build_tool_catalog_snapshot().await?;
        Ok(catalog.context_budget(&state, &options))
    }

    /// Shut down the runtime. Pending queued messages are dropped.
    pub async fn shutdown(mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(ClientCommand::Shutdown).await;
        if let Some(h) = self.handle.take() {
            let _ = h.await;
        }
        Ok(())
    }

    async fn build_tool_catalog_snapshot(
        &self,
    ) -> Result<(ToolCatalog, LoadedToolState, ToolSearchOptions), AgentError> {
        let options = self.inner.options.lock().await.clone();
        let mut registry = self.inner.tools.clone();
        if registry.get(SEARCH_TOOLS_NAME).is_none() {
            registry.register(Arc::new(SearchToolsDefinitionTool::new(
                options.tool_search.max_results,
            )));
        }
        let report = crate::mcp::register_mcp_tools(&mut registry, &options.mcp).await;
        if options.mcp.strict
            && report
                .statuses
                .iter()
                .any(|s| matches!(s.status, code_agent_mcp::McpConnectionStatus::Failed))
        {
            return Err(AgentError::InvalidConfig(
                "strict MCP mode: one or more MCP servers failed".into(),
            ));
        }
        let allowed = if options.allowed_tools.is_empty() {
            registry
        } else {
            registry.filter(&options.allowed_tools)
        };
        let mut builder =
            ToolCatalogBuilder::new(options.tool_search.clone()).ingest_registry(&allowed);
        for status in &report.statuses {
            builder = builder.mcp_transport(status.name.clone(), status.transport);
        }
        let catalog = builder.build();
        let state = catalog.initial_loaded_state(&options.tool_search);
        Ok((catalog, state, options.tool_search))
    }
}

impl Drop for AgentClient {
    fn drop(&mut self) {
        // Best-effort: signal the runtime to exit. Do NOT abort the
        // join handle — the runtime task may still be flushing events
        // to a stream consumer (e.g. via `into_event_stream()` which
        // takes ownership of the handle). When the last clone of
        // `cmd_tx` drops, the runtime will see the channel close and
        // exit cleanly.
        let _ = self.cmd_tx.try_send(ClientCommand::Shutdown);
    }
}

/// Stream of `AgentEvent` returned by [`AgentClient::into_event_stream`].
/// Owns the runtime task and a command-channel clone so the runtime
/// stays alive as long as the stream does.
pub struct AgentEventStream {
    rx: mpsc::Receiver<AgentEvent>,
    _cmd_tx: mpsc::Sender<ClientCommand>,
    _handle: Option<tokio::task::JoinHandle<()>>,
}

impl Stream for AgentEventStream {
    type Item = AgentEvent;
    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.rx).poll_recv(cx)
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
    checkpoint_handle: Option<Arc<dyn CheckpointHandle>>,
    checkpoint_api: Option<Arc<dyn CheckpointApi>>,
    child_session_sink_factory: Option<Arc<dyn crate::agents::ChildSessionSinkFactory>>,
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

        let mut builder = AgentBuilder::new()
            .options(options_snapshot.clone())
            .provider(self.inner.provider.clone())
            .tools(self.inner.tools.clone())
            .hooks(self.inner.hooks.clone())
            .approval_memory(self.inner.approval_memory.clone())
            .initial_history(initial_history.clone())
            .session_sink_opt(self.inner.session_sink.clone())
            .telemetry_sink_opt(self.inner.telemetry_sink.clone())
            .pricing_opt(self.inner.pricing);
        if let Some(h) = self.inner.checkpoint_handle.clone() {
            builder = builder.checkpoint_handle(h);
        }
        if let Some(f) = self.inner.child_session_sink_factory.clone() {
            builder = builder.child_session_sink_factory(f);
        }
        let agent_result = builder.build();
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
            if let AgentEvent::ToolLoaded(loaded) = &ev {
                let mut options = self.inner.options.lock().await;
                if options.tool_search.persist_discovered_tools
                    && !options
                        .tool_search
                        .preloaded_discovered_tools
                        .contains(&loaded.tool_name)
                {
                    options
                        .tool_search
                        .preloaded_discovered_tools
                        .push(loaded.tool_name.clone());
                }
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

async fn connect_for_mcp_inspection(
    options: &code_agent_mcp::McpOptions,
) -> (crate::mcp::McpRegistrationReport, ToolRegistry) {
    let mut tools = ToolRegistry::new();
    let report = crate::mcp::register_mcp_tools(&mut tools, options).await;
    (report, tools)
}

// The private re-export above makes `Agent` reachable from this module.
#[allow(dead_code)]
fn _agent_used(_a: Agent) {}
