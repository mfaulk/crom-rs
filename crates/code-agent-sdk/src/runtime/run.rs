//! Execution of `AgentRequest` against the underlying core agent loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use code_agent_core::events::{AgentEvent, ResultSubtype};
use code_agent_core::permission::PermissionMode;
use code_agent_core::plan::{
    ActOutcome, PlanCreatedBy, PlanDocument, PlanError, PlanStatus, PlanningOptions,
};
use code_agent_core::{AgentBuilder, AgentOptions, ToolRegistry};
use futures::stream::StreamExt;
use tokio::sync::mpsc;

use crate::budget::CancellationHandle;
use crate::error::{AgentSdkError, AgentSdkErrorKind, Diagnostic};
use crate::event::{redact_event, DiagnosticEvent, PatchProposedEvent, SdkEvent, SdkEventMapper};
use crate::patch::{CodeArtifacts, FilePatch, PatchSet};
use crate::plan::{ActRequest, PlanRequest, PlanResult};
use crate::request::{AgentMode, AgentRequest, PatchPolicy, PlanPolicy};
use crate::result::{AgentRunResult, AgentRunStatus, EventSummary, ValidationReport};
use crate::runtime::adapters::SdkPermissionCallback;
use crate::runtime::validation;
use crate::runtime::RuntimeInner;
use crate::workspace::{
    DiffBase, InMemoryWorkspace, PatchOnlyWorkspace, ReadOnlyWorkspace, Workspace, WorkspacePath,
    WorkspaceSource,
};
use crate::{RunId, SessionId};

/// RAII guard that aborts a spawned task when the guard drops. Used to
/// pin the deadline watcher to the lifetime of `execute_request` so it
/// cannot outlive a normal-completion return path. (#164)
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Redact, then ship a single `SdkEvent` to the three sinks: the
/// caller's event channel, the optional runtime event sink, and (when a
/// storage session is active) the storage log.
///
/// - Secret redaction is applied once here so every persisted/emitted
///   copy is scrubbed by the configured `SecretRedactor` (#211). The
///   value handed to tool execution is never routed through this path,
///   so tool dispatch still sees the real input.
/// - Storage-append and event-send failures are logged rather than
///   silently discarded (#151). They are intentionally non-fatal: a
///   closed consumer channel or a transient storage error must not abort
///   an otherwise-healthy run.
pub(crate) async fn emit_event(
    runtime: &RuntimeInner,
    event_tx: &mpsc::Sender<Result<SdkEvent, AgentSdkError>>,
    storage_session: Option<&SessionId>,
    event: SdkEvent,
) {
    let event = redact_event(&*runtime.secret_redactor, event);
    if let Err(e) = event_tx.send(Ok(event.clone())).await {
        tracing::debug!(error = %e, "sdk event channel closed; dropping event");
    }
    if let Some(sink) = &runtime.event_sink {
        sink.emit(event.clone()).await;
    }
    if let Some(s) = storage_session {
        if let Err(e) = runtime.storage.append_event(s, event).await {
            tracing::warn!(error = %e, "failed to append event to storage");
        }
    }
}

