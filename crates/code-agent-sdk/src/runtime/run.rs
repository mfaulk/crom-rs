//! Execution of `AgentRequest` against the underlying core agent loop.

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
use crate::event::{DiagnosticEvent, PatchProposedEvent, SdkEvent, SdkEventMapper};
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
    if let Some(d) = deadline {
        let c = cancellation.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(d) => c.cancel(),
                _ = c.cancelled() => {}
            }
        });
    }

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
                return Ok(AgentRunResult {
                    run_id,
                    session_id,
                    status: AgentRunStatus::Cancelled,
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
            )),
        };
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
        return tokio::select! {
            r = execute_act(runtime.clone(), act_req) => r,
            _ = cancellation.cancelled() => Ok(cancelled_result(
                run_id,
                None,
                provider_id,
                model,
                metadata,
                started,
                "plan act cancelled",
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
            let _ = event_tx.send(Ok(sdk_ev.clone())).await;
            if let Some(sink) = &runtime.event_sink {
                sink.emit(sdk_ev.clone()).await;
            }
            if let Some(s) = &storage_session {
                let _ = runtime.storage.append_event(s, sdk_ev).await;
            }
        }

        // Budget enforcement.
        if let Some(max) = request.budgets.max_tool_calls {
            if tool_calls > max as usize {
                cancellation.cancel();
                diagnostics.push(Diagnostic::error(
                    "budget.max_tool_calls_exceeded",
                    "max_tool_calls budget exceeded",
                ));
                status = AgentRunStatus::Failed;
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
                    let _ = event_tx.send(Ok(sdk_ev.clone())).await;
                    if let Some(sink) = &runtime.event_sink {
                        sink.emit(sdk_ev.clone()).await;
                    }
                    if let Some(s) = &storage_session {
                        let _ = runtime.storage.append_event(s, sdk_ev).await;
                    }
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
                    let _ = event_tx.send(Ok(sdk_ev.clone())).await;
                    if let Some(sink) = &runtime.event_sink {
                        sink.emit(sdk_ev.clone()).await;
                    }
                    if let Some(s) = &storage_session {
                        let _ = runtime.storage.append_event(s, sdk_ev).await;
                    }
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
        let _ = event_tx.send(Ok(sdk_ev.clone())).await;
        if let Some(sink) = &runtime.event_sink {
            sink.emit(sdk_ev.clone()).await;
        }
        if let Some(s) = &storage_session {
            let _ = runtime.storage.append_event(s, sdk_ev).await;
        }
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

    if cancellation.is_cancelled() && status == AgentRunStatus::Completed {
        status = AgentRunStatus::Cancelled;
    }

    let summary = if final_summary.is_empty() {
        match status {
            AgentRunStatus::Completed => "run completed".to_string(),
            AgentRunStatus::Cancelled => "run cancelled".to_string(),
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

/// Build a minimal `AgentRunResult` representing a cancelled SDK run.
/// Used when cancellation fires before / during a plan/act sub-task so
/// the SDK can return immediately without waiting for the underlying
/// core driver to wind down (#145, #155 follow-up: covers the plan/act
/// paths the codex reviewer flagged as still uncancellable).
fn cancelled_result(
    run_id: RunId,
    session_id: Option<SessionId>,
    provider: code_agent_auth::ProviderId,
    model: String,
    metadata: crate::request::RequestMetadata,
    started: Instant,
    reason: &str,
) -> AgentRunResult {
    let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    AgentRunResult {
        run_id,
        session_id,
        status: AgentRunStatus::Cancelled,
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
    // Best-effort: persist the plan if the runtime has storage.
    let _ = runtime.storage.save_plan(&outcome.plan).await;
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

pub(crate) async fn execute_act(
    runtime: Arc<RuntimeInner>,
    request: ActRequest,
) -> Result<AgentRunResult, AgentSdkError> {
    let workspace = resolve_workspace(&runtime, request.workspace).await?;
    let run_id = RunId::new();
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
            tool_calls: outcome
                .events
                .iter()
                .filter(|e| matches!(e, AgentEvent::ToolCallStarted(_)))
                .count(),
            approvals_requested: 0,
            files_changed: changed_files.len(),
            diagnostics_count: 0,
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
}
