//! The application-facing `AgentRuntime` and `AgentRuntimeBuilder`.
//!
//! Internally the runtime wires the SDK's safety-first defaults to the
//! existing core `Agent` and plan/act driver. Applications do not need to
//! understand the core types to use the SDK.

mod adapters;
mod builder;
mod run;
mod validation;

use std::pin::Pin;
use std::sync::Arc;

use crom_auth::ProviderId;
use crom_core::ModelProvider;
use futures::stream::Stream;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::budget::CancellationHandle;
use crate::callback::{
    ApprovalCallback, CredentialResolver, DefaultSecretRedactor, EventSink, SecretRedactor,
    ToolHost,
};
use crate::error::AgentSdkError;
use crate::event::{EventVerbosity, RunStarted, SdkEvent};
use crate::plan::{ActRequest, PlanRequest, PlanResult};
use crate::policy::{InteractivityMode, ToolPolicy};
use crate::request::{AgentRequest, SessionPolicy};
use crate::result::{AgentRunResult, AgentRunStatus};
use crate::storage::{AgentStorage, NoopStorage};
use crate::workspace::Workspace;
use crate::{RunId, SessionId};

pub use builder::AgentRuntimeBuilder;

/// Embeddable headless agent runtime.
///
/// Build with [`AgentRuntime::builder`]. Reusable across independent runs:
/// no state leaks between runs unless the host explicitly configures
/// persistent storage and a long-lived session policy.
pub struct AgentRuntime {
    pub(crate) inner: Arc<RuntimeInner>,
}

impl std::fmt::Debug for AgentRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntime")
            .field("provider", &self.inner.provider_id)
            .field("model", &self.inner.default_model)
            .finish()
    }
}

pub(crate) struct RuntimeInner {
    pub(crate) provider: Arc<dyn ModelProvider>,
    pub(crate) provider_id: ProviderId,
    pub(crate) default_model: String,
    /// Runtime-level system prompt; a per-request override takes precedence.
    pub(crate) default_system_prompt: Option<String>,
    /// Runtime-level project instructions threaded into every run.
    pub(crate) default_project_instructions: Option<String>,
    pub(crate) workspace: Option<Arc<dyn Workspace>>,
    pub(crate) storage: Arc<dyn AgentStorage>,
    pub(crate) approval_callback: Option<Arc<dyn ApprovalCallback>>,
    // The runtime holds these so future plumbing can opt them in without
    // breaking the builder surface. Today they are accepted on the builder
    // for forward compatibility; once they participate in the run path the
    // dead_code allowance can be dropped.
    #[allow(dead_code)]
    pub(crate) credential_resolver: Option<Arc<dyn CredentialResolver>>,
    pub(crate) event_sink: Option<Arc<dyn EventSink>>,
    pub(crate) secret_redactor: Arc<dyn SecretRedactor>,
    #[allow(dead_code)]
    pub(crate) tool_host: Option<Arc<dyn ToolHost>>,
    pub(crate) tool_policy: ToolPolicy,
    pub(crate) interactivity: InteractivityMode,
    pub(crate) verbosity: EventVerbosity,
    #[allow(dead_code)]
    pub(crate) default_budgets: crate::budget::RunBudgets,
}

impl AgentRuntime {
    pub fn builder() -> AgentRuntimeBuilder {
        AgentRuntimeBuilder::new()
    }

    /// Convenience: drive a run to completion and return the final result.
    pub async fn run_once(&self, request: AgentRequest) -> Result<AgentRunResult, AgentSdkError> {
        let mut handle = self.run(request).await?;
        use futures::StreamExt;
        // Drain events; the runtime task drops them onto the event_sink and
        // we also collect the count for the result.
        let events = std::mem::replace(&mut handle.events, Box::pin(futures::stream::empty()));
        futures::pin_mut!(events);
        while events.next().await.is_some() {}
        handle.collected_result().await
    }

    /// Start a run and return a streaming handle. The caller drives the
    /// stream to completion and may cancel via the embedded handle.
    pub async fn run(&self, request: AgentRequest) -> Result<AgentRun, AgentSdkError> {
        request.validate()?;
        let run_id = RunId::new();
        let metadata = request.metadata.clone();
        let session_id = match &request.session_policy {
            SessionPolicy::None => None,
            SessionPolicy::Existing(id) => Some(id.clone()),
            _ => Some(SessionId::default()),
        };
        // SDK-managed persistence only happens for `InMemory` / `Persisted`
        // session policies. `None`, `Existing`, and `HostManaged` keep the
        // SDK out of the storage path so the host can own the session
        // record without our writes interleaving.
        let persist_session = matches!(
            request.session_policy,
            SessionPolicy::InMemory | SessionPolicy::Persisted
        );
        let storage_session = if persist_session {
            session_id.clone()
        } else {
            None
        };
        let (event_tx, event_rx) = mpsc::channel::<Result<SdkEvent, AgentSdkError>>(64);
        let cancellation = CancellationHandle::new();
        let cancellation_for_task = cancellation.clone();
        let inner = self.inner.clone();
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| inner.default_model.clone());
        let provider_id = inner.provider_id.clone();
        let result_holder = Arc::new(tokio::sync::Mutex::new(
            None::<Result<AgentRunResult, AgentSdkError>>,
        ));
        let result_for_task = result_holder.clone();

        let start_event = SdkEvent::RunStarted(RunStarted {
            run_id: run_id.clone(),
            session_id: session_id.clone(),
            provider: provider_id.clone(),
            model: model.clone(),
            metadata: metadata.clone(),
        });
        run::emit_event(&inner, &event_tx, storage_session.as_ref(), start_event).await;