/// Execute an AgentRequest. Owns the actual agent loop and returns a result
/// summary; the streaming wrapper in `mod.rs` is responsible for shipping
/// events to the host.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_request(
    runtime: Arc<RuntimeInner>,
    run_id: RunId,
    session_id: Option<SessionId>,
    request: AgentRequest,
    model: String,
    provider_id: code_agent_auth::ProviderId,
    event_tx: mpsc::Sender<Result<SdkEvent, AgentSdkError>>,
    cancellation: CancellationHandle,
) -> Result<AgentRunResult, AgentSdkError> {
    let metadata = request.metadata.clone();
    let started = Instant::now();
    let mapper = SdkEventMapper::new(run_id.clone(), runtime.verbosity);

    // SDK-managed persistence only happens for InMemory/Persisted session
    // policies. The session id may still be set (e.g. HostManaged), but the
    // storage append is suppressed so the host can own the record.
    let storage_session = if matches!(
        request.session_policy,
        crate::request::SessionPolicy::InMemory | crate::request::SessionPolicy::Persisted
    ) {
        session_id.clone()
    } else {
        None
    };

    // Resolve workspace.
    let workspace = resolve_workspace(&runtime, request.workspace).await?;

    // Apply patch policy by wrapping the workspace. For filesystem workspaces
    // the tools write to disk, so PatchOnly only redirects writes that go
    // through the SDK Workspace trait; the rollback step covers tool writes.
    let workspace_for_run: Arc<dyn Workspace> = match request.patch_policy {
        PatchPolicy::ReturnOnly if workspace.filesystem_root().is_none() => {
            Arc::new(PatchOnlyWorkspace::new(workspace.clone()))
        }
        PatchPolicy::ReturnOnly => workspace.clone(),
        PatchPolicy::RejectMutations => Arc::new(ReadOnlyWorkspace::new(workspace.clone())),
        _ => workspace.clone(),
    };

    // Apply deadline.
    let deadline = request.deadline.or(request.budgets.max_duration);
    // Flag set by the deadline watcher before it cancels, so the
    // finalization logic can distinguish a deadline-driven cancellation
    // (which must surface as `TimedOut`) from an external cancel (which
    // stays `Cancelled`). Without it, a deadline expiry cancelled the run
    // but the status was only ever `Cancelled`; `TimedOut` was unreachable
    // on this path (#212).
    let deadline_fired = Arc::new(AtomicBool::new(false));
    // Hold the watcher in a drop guard so a normal early return (Plan-only
    // short-circuit, validation failure, etc.) tears the task down instead
    // of letting it sleep for the rest of the deadline window. Previously
    // the JoinHandle was dropped immediately, so the watcher relied on
    // `cancellation.cancel()` being called somewhere to exit — which the
    // normal-completion path does not do. (#164)
    let _watcher_guard = deadline.map(|d| {
        let c = cancellation.clone();
        let fired = deadline_fired.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(d) => {
                    fired.store(true, Ordering::SeqCst);
                    c.cancel();
                }
                _ = c.cancelled() => {}
            }
        });
        AbortOnDrop(handle.abort_handle())
    });

    // Plan-only short circuit.
    if matches!(request.mode, AgentMode::PlanOnly)
        || matches!(request.plan_policy, PlanPolicy::Required)
    {
        let plan_req = PlanRequest {
            task: request.task.clone(),
            workspace: Some(WorkspaceSource::custom(workspace_for_run.clone())),
            planning_mode: code_agent_core::PlanningMode::default(),
            max_steps: 16,
            provider: request.provider.clone(),
            model: Some(model.clone()),
            metadata: metadata.clone(),
            budgets: request.budgets.clone(),
        };
        // Race plan generation against cancellation so cancel/deadline
        // can stop a stuck plan-only run (#145 / #155 follow-up). The
        // underlying core plan driver does not yet observe cancel
        // internally, so cancel here just releases the SDK await; the
        // background task may continue briefly until it finishes
        // naturally.
        let plan_result = tokio::select! {
            r = execute_plan(runtime.clone(), plan_req) => r?,
            _ = cancellation.cancelled() => {
                let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
                // A deadline-driven cancel must surface as TimedOut, not
                // Cancelled, on the plan-only path too — mirroring the regular
                // run finalization (#212 / #40).
                let status = if deadline_fired.load(Ordering::SeqCst) {
                    AgentRunStatus::TimedOut
                } else {
                    AgentRunStatus::Cancelled
                };
                return Ok(AgentRunResult {
                    run_id,
                    session_id,
                    status,
                    summary: String::new(),
                    artifacts: CodeArtifacts::empty(),
                    patch: None,
                    diagnostics: vec![Diagnostic::warning(
                        "run.cancelled",
                        "plan generation cancelled",
                    )],
                    validations: Vec::new(),
                    plan: None,
                    usage: None,
                    provider: provider_id,
                    model,
                    events_summary: EventSummary {
                        total_events: 0,
                        tool_calls: 0,
                        approvals_requested: 0,
                        files_changed: 0,
                        diagnostics_count: 1,
                        duration_ms,
                    },
                    metadata,
                    changed_files: Vec::new(),
                });
            }
        };
        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        return Ok(AgentRunResult {
            run_id,
            session_id,
            status: AgentRunStatus::Completed,
            summary: plan_result.plan.summary.clone(),
            artifacts: CodeArtifacts::empty(),
            patch: None,
            diagnostics: plan_result.diagnostics.clone(),
            validations: Vec::new(),
            plan: Some(plan_result.plan),
            usage: plan_result.usage,
            provider: provider_id,
            model,
            events_summary: EventSummary {
                total_events: plan_result.events_summary.total_events,
                tool_calls: plan_result.events_summary.tool_calls,
                approvals_requested: 0,
                files_changed: 0,
                diagnostics_count: plan_result.diagnostics.len(),
                duration_ms,
            },
            metadata,
            changed_files: Vec::new(),
        });
    }

    // For PlanThenAct, first generate a plan, then act on it.
    if matches!(request.mode, AgentMode::PlanThenAct)
        || matches!(request.plan_policy, PlanPolicy::PlanThenAct)
    {
        let plan_req = PlanRequest {
            task: request.task.clone(),
            workspace: Some(WorkspaceSource::custom(workspace_for_run.clone())),
            planning_mode: code_agent_core::PlanningMode::default(),
            max_steps: 16,
            provider: request.provider.clone(),
            model: Some(model.clone()),
            metadata: metadata.clone(),
            budgets: request.budgets.clone(),
        };
        // Race plan-then-act against cancellation at the SDK boundary
        // so cancel/deadline can stop the run (#145 / #155 follow-up).
        let plan_result = tokio::select! {
            r = execute_plan(runtime.clone(), plan_req) => r?,
            _ = cancellation.cancelled() => return Ok(cancelled_result(
                run_id.clone(),
                session_id,
                provider_id,
                model,
                metadata,
                started,
                "plan generation cancelled",
                deadline_fired.load(Ordering::SeqCst),
            )),
        };
        // Emit PlanCreated once the plan is produced so consumers (e.g. the
        // eval harness) can observe planning natively from the event stream
        // rather than reconstructing it from the result (#34).
        let plan_created = SdkEvent::PlanCreated(crate::event::PlanCreated {
            run_id: run_id.clone(),
            plan: plan_result.plan.clone(),
        });
        emit_event(&runtime, &event_tx, storage_session.as_ref(), plan_created).await;
        let act_req = ActRequest {
            plan: plan_result.plan.clone(),
            workspace: Some(WorkspaceSource::custom(workspace_for_run.clone())),
            patch_policy: request.patch_policy,
            approval_policy: request.approval_policy,
            validation: request.validation.clone(),
            provider: request.provider.clone(),
            model: Some(model.clone()),
            metadata: metadata.clone(),
            budgets: request.budgets.clone(),
            test_force_approval: true,
            deadline,
        };
        let act_emit = ActEmitContext {
            run_id: run_id.clone(),
            event_tx: &event_tx,
            storage_session: storage_session.clone(),
            mapper: &mapper,
        };
        return tokio::select! {
            r = execute_act(runtime.clone(), act_req, Some(act_emit)) => r,
            _ = cancellation.cancelled() => Ok(cancelled_result(
                run_id,
                None,
                provider_id,
                model,
                metadata,
                started,
                "plan act cancelled",
                deadline_fired.load(Ordering::SeqCst),
            )),
        };
    }

    // Snapshot the workspace state before the regular run for any patch
    // policy that needs to return a meaningful diff. Built-in tools
    // write directly to disk; without a baseline the post-run diff
    // would treat every file in a filesystem workspace as newly
    // created. We snapshot for ReturnOnly (where we also use the
    // snapshot to roll back disk mutations) and for ApplyAndReturn
    // (where mutations stay on disk but the diff still needs a
    // baseline). Hoisted below the PlanOnly / PlanThenAct
    // short-circuits so plan-only modes do not pay (or fail on) a
    // snapshot they never use (#144 codex review).
    //
    // Any I/O error during snapshot is fatal — proceeding with a
    // partial snapshot would let the post-run rollback delete every
    // file that snapshot missed, which was the silent-data-loss
    // regression in #144.
    let baseline_snapshot: Option<std::collections::BTreeMap<WorkspacePath, Vec<u8>>> = if matches!(
        request.patch_policy,
        PatchPolicy::ReturnOnly | PatchPolicy::ApplyAndReturn
    )
        && workspace.filesystem_root().is_some()
    {
        Some(snapshot_workspace(&*workspace).await?)
    } else {
        None
    };

    // Regular run.
    let tools = build_tool_registry(&runtime.tool_policy, workspace_for_run.clone());
    let mut options = AgentOptions {
        model: model.clone(),
        session_id: session_id.clone(),
        max_turns: request.max_turns.or(request.budgets.max_turns),
        permission_mode: PermissionMode::Default,
        allowed_tools: tools.names(),
        ..Default::default()
    };
    if let Some(root) = workspace_for_run.filesystem_root() {
        options.cwd = root;
    }
    // System prompt: per-request override wins over the runtime-level
    // default; project instructions come from the runtime.
    options.system_prompt = request
        .system_prompt
        .clone()
        .or_else(|| runtime.default_system_prompt.clone());
    options.project_instructions = runtime.default_project_instructions.clone();
    let interactivity = request.interactivity.unwrap_or(runtime.interactivity);
    let approval_cb: Arc<dyn code_agent_core::PermissionCallback> =
        Arc::new(SdkPermissionCallback {
            inner: runtime.approval_callback.clone(),
            policy: request.approval_policy,
            interactivity,
            run_id: run_id.clone(),
            session_id: session_id.clone(),
            metadata: metadata.clone(),
        });
    options.permission_callback = Some(approval_cb);

    let provider = runtime.provider.clone();
    // Thread the SDK-level cancellation pair (#145, #155) into the core
    // agent loop so external cancel, deadline expiry, and budget
    // breaches actually stop the run instead of being advisory.
    let agent = AgentBuilder::new()
        .options(options.clone())
        .provider(provider)
        .tools(tools.clone())
        .cancellation(cancellation.flag(), cancellation.notify())
        .build()
        .map_err(AgentSdkError::from)?;

    let prompt = request.task.clone();
    let mut event_stream = agent.run(prompt);

    // Collect events.
    let mut events_total = 0;
    let mut tool_calls = 0;
    let mut approvals_requested = 0;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    let mut usage: Option<code_agent_core::TokenUsage> = None;
    let mut final_summary = String::new();
    let mut status = AgentRunStatus::Completed;
    // One-shot guard for the `max_tool_calls` budget. Without this,
    // every event after the cap was crossed re-pushed an identical
    // `budget.max_tool_calls_exceeded` diagnostic and re-fired
    // `cancellation.cancel()`. Since cancellation does not stop the
    // already-streaming agent loop instantly, dozens of duplicate
    // diagnostics could accumulate before the run wound down (#149).
    let mut max_tool_calls_reported = false;

    loop {
        // Race the next agent event against cancellation so the SDK
        // also releases the stream on cancel — without this the
        // consumer keeps blocking on the agent task until it finishes
        // naturally (#145, #155).
        let ev = tokio::select! {
            ev = event_stream.next() => match ev {
                Some(ev) => ev,
                None => break,
            },
            _ = cancellation.cancelled() => {
                status = AgentRunStatus::Cancelled;
                break;
            }
        };
        events_total += 1;
        // Track key counts.
        match &ev {
            AgentEvent::ToolCallStarted(_) => tool_calls += 1,
            AgentEvent::ApprovalPromptShown(_) => approvals_requested += 1,
            AgentEvent::ToolCallDenied(d) => {
                diagnostics.push(
                    Diagnostic::warning("tool.permission_denied", d.reason.clone()).with_location(
                        crate::error::DiagnosticLocation::Tool {
                            name: d.tool_name.clone(),
                        },
                    ),
                );
            }
            AgentEvent::AssistantMessage(am) => {
                for c in &am.content {
                    if let code_agent_core::conversation::AssistantContent::Text { text } = c {
                        final_summary.push_str(text);
                    }
                }
            }
            AgentEvent::Result(r) => {
                usage = r.usage.clone();
                match r.subtype {
                    ResultSubtype::Success => status = AgentRunStatus::Completed,
                    ResultSubtype::ErrorMaxTurns => {
                        status = AgentRunStatus::Failed;
                        diagnostics.push(Diagnostic::error(
                            "budget.max_turns_exceeded",
                            r.result.clone().unwrap_or_default(),
                        ));
                    }
                    ResultSubtype::ErrorDuringExecution => {
                        status = AgentRunStatus::Failed;
                        if let Some(msg) = &r.result {
                            diagnostics.push(Diagnostic::error("run.execution_error", msg.clone()));
                        }
                    }
                    ResultSubtype::Cancelled => status = AgentRunStatus::Cancelled,
                }
            }
            _ => {}
        }
        // Map to SDK event for downstream consumers.
        if let Some(sdk_ev) = mapper.map(&ev) {
            emit_event(&runtime, &event_tx, storage_session.as_ref(), sdk_ev).await;
        }

        // Budget enforcement. Guard against re-reporting: every event
        // that arrives after the cap is crossed used to re-push an
        // identical diagnostic and re-fire `cancel()`, producing a
        // diagnostics list that grew with every additional event (#149).
        //
        // Trip only once the count *exceeds* the cap (`> max`), so a run
        // that uses exactly `max` tool calls still finishes and returns a
        // result rather than being marked Failed at its allowance. This is a
        // reactive cancellation: `tool_calls` is incremented on
        // `ToolCallStarted`, so the (max+1)-th call has already started by the
        // time this fires (cooperative cancellation cannot un-start it).
        // Strictly preventing the (max+1)-th call from dispatching at all
        // would require a pre-dispatch budget gate in the core agent loop
        // (code-agent-core), which is out of scope for the SDK runtime — see
        // #213. (An earlier `>=` attempt failed runs at their exact allowance,
        // e.g. max_tool_calls=1 on the first call, which is worse.)
        if let Some(max) = request.budgets.max_tool_calls {
            if !max_tool_calls_reported && tool_calls > max as usize {
                cancellation.cancel();
                diagnostics.push(Diagnostic::error(
                    "budget.max_tool_calls_exceeded",
                    "max_tool_calls budget exceeded",
                ));
                status = AgentRunStatus::Failed;
                max_tool_calls_reported = true;
            }
        }
    }
    drop(event_stream);

    // Compute patch from workspace if appropriate.
    let mut patch: Option<PatchSet> = None;
    let mut changed_files: Vec<WorkspacePath> = Vec::new();
    if matches!(
        request.patch_policy,
        PatchPolicy::ReturnOnly | PatchPolicy::ApplyAndReturn
    ) {
        let diff_base = if let Some(snap) = &baseline_snapshot {
            DiffBase::Snapshot(snap.clone())
        } else {
            DiffBase::Initial
        };
        match workspace_for_run.diff(diff_base).await {
            Ok(p) => {
                for fp in &p.files {
                    match fp {
                        FilePatch::Create { path, .. }
                        | FilePatch::Modify { path, .. }
                        | FilePatch::Delete { path } => changed_files.push(path.clone()),
                        FilePatch::Rename { to, .. } => changed_files.push(to.clone()),
                    }
                }
                // Enforce max_patch_bytes/max_files_changed budgets.
                if let Some(limit) = request.budgets.max_patch_bytes {
                    if p.unified_diff.len() > limit {
                        diagnostics.push(Diagnostic::error(
                            "patch.too_large",
                            format!(
                                "patch size {} exceeds max_patch_bytes={}",
                                p.unified_diff.len(),
                                limit
                            ),
                        ));
                        status = AgentRunStatus::Failed;
                    }
                }
                if let Some(limit) = request.budgets.max_files_changed {
                    if changed_files.len() > limit {
                        diagnostics.push(Diagnostic::error(
                            "budget.max_files_changed_exceeded",
                            format!(
                                "{} files changed exceeds max_files_changed={}",
                                changed_files.len(),
                                limit
                            ),
                        ));
                        status = AgentRunStatus::Failed;
                    }
                }
                if !p.files.is_empty() {
                    let sdk_ev = SdkEvent::PatchProposed(PatchProposedEvent {
                        run_id: run_id.clone(),
                        patch: p.clone(),
                    });
                    emit_event(&runtime, &event_tx, storage_session.as_ref(), sdk_ev).await;
                }
                patch = Some(p);
            }
            Err(e) => {
                // Diff failure must NOT silently drop the rollback —
                // disk mutations would stay on disk while the run
                // still reported success. Record the diff failure as
                // a diagnostic and fall through; the rollback block
                // below still runs.
                diagnostics.push(Diagnostic::error(
                    "patch.diff_failed",
                    format!("workspace.diff failed: {e}"),
                ));
                status = AgentRunStatus::Failed;
            }
        }
        // ReturnOnly + filesystem: roll back any disk mutations to the
        // baseline snapshot we took before the agent ran. Hoisted out
        // of the diff `Ok(_)` arm so a diff failure cannot silently
        // skip rollback (#144 codex review).
        if matches!(request.patch_policy, PatchPolicy::ReturnOnly) {
            if let Some(snap) = &baseline_snapshot {
                if let Err(e) = restore_workspace(&*workspace, snap).await {
                    diagnostics.push(Diagnostic::error(
                        "workspace.rollback_failed",
                        e.message.clone(),
                    ));
                    status = AgentRunStatus::Failed;
                }
            }
        }
    }

    // Validation step.
    let mut validations: Vec<ValidationReport> = Vec::new();
    if request.validation.run_after_completion && !request.validation.commands.is_empty() {
        let policy = &runtime.tool_policy.side_effect_policy;
        if policy.allow_command_execution {
            // Use the underlying filesystem root if available; otherwise refuse.
            if let Some(root) = workspace_for_run.filesystem_root() {
                for cmd in &request.validation.commands {
                    let report = validation::run_validation_command(
                        &root,
                        cmd,
                        &request.validation,
                        cancellation.clone(),
                    )
                    .await;
                    if !report.success && request.validation.require_success {
                        diagnostics.push(Diagnostic::error(
                            "validation.command_failed",
                            format!("validation command failed: {cmd}"),
                        ));
                        status = AgentRunStatus::ValidationFailed;
                    }
                    let sdk_ev =
                        SdkEvent::ValidationCompleted(crate::event::ValidationCompletedEvent {
                            run_id: run_id.clone(),
                            report: report.clone(),
                        });
                    emit_event(&runtime, &event_tx, storage_session.as_ref(), sdk_ev).await;
                    validations.push(report);
                }
            } else {
                let d = Diagnostic::warning(
                    "validation.requires_filesystem",
                    "validation commands require a filesystem-backed workspace",
                );
                diagnostics.push(d.clone());
                for cmd in &request.validation.commands {
                    validations.push(ValidationReport::skipped(
                        cmd.clone(),
                        "no filesystem root for command execution",
                    ));
                }
            }
        } else {
            for cmd in &request.validation.commands {
                validations.push(ValidationReport::skipped(
                    cmd.clone(),
                    "command_execution disabled by side-effect policy",
                ));
            }
            let d = Diagnostic::warning(
                "tool.side_effect_denied",
                "validation commands require allow_command_execution",
            );
            diagnostics.push(d);
        }
    }

    // Emit any new diagnostics that haven't been forwarded yet.
    for diag in &diagnostics {
        let sdk_ev = SdkEvent::Diagnostic(DiagnosticEvent {
            run_id: run_id.clone(),
            diagnostic: diag.clone(),
        });
        emit_event(&runtime, &event_tx, storage_session.as_ref(), sdk_ev).await;
    }

    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let events_summary = EventSummary {
        total_events: events_total,
        tool_calls,
        approvals_requested,
        files_changed: changed_files.len(),
        diagnostics_count: diagnostics.len(),
        duration_ms,
    };

    status = finalize_cancel_status(
        status,
        cancellation.is_cancelled(),
        deadline_fired.load(Ordering::SeqCst),
    );

    let summary = if final_summary.is_empty() {
        match status {
            AgentRunStatus::Completed => "run completed".to_string(),
            AgentRunStatus::Cancelled => "run cancelled".to_string(),
            AgentRunStatus::TimedOut => "run timed out".to_string(),
            AgentRunStatus::Failed => "run failed".to_string(),
            _ => format!("{status:?}"),
        }
    } else {
        final_summary
    };

    Ok(AgentRunResult {
        run_id,
        session_id,
        status,
        summary,
        artifacts: CodeArtifacts::empty(),
        patch,
        diagnostics,
        validations,
        plan: None,
        usage,
        provider: provider_id,
        model,
        events_summary,
        metadata,
        changed_files,
    })
}

