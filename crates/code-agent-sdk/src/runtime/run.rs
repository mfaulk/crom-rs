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
use crate::error::{AgentSdkError, Diagnostic};
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

    // Snapshot the workspace state before the run for any patch policy that
    // needs to return a meaningful diff. Built-in tools write directly to
    // disk; without a baseline the post-run diff would treat every file in a
    // filesystem workspace as newly created. We snapshot for ReturnOnly
    // (where we also use the snapshot to roll back disk mutations) and for
    // ApplyAndReturn (where mutations stay on disk but the diff still needs
    // a baseline).
    let baseline_snapshot: Option<std::collections::BTreeMap<WorkspacePath, Vec<u8>>> = if matches!(
        request.patch_policy,
        PatchPolicy::ReturnOnly | PatchPolicy::ApplyAndReturn
    )
        && workspace.filesystem_root().is_some()
    {
        Some(snapshot_workspace(&*workspace).await)
    } else {
        None
    };

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
        let plan_result = execute_plan(runtime.clone(), plan_req).await?;
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
        let plan_result = execute_plan(runtime.clone(), plan_req).await?;
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
        return execute_act(runtime.clone(), act_req).await;
    }

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
        if let Ok(p) = workspace_for_run.diff(diff_base).await {
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
            // ReturnOnly + filesystem: roll back any disk mutations to the
            // baseline snapshot we took before the agent ran.
            if matches!(request.patch_policy, PatchPolicy::ReturnOnly) {
                if let Some(snap) = &baseline_snapshot {
                    restore_workspace(&*workspace, snap).await;
                }
            }
            patch = Some(p);
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
async fn snapshot_workspace(
    workspace: &dyn Workspace,
) -> std::collections::BTreeMap<WorkspacePath, Vec<u8>> {
    let mut out = std::collections::BTreeMap::new();
    let files = match workspace
        .list_files(crate::workspace::FileQuery::new())
        .await
    {
        Ok(f) => f,
        Err(_) => return out,
    };
    for path in files {
        if let Ok(content) = workspace.read_file(&path).await {
            out.insert(path, content.bytes);
        }
    }
    out
}

/// Restore `workspace` to match `snapshot`. Used after a `ReturnOnly` run
/// to roll back any mutations the model performed through filesystem
/// tools.
async fn restore_workspace(
    workspace: &dyn Workspace,
    snapshot: &std::collections::BTreeMap<WorkspacePath, Vec<u8>>,
) {
    let current = match workspace
        .list_files(crate::workspace::FileQuery::new())
        .await
    {
        Ok(f) => f,
        Err(_) => return,
    };
    for path in &current {
        if !snapshot.contains_key(path) {
            // File was created during the run; delete it.
            let _ = workspace.delete_file(path).await;
        }
    }
    for (path, bytes) in snapshot {
        // Restore original content (or recreate deleted file).
        let _ = workspace.write_file(path, bytes).await;
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