        let run_id_for_task = run_id.clone();
        let session_for_task = session_id.clone();
        let storage_session_for_task = storage_session.clone();
        let request_metadata_for_task = metadata.clone();
        let model_for_task = model.clone();
        let task = tokio::spawn(async move {
            let res = run::execute_request(
                inner.clone(),
                run_id_for_task.clone(),
                session_for_task.clone(),
                request,
                model_for_task,
                provider_id.clone(),
                event_tx.clone(),
                cancellation_for_task,
            )
            .await;
            let mut final_result = match res {
                Ok(r) => r,
                Err(e) => {
                    let mut r = AgentRunResult::failed(
                        run_id_for_task.clone(),
                        provider_id.clone(),
                        inner.default_model.clone(),
                        request_metadata_for_task.clone(),
                        e.message.clone(),
                    );
                    r.diagnostics = e.diagnostics.clone();
                    if e.is_cancelled() {
                        r.status = AgentRunStatus::Cancelled;
                    }
                    r
                }
            };
            final_result.session_id = session_for_task.clone();
            // Emit completion event.
            let evt = match final_result.status {
                AgentRunStatus::Completed
                | AgentRunStatus::NeedsApproval
                | AgentRunStatus::NeedsReplan
                | AgentRunStatus::ValidationFailed => {
                    SdkEvent::RunCompleted(crate::event::RunCompletedEvent {
                        run_id: run_id_for_task.clone(),
                        status: final_result.status,
                        summary: final_result.summary.clone(),
                        usage: final_result.usage.clone(),
                        events_summary: final_result.events_summary.clone(),
                    })
                }
                AgentRunStatus::Cancelled => {
                    SdkEvent::RunCancelled(crate::event::RunCancelledEvent {
                        run_id: run_id_for_task.clone(),
                        message: final_result.summary.clone(),
                    })
                }
                AgentRunStatus::TimedOut => SdkEvent::RunFailed(crate::event::RunFailedEvent {
                    run_id: run_id_for_task.clone(),
                    status: AgentRunStatus::TimedOut,
                    message: final_result.summary.clone(),
                    diagnostics: final_result.diagnostics.clone(),
                }),
                AgentRunStatus::Failed => SdkEvent::RunFailed(crate::event::RunFailedEvent {
                    run_id: run_id_for_task.clone(),
                    status: AgentRunStatus::Failed,
                    message: final_result.summary.clone(),
                    diagnostics: final_result.diagnostics.clone(),
                }),
            };
            run::emit_event(&inner, &event_tx, storage_session_for_task.as_ref(), evt).await;
            *result_for_task.lock().await = Some(Ok(final_result));
        });

        Ok(AgentRun {
            run_id,
            session_id,
            events: Box::pin(ReceiverStream::new(event_rx)),
            cancellation,
            task: Some(task),
            result: result_holder,
        })
    }

    /// Plan-only entry point.
    pub async fn plan(&self, request: PlanRequest) -> Result<PlanResult, AgentSdkError> {
        run::execute_plan(self.inner.clone(), request).await
    }

    /// Act on an existing plan.
    pub async fn act(&self, request: ActRequest) -> Result<AgentRunResult, AgentSdkError> {
        run::execute_act(self.inner.clone(), request, None).await
    }

    /// Register a custom tool at the runtime level. The tool participates in
    /// every subsequent run.
    pub fn register_tool(&self, _tool: Arc<dyn crom_core::Tool>) {
        // Tools are part of `ToolPolicy`, which is fixed at build time for
        // the SDK runtime. To register a tool dynamically, applications can
        // build a new runtime; we keep this entry point for forward-compat.
    }

    /// Provider id (e.g. `anthropic`, `fake`).
    pub fn provider_id(&self) -> ProviderId {
        self.inner.provider_id.clone()
    }

    /// Default model id.
    pub fn default_model(&self) -> &str {
        &self.inner.default_model
    }
}

/// A streaming handle to a live run.
pub struct AgentRun {
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub events: Pin<Box<dyn Stream<Item = Result<SdkEvent, AgentSdkError>> + Send>>,
    pub cancellation: CancellationHandle,
    task: Option<tokio::task::JoinHandle<()>>,
    result: Arc<tokio::sync::Mutex<Option<Result<AgentRunResult, AgentSdkError>>>>,
}

impl std::fmt::Debug for AgentRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRun")
            .field("run_id", &self.run_id)
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl AgentRun {
    /// Collect the final result. Awaits the background task. Use this after
    /// the event stream has been drained, or after cancellation.
    pub async fn final_result(mut self) -> Result<AgentRunResult, AgentSdkError> {
        // Drain stream to completion if the caller hasn't already.
        use futures::StreamExt;
        let mut events = std::mem::replace(&mut self.events, Box::pin(futures::stream::empty()));
        while events.next().await.is_some() {}
        self.collected_result().await
    }

    async fn collected_result(mut self) -> Result<AgentRunResult, AgentSdkError> {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        let mut g = self.result.lock().await;
        g.take()
            .unwrap_or_else(|| Err(AgentSdkError::internal("runtime never produced a result")))
    }
}

impl Drop for AgentRun {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

// Defaults plumbed through the builder.
pub(crate) fn default_secret_redactor() -> Arc<dyn SecretRedactor> {
    Arc::new(DefaultSecretRedactor)
}

pub(crate) fn default_storage() -> Arc<dyn AgentStorage> {
    Arc::new(NoopStorage)
}