/// Resolve the terminal status after the regular run loop, applying the two
/// cancellation demotions the finalization path performs (#212, #149):
///
/// - An external/budget cancel that arrived while the run otherwise reported
///   `Completed` demotes to `Cancelled` (the loop broke on cancel before the
///   core `Result` event landed, so `Completed` is stale).
/// - A cancel that coincides with the deadline-watcher flag (`deadline_fired`)
///   is a deadline expiry and surfaces as `TimedOut` rather than `Cancelled`.
///   External cancels leave the flag clear and stay `Cancelled`.
///
/// A status that is already terminal in some other way (e.g. `Failed` from a
/// budget breach, `ValidationFailed`) is sticky and is never overwritten here
/// — the deadline demotion only ever upgrades a `Cancelled` to `TimedOut`.
pub(crate) fn finalize_cancel_status(
    status: AgentRunStatus,
    is_cancelled: bool,
    deadline_fired: bool,
) -> AgentRunStatus {
    let mut status = status;
    if is_cancelled && status == AgentRunStatus::Completed {
        status = AgentRunStatus::Cancelled;
    }
    if status == AgentRunStatus::Cancelled && deadline_fired {
        status = AgentRunStatus::TimedOut;
    }
    status
}

/// Build a minimal `AgentRunResult` representing a cancelled SDK run.
/// Used when cancellation fires before / during a plan/act sub-task so
/// the SDK can return immediately without waiting for the underlying
/// core driver to wind down (#145, #155 follow-up: covers the plan/act
/// paths the codex reviewer flagged as still uncancellable).
#[allow(clippy::too_many_arguments)]
fn cancelled_result(
    run_id: RunId,
    session_id: Option<SessionId>,
    provider: code_agent_auth::ProviderId,
    model: String,
    metadata: crate::request::RequestMetadata,
    started: Instant,
    reason: &str,
    timed_out: bool,
) -> AgentRunResult {
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    // A deadline-driven cancel must surface as TimedOut, not Cancelled, on the
    // plan-then-act paths too — mirroring the regular run finalization
    // (#212 / #40).
    let status = if timed_out {
        AgentRunStatus::TimedOut
    } else {
        AgentRunStatus::Cancelled
    };
    AgentRunResult {
        run_id,
        session_id,
        status,
        summary: String::new(),
        artifacts: CodeArtifacts::empty(),
        patch: None,
        diagnostics: vec![Diagnostic::warning("run.cancelled", reason.to_string())],
        validations: Vec::new(),
        plan: None,
        usage: None,
        provider,
        model,
        events_summary: EventSummary {
            total_events: 0,
            tool_calls: 0,
            approvals_requested: 0,
            files_changed: 0,
            diagnostics_count: 1,
            duration_ms,
        },
        metadata,
        changed_files: Vec::new(),
    }
}

/// Run a standalone plan.
pub(crate) async fn execute_plan(
    runtime: Arc<RuntimeInner>,
    request: PlanRequest,
) -> Result<PlanResult, AgentSdkError> {
    let workspace = resolve_workspace(&runtime, request.workspace).await?;
    let mut options = AgentOptions {
        model: request
            .model
            .clone()
            .unwrap_or_else(|| runtime.default_model.clone()),
        ..Default::default()
    };
    if let Some(root) = workspace.filesystem_root() {
        options.cwd = root;
    }
    options.system_prompt = runtime.default_system_prompt.clone();
    options.project_instructions = runtime.default_project_instructions.clone();
    let planning = PlanningOptions {
        enabled: true,
        planning_mode: request.planning_mode,
        ..Default::default()
    };
    let provider = runtime.provider.clone();
    let tools = build_tool_registry(&runtime.tool_policy, workspace.clone());
    options.allowed_tools = tools.names();

    let plan_req = code_agent_core::plan::PlanRequest {
        prompt: request.task.clone(),
        options: options.clone(),
        planning: planning.clone(),
        provider,
        tools,
        plan_store: None,
        parent_plan_id: None,
        created_by: PlanCreatedBy::Agent,
    };
    let started = Instant::now();
    let outcome = code_agent_core::plan::generate_plan(plan_req)
        .await
        .map_err(|e: PlanError| AgentSdkError::planning("plan.failed", e.to_string()))?;
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    if outcome.fallback_used {
        diagnostics.push(Diagnostic::info(
            "plan.unstructured_fallback",
            "model returned a textual plan instead of using the SubmitPlan tool",
        ));
    }
    // Best-effort: persist the plan if the runtime has storage. A
    // storage failure is logged but does not fail plan generation (#151).
    if let Err(e) = runtime.storage.save_plan(&outcome.plan).await {
        tracing::warn!(error = %e, "failed to persist plan to storage");
    }
    Ok(PlanResult {
        plan: outcome.plan,
        diagnostics,
        usage: outcome.events.iter().rev().find_map(|ev| match ev {
            AgentEvent::Result(r) => r.usage.clone(),
            _ => None,
        }),
        events_summary: EventSummary {
            total_events: outcome.events.len(),
            tool_calls: outcome
                .events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolCallStarted(_)))
                .count(),
            approvals_requested: 0,
            files_changed: 0,
            diagnostics_count: 0,
            duration_ms,
        },
    })
}

/// Context that lets `execute_act` route the act phase's core
/// `AgentEvent` stream through the same `SdkEventMapper` + `emit_event`
/// path the regular run uses (#34). Plan-then-act consumers therefore see
/// the same SdkEvents the mapper produces for a regular run — tool-call
/// started/completed, assistant messages/deltas, tool-call denied, plus the
/// `PatchProposed`/diagnostic events emitted around the run.
/// `ApprovalPromptShown`/`ApprovalPromptResolved` AgentEvents are counted in
/// `EventSummary.approvals_requested` but are not mapped to dedicated
/// SdkEvents (a mapper-level gap shared with the regular run path).
/// When `None`, `execute_act` runs silently (the standalone `act()` entry
/// point on the runtime collects events into the result but does not own an
/// event channel).
pub(crate) struct ActEmitContext<'a> {
    /// Reuse the parent run's id so emitted act events share the same
    /// `run_id` as `PlanCreated` / `RunStarted` / `RunCompleted`.
    pub run_id: RunId,
    pub event_tx: &'a mpsc::Sender<Result<SdkEvent, AgentSdkError>>,
    pub storage_session: Option<SessionId>,
    pub mapper: &'a SdkEventMapper,
}

pub(crate) async fn execute_act(
    runtime: Arc<RuntimeInner>,
    request: ActRequest,
    emit: Option<ActEmitContext<'_>>,
) -> Result<AgentRunResult, AgentSdkError> {
    let workspace = resolve_workspace(&runtime, request.workspace).await?;
    // When driven from the plan-then-act path, reuse the parent run id so
    // every emitted act event is tagged with the same run as the plan.
    let run_id = match &emit {
        Some(ctx) => ctx.run_id.clone(),
        None => RunId::new(),
    };
    let metadata = request.metadata.clone();
    let model = request
        .model
        .clone()
        .unwrap_or_else(|| runtime.default_model.clone());
    let provider_id = runtime.provider_id.clone();

    // Apply patch policy by wrapping the workspace.
    let workspace_for_run: Arc<dyn Workspace> = match request.patch_policy {
        PatchPolicy::ReturnOnly => Arc::new(PatchOnlyWorkspace::new(workspace.clone())),
        PatchPolicy::RejectMutations => Arc::new(ReadOnlyWorkspace::new(workspace.clone())),
        _ => workspace.clone(),
    };

    // Prep plan store under a temp dir so the core driver can persist
    // intermediate state. We use an in-memory store wrapper.
    let mut plan = request.plan.clone();
    if request.test_force_approval || !matches!(plan.status, PlanStatus::Approved) {
        plan.status = PlanStatus::Approved;
        plan.approval = Some(code_agent_core::plan::PlanApproval {
            approved_at: time::OffsetDateTime::now_utc(),
            approved_by: code_agent_core::plan::PlanApprover::TestHarness,
            approved_version: plan.version,
            notes: Some("SDK auto-approval".to_string()),
            scope: code_agent_core::plan::PlanApprovalScope::FullPlan,
        });
    }
    let store = std::sync::Arc::new(InMemoryPlanStore::with_plan(plan.clone()));

    let started = Instant::now();
    let mut options = AgentOptions {
        model: model.clone(),
        ..Default::default()
    };
    if let Some(root) = workspace_for_run.filesystem_root() {
        options.cwd = root;
    }
    options.system_prompt = runtime.default_system_prompt.clone();
    options.project_instructions = runtime.default_project_instructions.clone();
    options.permission_callback = Some(Arc::new(SdkPermissionCallback {
        inner: runtime.approval_callback.clone(),
        policy: request.approval_policy,
        interactivity: runtime.interactivity,
        run_id: run_id.clone(),
        session_id: None,
        metadata: metadata.clone(),
    }));
    let tools = build_tool_registry(&runtime.tool_policy, workspace_for_run.clone());

    let act_req = code_agent_core::plan::ActRequest {
        plan_id: plan.id.clone(),
        options,
        planning: PlanningOptions {
            enabled: true,
            ..Default::default()
        },
        provider: runtime.provider.clone(),
        tools,
        plan_store: store.clone(),
        user_callback: None,
        deviation_callback: Some(Arc::new(code_agent_core::plan::DenyAllDeviations)),
        prompt: None,
    };
    let outcome: ActOutcome = code_agent_core::plan::act_plan(act_req)
        .await
        .map_err(|e| AgentSdkError::planning("plan.act_failed", e.to_string()))?;

    // Account for the act phase's core AgentEvent stream and (when driven
    // from the plan-then-act path) route it through the same mapper +
    // emit_event path the regular run uses, so plan-then-act consumers
    // observe tool-call/validation events (#34). The core `act_plan`
    // driver collects events rather than streaming them, so we replay the
    // collected vector here in order; ordering is preserved.
    //
    // Per-event accounting (tool calls, approvals) is unconditional so the
    // returned EventSummary is correct for both the plan-then-act path and
    // the standalone `act()` entry (which passes `emit: None`). Only
    // `emit_event` is gated on `emit.is_some()`.
    //
    // Mapping covers exactly the regular-run set: assistant messages, tool
    // calls started/completed, and denials. `SdkEventMapper::map` does not
    // map `ApprovalPromptShown`/`ApprovalPromptResolved` to a dedicated
    // SdkEvent, so approval *events* are not surfaced here — but neither are
    // they on the regular run path, which also emits solely via
    // `mapper.map`. That approval-event mapping gap is shared by both paths
    // and is out of scope for this change.
    let mut tool_calls = 0usize;
    let mut approvals_requested = 0usize;
    for ev in &outcome.events {
        match ev {
            AgentEvent::ToolCallStarted(_) => tool_calls += 1,
            AgentEvent::ApprovalPromptShown(_) => approvals_requested += 1,
            _ => {}
        }
        if let Some(ctx) = &emit {
            if let Some(sdk_ev) = ctx.mapper.map(ev) {
                emit_event(&runtime, ctx.event_tx, ctx.storage_session.as_ref(), sdk_ev).await;
            }
        }
    }

    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for d in &outcome.deviations {
        let reason = if d.reason.is_empty() {
            "plan deviation".to_string()
        } else {
            d.reason.clone()
        };
        diagnostics.push(Diagnostic::warning("plan.deviation_detected", reason));
    }

    // Compute the patch from the wrapped workspace.
    let patch = match request.patch_policy {
        PatchPolicy::ReturnOnly | PatchPolicy::ApplyAndReturn => {
            workspace_for_run.diff(DiffBase::Initial).await.ok()
        }
        _ => None,
    };

    let summary = outcome.plan.summary.clone();
    let mut changed_files = Vec::new();
    if let Some(p) = &patch {
        for f in &p.files {
            match f {
                FilePatch::Create { path, .. }
                | FilePatch::Modify { path, .. }
                | FilePatch::Delete { path } => changed_files.push(path.clone()),
                FilePatch::Rename { to, .. } => changed_files.push(to.clone()),
            }
        }
    }

    // When driven from the plan-then-act path, emit a PatchProposed event
    // (mirroring the regular run) plus any deviation diagnostics, so the
    // act phase's outcome is observable on the event stream as well.
    if let Some(ctx) = &emit {
        if let Some(p) = &patch {
            if !p.files.is_empty() {
                let sdk_ev = SdkEvent::PatchProposed(PatchProposedEvent {
                    run_id: run_id.clone(),
                    patch: p.clone(),
                });
                emit_event(&runtime, ctx.event_tx, ctx.storage_session.as_ref(), sdk_ev).await;
            }
        }
        for diag in &diagnostics {
            let sdk_ev = SdkEvent::Diagnostic(DiagnosticEvent {
                run_id: run_id.clone(),
                diagnostic: diag.clone(),
            });
            emit_event(&runtime, ctx.event_tx, ctx.storage_session.as_ref(), sdk_ev).await;
        }
    }

    let diagnostics_count = diagnostics.len();
    Ok(AgentRunResult {
        run_id,
        session_id: None,
        status: AgentRunStatus::Completed,
        summary,
        artifacts: CodeArtifacts::empty(),
        patch,
        diagnostics,
        validations: Vec::new(),
        plan: Some(outcome.plan),
        usage: outcome.summary.usage.clone(),
        provider: provider_id,
        model,
        events_summary: EventSummary {
            total_events: outcome.events.len(),
            tool_calls,
            approvals_requested,
            files_changed: changed_files.len(),
            diagnostics_count,
            duration_ms,
        },
        metadata,
        changed_files,
    })
}

/// Snapshot every readable file in `workspace` into a `BTreeMap`. Used by
/// `PatchPolicy::ReturnOnly` on a filesystem workspace to capture the
/// pristine state so we can compute a diff and roll back tool-driven disk
/// writes that bypass the SDK `Workspace` trait.
///
/// A `list_files` error is fatal — without a definitive file list we
/// would silently delete every existing file on rollback (the original
/// #144 bug). Per-file `read_file` errors are split:
/// - `workspace.not_found` (an external process deleted the file
///   between `list_files` and `read_file`) is treated as benign and
///   skipped. Failing the run on a transient external delete is worse
///   than skipping a file we never owned anyway (gemini review).
/// - Any other error (EACCES, EIO, ...) is fatal: rollback cannot
///   safely undo what it has not captured.
async fn snapshot_workspace(
    workspace: &dyn Workspace,
) -> Result<std::collections::BTreeMap<WorkspacePath, Vec<u8>>, AgentSdkError> {
    let mut out = std::collections::BTreeMap::new();
    let files = workspace
        .list_files(crate::workspace::FileQuery::new())
        .await
        .map_err(|e| {
            AgentSdkError::new(
                AgentSdkErrorKind::Workspace,
                format!("workspace.snapshot_failed: list_files: {e}"),
            )
        })?;
    for path in files {
        match workspace.read_file(&path).await {
            Ok(content) => {
                out.insert(path, content.bytes);
            }
            // External delete between list_files and read_file: the
            // file is no longer ours to track; do not abort the run.
            Err(e) if e.code == "workspace.not_found" => continue,
            Err(e) => {
                return Err(AgentSdkError::new(
                    AgentSdkErrorKind::Workspace,
                    format!(
                        "workspace.snapshot_failed: read_file {}: {}",
                        path.as_str(),
                        e
                    ),
                ));
            }
        }
    }
    Ok(out)
}

/// Restore `workspace` to match `snapshot`. Used after a `ReturnOnly` run
/// to roll back any mutations the model performed through filesystem
/// tools.
///
/// Errors from `delete_file` and `write_file` are collected and
/// surfaced as a single `workspace.rollback_failed` error so partial
/// rollback can still complete (one bad file does not block the rest)
/// while the caller still learns the workspace is inconsistent.
///
/// Per-file resilience tweaks from review feedback:
/// - `workspace.not_found` on delete is benign (file already gone) and
///   is not recorded as an error.
/// - A `list_files` failure does *not* skip rollback entirely. We fall
///   back to "restore every snapshot entry", which is the best we can
///   do without an authoritative current-files list. New files created
///   during the run that we cannot enumerate stay on disk — that is
///   strictly safer than full inaction.
async fn restore_workspace(
    workspace: &dyn Workspace,
    snapshot: &std::collections::BTreeMap<WorkspacePath, Vec<u8>>,
) -> Result<(), AgentSdkError> {
    let mut errors: Vec<String> = Vec::new();
    match workspace
        .list_files(crate::workspace::FileQuery::new())
        .await
    {
        Ok(current) => {
            for path in &current {
                if !snapshot.contains_key(path) {
                    // File was created during the run; delete it. A
                    // concurrent external delete is fine.
                    match workspace.delete_file(path).await {
                        Ok(()) => {}
                        Err(e) if e.code == "workspace.not_found" => continue,
                        Err(e) => errors.push(format!("delete {}: {}", path.as_str(), e)),
                    }
                }
            }
        }
        Err(e) => {
            // Cannot enumerate; record the failure but still restore
            // the snapshot below so original file content gets put back.
            errors.push(format!("list_files: {e}"));
        }
    }
    for (path, bytes) in snapshot {
        // Restore original content (or recreate deleted file).
        if let Err(e) = workspace.write_file(path, bytes).await {
            errors.push(format!("write {}: {}", path.as_str(), e));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AgentSdkError::new(
            AgentSdkErrorKind::Workspace,
            format!("workspace.rollback_failed: {}", errors.join("; ")),
        ))
    }
}

/// Resolve the workspace for a request. The request's workspace overrides
/// the runtime default; if neither is set we hand back an empty in-memory
/// workspace.
async fn resolve_workspace(
    runtime: &Arc<RuntimeInner>,
    request_ws: Option<WorkspaceSource>,
) -> Result<Arc<dyn Workspace>, AgentSdkError> {
    if let Some(ws) = request_ws {
        return ws.into_workspace();
    }
    if let Some(ws) = &runtime.workspace {
        return Ok(ws.clone());
    }
    Ok(Arc::new(InMemoryWorkspace::new()))
}

/// Build a `ToolRegistry` from the SDK's `ToolPolicy`. Honors the builtin
/// set and side-effect policy by selectively registering core tools. Custom
/// tools always run regardless of the builtin set.
fn build_tool_registry(
    policy: &crate::policy::ToolPolicy,
    _workspace: Arc<dyn Workspace>,
) -> ToolRegistry {
    use code_agent_tools::{
        BashTool, EditTool, GlobTool, GrepTool, ReadTool, ToolLimits, WriteTool,
    };
    let mut registry = ToolRegistry::new();
    let limits = Arc::new(ToolLimits::default());
    let builtins = policy.enable_builtin_tools;
    if builtins.read {
        registry.register(Arc::new(ReadTool::new(limits.clone())));
    }
    if builtins.glob {
        registry.register(Arc::new(GlobTool::new(limits.clone())));
    }
    if builtins.grep {
        registry.register(Arc::new(GrepTool::new(limits.clone())));
    }
    if builtins.write && policy.side_effect_policy.allow_workspace_write {
        registry.register(Arc::new(WriteTool::new(limits.clone())));
    }
    if builtins.edit && policy.side_effect_policy.allow_workspace_write {
        registry.register(Arc::new(EditTool::new(limits.clone())));
    }
    if builtins.bash && policy.side_effect_policy.allow_command_execution {
        registry.register(Arc::new(BashTool::new(limits.clone())));
    }
    if builtins.todo {
        registry.register(Arc::new(code_agent_tools::TodoWriteTool::new()));
    }
    for tool in &policy.custom_tools {
        registry.register(tool.clone());
    }
    registry
}

/// Quick adapter wrapping a single plan inside an in-memory PlanStore-like
/// object. Avoids depending on `FsPlanStore` for the SDK act flow.
struct InMemoryPlanStore {
    plans:
        tokio::sync::Mutex<std::collections::HashMap<code_agent_core::plan::PlanId, PlanDocument>>,
}

impl InMemoryPlanStore {
    fn with_plan(plan: PlanDocument) -> Self {
        let mut map = std::collections::HashMap::new();
        map.insert(plan.id.clone(), plan);
        Self {
            plans: tokio::sync::Mutex::new(map),
        }
    }
}

#[async_trait::async_trait]
impl code_agent_core::plan::PlanStore for InMemoryPlanStore {
    async fn create(&self, plan: PlanDocument) -> Result<code_agent_core::plan::PlanId, PlanError> {
        let id = plan.id.clone();
        self.plans.lock().await.insert(id.clone(), plan);
        Ok(id)
    }
    async fn save_revision(&self, plan: PlanDocument) -> Result<(), PlanError> {
        self.plans.lock().await.insert(plan.id.clone(), plan);
        Ok(())
    }
    async fn load(&self, id: &code_agent_core::plan::PlanId) -> Result<PlanDocument, PlanError> {
        self.plans
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| PlanError::NotFound(id.to_string()))
    }
    async fn list(
        &self,
        _filter: code_agent_core::plan::PlanListFilter,
    ) -> Result<Vec<code_agent_core::plan::PlanSummary>, PlanError> {
        Ok(Vec::new())
    }
    async fn approve(
        &self,
        request: code_agent_core::plan::PlanApprovalRequest,
    ) -> Result<PlanDocument, PlanError> {
        let mut plans = self.plans.lock().await;
        let plan = plans
            .get_mut(&request.plan_id)
            .ok_or_else(|| PlanError::NotFound(request.plan_id.to_string()))?;
        plan.status = code_agent_core::plan::PlanStatus::Approved;
        plan.approval = Some(code_agent_core::plan::PlanApproval {
            approved_at: time::OffsetDateTime::now_utc(),
            approved_by: request.approved_by,
            approved_version: request.approved_version,
            notes: request.notes,
            scope: request.scope,
        });
        Ok(plan.clone())
    }
    async fn reject(
        &self,
        request: code_agent_core::plan::PlanRejectRequest,
    ) -> Result<PlanDocument, PlanError> {
        let mut plans = self.plans.lock().await;
        let plan = plans
            .get_mut(&request.plan_id)
            .ok_or_else(|| PlanError::NotFound(request.plan_id.to_string()))?;
        plan.status = code_agent_core::plan::PlanStatus::Rejected;
        Ok(plan.clone())
    }
    async fn archive(&self, id: &code_agent_core::plan::PlanId) -> Result<(), PlanError> {
        self.plans.lock().await.remove(id);
        Ok(())
    }
    async fn latest_for_session(
        &self,
        _session_id: &code_agent_core::SessionId,
    ) -> Result<Option<code_agent_core::plan::PlanId>, PlanError> {
        Ok(self.plans.lock().await.keys().next().cloned())
    }
    async fn writable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod status_tests {
    //! Branch coverage for the run finalization helpers that decide the
    //! terminal status: `finalize_cancel_status` (the
    //! `Completed`→`Cancelled`→`TimedOut` demotion chain, #212/#149) and
    //! `cancelled_result` (the plan/act cancel short-circuit's TimedOut vs
    //! Cancelled mapping, #212/#40). These are the pure state decisions the
    //! single integration file never isolates.

    use super::*;

    // A clean completion with no cancellation stays Completed.
    #[test]
    fn finalize_completed_without_cancel_stays_completed() {
        let s = finalize_cancel_status(AgentRunStatus::Completed, false, false);
        assert_eq!(s, AgentRunStatus::Completed);
    }

    // An external cancel observed while the status is still the loop's
    // default `Completed` demotes to `Cancelled` (the loop broke on cancel
    // before the core Result event upgraded the status).
    #[test]
    fn finalize_external_cancel_demotes_completed_to_cancelled() {
        let s = finalize_cancel_status(AgentRunStatus::Completed, true, false);
        assert_eq!(s, AgentRunStatus::Cancelled);
    }

    // A cancel that coincides with the deadline-watcher flag is a deadline
    // expiry and surfaces as TimedOut rather than Cancelled (#212).
    #[test]
    fn finalize_deadline_cancel_becomes_timed_out() {
        let s = finalize_cancel_status(AgentRunStatus::Completed, true, true);
        assert_eq!(s, AgentRunStatus::TimedOut);
    }

    // The deadline flag alone (no cancellation observed) must NOT change a
    // Completed status: only a Cancelled status is upgraded to TimedOut.
    // Pins the `status == Cancelled` guard so a stray flag can't time-out a
    // run that actually finished.
    #[test]
    fn finalize_deadline_flag_without_cancel_keeps_completed() {
        let s = finalize_cancel_status(AgentRunStatus::Completed, false, true);
        assert_eq!(s, AgentRunStatus::Completed);
    }

    // A sticky Failed status (e.g. a budget breach) is never overwritten by
    // the cancellation demotions, even if the run was also cancelled and the
    // deadline fired. This is the budget-Failed sticky guard: cancelling the
    // run to stop it after a budget breach must not relabel it as
    // Cancelled/TimedOut (#149).
    #[test]
    fn finalize_failed_is_sticky_under_cancel_and_deadline() {
        let s = finalize_cancel_status(AgentRunStatus::Failed, true, true);
        assert_eq!(s, AgentRunStatus::Failed);
    }

    // ValidationFailed is likewise sticky — it is terminal and not a
    // Cancelled, so neither demotion applies.
    #[test]
    fn finalize_validation_failed_is_sticky() {
        let s = finalize_cancel_status(AgentRunStatus::ValidationFailed, true, true);
        assert_eq!(s, AgentRunStatus::ValidationFailed);
    }

    fn meta() -> crate::request::RequestMetadata {
        crate::request::RequestMetadata::default()
    }

    // cancelled_result with timed_out=false yields Cancelled, an empty
    // summary, and exactly one run.cancelled warning diagnostic carrying the
    // supplied reason.
    #[test]
    fn cancelled_result_maps_external_cancel_to_cancelled() {
        let r = cancelled_result(
            RunId::new(),
            None,
            code_agent_auth::ProviderId::Fake,
            "fake-model".to_string(),
            meta(),
            Instant::now(),
            "plan generation cancelled",
            false,
        );
        assert_eq!(r.status, AgentRunStatus::Cancelled);
        assert!(r.summary.is_empty());
        assert_eq!(r.diagnostics.len(), 1);
        assert_eq!(r.diagnostics[0].code, "run.cancelled");
        assert!(r.diagnostics[0]
            .message
            .contains("plan generation cancelled"));
        assert_eq!(r.events_summary.diagnostics_count, 1);
    }

    // cancelled_result with timed_out=true (the deadline-driven plan/act
    // cancel) surfaces TimedOut, mirroring the regular-run finalization.
    #[test]
    fn cancelled_result_maps_deadline_cancel_to_timed_out() {
        let r = cancelled_result(
            RunId::new(),
            None,
            code_agent_auth::ProviderId::Fake,
            "fake-model".to_string(),
            meta(),
            Instant::now(),
            "plan act cancelled",
            true,
        );
        assert_eq!(r.status, AgentRunStatus::TimedOut);
        assert_eq!(r.diagnostics.len(), 1);
    }
}

#[cfg(all(test, feature = "fake-provider"))]
mod budget_tests {
    //! End-to-end coverage of the `max_tool_calls` budget gate boundary in
    //! `execute_request`. The gate trips only once the running tool-call
    //! count *exceeds* the cap (`tool_calls > max`), so a run that uses
    //! exactly `max` calls still completes while `max + 1` fails with the
    //! `budget.max_tool_calls_exceeded` diagnostic, and that diagnostic is
    //! pushed at most once (#149). Driven with the scripted fake provider so
    //! the tool-call count is deterministic.

    use super::*;
    use crate::fake_provider::ScriptedFakeProvider;
    use crate::policy::{BuiltinToolSet, SideEffectPolicy, ToolPolicy};
    use crate::request::AgentRequest;
    use crate::{AgentRuntime, AllowAllApprovals, InteractivityMode, ProviderConfig, RunBudgets};
    use code_agent_core::{ModelEvent, TokenUsage, ToolCall};

    /// A scripted provider that issues `n` Write tool calls (each to a
    /// distinct file) across `n` turns, then a final text turn. Each Write is
    /// counted as one tool call by the runtime's `ToolCallStarted` tracking.
    fn provider_with_writes(n: usize) -> ScriptedFakeProvider {
        let mut turns: Vec<Vec<ModelEvent>> = Vec::new();
        for i in 0..n {
            turns.push(vec![
                ModelEvent::ToolCall(ToolCall {
                    id: format!("w{i}"),
                    name: "Write".into(),
                    input: serde_json::json!({
                        "file_path": format!("f{i}.txt"),
                        "content": "x\n",
                    }),
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: Some(TokenUsage::default()),
                },
            ]);
        }
        turns.push(vec![
            ModelEvent::TextDelta {
                text: "done".into(),
            },
            ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: Some(TokenUsage::default()),
            },
        ]);
        ScriptedFakeProvider::new(turns)
    }

    async fn run_with_budget(writes: usize, max_tool_calls: u32) -> AgentRunResult {
        // Point the run at a temp-dir filesystem workspace so the Write tool's
        // disk writes are isolated and never litter the crate's source tree.
        let dir = tempfile::TempDir::new().expect("temp dir");
        let runtime = AgentRuntime::builder()
            .provider(ProviderConfig::fake(Arc::new(provider_with_writes(writes))))
            .workspace(crate::workspace::WorkspaceSource::filesystem(dir.path()))
            .tool_policy(ToolPolicy {
                enable_builtin_tools: BuiltinToolSet::cli_defaults(),
                side_effect_policy: SideEffectPolicy::full(),
                ..Default::default()
            })
            .interactivity(InteractivityMode::Headless)
            .approval_callback(Arc::new(AllowAllApprovals))
            .build()
            .await
            .expect("build runtime");
        runtime
            .run_once(
                AgentRequest::new("write files")
                    .interactivity(InteractivityMode::Headless)
                    .max_turns((writes + 2) as u32)
                    .budgets(RunBudgets::default().with_max_tool_calls(max_tool_calls)),
            )
            .await
            .expect("run completes")
    }

    // Exactly `max` tool calls: the gate uses `> max`, so a run that uses its
    // full allowance is NOT failed and emits no budget diagnostic.
    #[tokio::test]
    async fn exactly_max_tool_calls_is_not_failed() {
        let result = run_with_budget(2, 2).await;
        assert_ne!(
            result.status,
            AgentRunStatus::Failed,
            "a run at exactly max_tool_calls must not be Failed"
        );
        assert!(
            !result
                .diagnostics
                .iter()
                .any(|d| d.code == "budget.max_tool_calls_exceeded"),
            "no budget diagnostic expected at the allowance boundary"
        );
    }

    // `max + 1` tool calls: the gate trips. The run ends in a terminal
    // non-success state and records exactly one
    // `budget.max_tool_calls_exceeded` diagnostic — the #149 one-shot guard
    // prevents the duplicate diagnostics that earlier re-fired on every event
    // after the cap was crossed. (The exact terminal status is Failed or
    // Cancelled depending on whether the post-gate `cancel()` is observed by
    // the loop's select before it breaks; both are non-success, so we pin the
    // diagnostic-once invariant rather than the racy status.)
    #[tokio::test]
    async fn exceeding_max_tool_calls_fails_once() {
        let result = run_with_budget(2, 1).await;
        assert!(
            matches!(
                result.status,
                AgentRunStatus::Failed | AgentRunStatus::Cancelled
            ),
            "expected a terminal non-success status, got {:?}",
            result.status
        );
        let count = result
            .diagnostics
            .iter()
            .filter(|d| d.code == "budget.max_tool_calls_exceeded")
            .count();
        assert_eq!(count, 1, "budget diagnostic must be pushed exactly once");
    }
}

#[cfg(test)]
mod snapshot_tests {
    //! Tests for the `snapshot_workspace` / `restore_workspace` helpers
    //! that underpin `PatchPolicy::ReturnOnly`'s rollback (#144).

    use super::*;
    use crate::workspace::{
        ApplyPatchReport, DiffBase, FileContent, FileQuery, SearchMatch, SearchQuery,
        WorkspaceCapabilities, WorkspaceError, WorkspacePath,
    };
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// Workspace whose every method can be configured to error. Used to
    /// drive the fail-soft → fail-hard regression for #144.
    struct FaultyWorkspace {
        files: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        fail_list: bool,
        fail_read: bool,
        fail_write: bool,
        fail_delete: bool,
    }

    impl FaultyWorkspace {
        fn empty() -> Self {
            Self {
                files: Mutex::new(Default::default()),
                fail_list: false,
                fail_read: false,
                fail_write: false,
                fail_delete: false,
            }
        }
        fn with_file(name: &str, bytes: &[u8]) -> Self {
            let s = Self::empty();
            s.files.lock().unwrap().insert(name.into(), bytes.to_vec());
            s
        }
    }

    #[async_trait]
    impl Workspace for FaultyWorkspace {
        async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
            if self.fail_read {
                return Err(WorkspaceError::read_failed(
                    path.clone(),
                    "forced read failure",
                ));
            }
            let g = self.files.lock().unwrap();
            let bytes = g
                .get(path.as_str())
                .cloned()
                .ok_or_else(|| WorkspaceError::not_found(path.clone()))?;
            Ok(FileContent::new_text(bytes))
        }
        async fn write_file(
            &self,
            path: &WorkspacePath,
            content: &[u8],
        ) -> Result<(), WorkspaceError> {
            if self.fail_write {
                return Err(WorkspaceError::write_failed(
                    path.clone(),
                    "forced write failure",
                ));
            }
            self.files
                .lock()
                .unwrap()
                .insert(path.as_str().to_string(), content.to_vec());
            Ok(())
        }
        async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError> {
            if self.fail_delete {
                return Err(WorkspaceError::new(
                    "workspace.delete_failed",
                    "forced delete failure",
                )
                .with_path(path.clone()));
            }
            self.files.lock().unwrap().remove(path.as_str());
            Ok(())
        }
        async fn list_files(&self, _q: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
            if self.fail_list {
                return Err(WorkspaceError::new(
                    "workspace.list_failed",
                    "forced list failure",
                ));
            }
            let g = self.files.lock().unwrap();
            let mut out = Vec::new();
            for name in g.keys() {
                out.push(WorkspacePath::new(name).unwrap());
            }
            Ok(out)
        }
        async fn search(&self, _q: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
            Ok(Vec::new())
        }
        async fn apply_patch(&self, _p: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
            Ok(ApplyPatchReport {
                files_changed: 0,
                files_skipped: 0,
                failures: Vec::new(),
            })
        }
        async fn diff(&self, _b: DiffBase) -> Result<PatchSet, WorkspaceError> {
            Ok(PatchSet::default())
        }
        fn capabilities(&self) -> WorkspaceCapabilities {
            WorkspaceCapabilities::full()
        }
    }

    // snapshot_workspace must FAIL when list_files errors, instead of
    // returning an empty snapshot. Returning an empty snapshot here was
    // the silent-data-loss bug in #144: the post-run rollback would
    // delete every existing file because none of them appeared in the
    // (empty) snapshot.
    #[tokio::test]
    async fn snapshot_fails_when_list_files_errors() {
        let ws = FaultyWorkspace {
            fail_list: true,
            ..FaultyWorkspace::with_file("kept.txt", b"keep me")
        };
        let err = snapshot_workspace(&ws)
            .await
            .expect_err("list_files error must abort snapshot");
        assert_eq!(err.kind, AgentSdkErrorKind::Workspace);
        assert!(err.message.contains("snapshot_failed"), "got: {err}");
    }

    // snapshot_workspace must FAIL when read_file errors on any single
    // file, instead of skipping it. Skipping it left the file out of
    // the snapshot; the post-run rollback would then treat it as a new
    // file the agent created and delete it.
    #[tokio::test]
    async fn snapshot_fails_when_read_file_errors() {
        let ws = FaultyWorkspace {
            fail_read: true,
            ..FaultyWorkspace::with_file("secret.bin", b"private")
        };
        let err = snapshot_workspace(&ws)
            .await
            .expect_err("read_file error must abort snapshot");
        assert_eq!(err.kind, AgentSdkErrorKind::Workspace);
        assert!(err.message.contains("secret.bin"), "got: {err}");
    }

    // restore_workspace must surface rollback failures (delete/write
    // errors) as an Err, not silently leave the workspace half-restored.
    #[tokio::test]
    async fn restore_surfaces_write_errors() {
        let ws = FaultyWorkspace {
            fail_write: true,
            ..FaultyWorkspace::empty()
        };
        let mut snap: std::collections::BTreeMap<WorkspacePath, Vec<u8>> =
            std::collections::BTreeMap::new();
        snap.insert(WorkspacePath::new("kept.txt").unwrap(), b"hi".to_vec());
        let err = restore_workspace(&ws, &snap)
            .await
            .expect_err("write error must surface");
        assert_eq!(err.kind, AgentSdkErrorKind::Workspace);
        assert!(err.message.contains("rollback_failed"), "got: {err}");
    }

    // restore_workspace must also surface list_files errors so the
    // run is marked failed instead of completed-as-if-rolled-back.
    // Updated for the gemini-review fix: a list_files failure no
    // longer aborts rollback entirely; the snapshot's tracked files
    // are still restored. We assert the error is surfaced AND a
    // snapshot entry was still written.
    #[tokio::test]
    async fn restore_surfaces_list_errors() {
        let ws = FaultyWorkspace {
            fail_list: true,
            ..FaultyWorkspace::with_file("kept.txt", b"current")
        };
        let mut snap: std::collections::BTreeMap<WorkspacePath, Vec<u8>> =
            std::collections::BTreeMap::new();
        snap.insert(
            WorkspacePath::new("kept.txt").unwrap(),
            b"original".to_vec(),
        );
        let err = restore_workspace(&ws, &snap)
            .await
            .expect_err("list_files error must surface");
        assert_eq!(err.kind, AgentSdkErrorKind::Workspace);
        // Snapshot was still written despite list_files failure.
        let after = ws.files.lock().unwrap().get("kept.txt").cloned().unwrap();
        assert_eq!(after, b"original");
    }

    // Delete-error path: write_file succeeds for the snapshot, but
    // delete_file fails when restore_workspace tries to remove a file
    // that exists in `current` but not in `snapshot`. The rollback
    // must surface the delete failure as part of the aggregated
    // rollback_failed error.
    #[tokio::test]
    async fn restore_surfaces_delete_errors() {
        let ws = FaultyWorkspace {
            fail_delete: true,
            ..FaultyWorkspace::with_file("agent_created.txt", b"new")
        };
        // Empty snapshot: every file in `current` should be deleted.
        let snap: std::collections::BTreeMap<WorkspacePath, Vec<u8>> =
            std::collections::BTreeMap::new();
        let err = restore_workspace(&ws, &snap)
            .await
            .expect_err("delete error must surface");
        assert_eq!(err.kind, AgentSdkErrorKind::Workspace);
        assert!(err.message.contains("delete"), "got: {err}");
    }

    // External-delete TOCTOU on snapshot: a file disappears between
    // list_files and read_file. The not_found error is treated as
    // benign — the snapshot just omits the file, the run continues.
    #[tokio::test]
    async fn snapshot_skips_files_deleted_between_list_and_read() {
        struct VanishingWorkspace {
            inner: FaultyWorkspace,
        }
        #[async_trait]
        impl Workspace for VanishingWorkspace {
            async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError> {
                // First, every read returns not_found — simulating an
                // external process that deleted the file between
                // list_files and read_file.
                Err(WorkspaceError::not_found(path.clone()))
            }
            async fn write_file(&self, p: &WorkspacePath, c: &[u8]) -> Result<(), WorkspaceError> {
                self.inner.write_file(p, c).await
            }
            async fn delete_file(&self, p: &WorkspacePath) -> Result<(), WorkspaceError> {
                self.inner.delete_file(p).await
            }
            async fn list_files(&self, q: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError> {
                self.inner.list_files(q).await
            }
            async fn search(&self, q: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError> {
                self.inner.search(q).await
            }
            async fn apply_patch(&self, p: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError> {
                self.inner.apply_patch(p).await
            }
            async fn diff(&self, b: DiffBase) -> Result<PatchSet, WorkspaceError> {
                self.inner.diff(b).await
            }
            fn capabilities(&self) -> WorkspaceCapabilities {
                self.inner.capabilities()
            }
        }
        let ws = VanishingWorkspace {
            inner: FaultyWorkspace::with_file("vanishing.txt", b"hi"),
        };
        let snap = snapshot_workspace(&ws)
            .await
            .expect("snapshot must succeed despite external delete");
        // File was listed but read_file returned not_found: omit it.
        assert!(snap.is_empty(), "expected empty snapshot, got {snap:?}");
    }

    // snapshot_workspace happy path: a workspace with a single readable
    // file produces a one-entry snapshot keyed by that file's path.
    #[tokio::test]
    async fn snapshot_returns_files_on_success() {
        let ws = FaultyWorkspace::with_file("keep.txt", b"hello");
        let snap = snapshot_workspace(&ws).await.unwrap();
        assert_eq!(snap.len(), 1);
        let path = WorkspacePath::new("keep.txt").unwrap();
        assert_eq!(snap.get(&path).map(Vec::as_slice), Some(b"hello".as_ref()));
    }

    // AbortOnDrop must abort the spawned task when the guard is dropped,
    // so a leaked deadline watcher doesn't outlive its enclosing
    // execute_request call. (#164) Asserts via JoinError::is_cancelled
    // so the join reason is pinned to cancellation, not merely "finished".
    #[tokio::test]
    async fn abort_on_drop_aborts_pinned_task() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let woke = Arc::new(AtomicBool::new(false));
        let woke_clone = woke.clone();
        let handle = tokio::spawn(async move {
            // Long sleep that should NEVER fire because the guard
            // aborts the task first.
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            woke_clone.store(true, Ordering::SeqCst);
        });
        let abort_handle = handle.abort_handle();
        {
            let _guard = AbortOnDrop(abort_handle);
            // Guard drops here.
        }
        // Awaiting the handle should resolve with a cancelled JoinError.
        let join_err = handle.await.expect_err("task should be cancelled, not Ok");
        assert!(
            join_err.is_cancelled(),
            "join reason was not cancellation: {join_err:?}"
        );
        assert!(
            !woke.load(Ordering::SeqCst),
            "task body ran past abort point"
        );
    }
}
