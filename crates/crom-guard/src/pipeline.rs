//! The enforcement pipeline (MILESTONE_18 §10.2, #351). Every protected
//! call goes through [`Guard::call`]; no path executes on a non-`Allow`
//! decision, and every external component (store, resolver, operation, PDP,
//! broker, audit sink) is called under `catch_unwind`, so a panic is a deny
//! or a failed outcome, never an effect.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::action::{
    ApprovalContext, AuthorizationContext, AuthorizationRequest, AuthorizedAction, ValidatedArgs,
};
use crate::audit::{
    AuditIdentity, AuditReceipt, AuditRecord, AuditSink, DecisionKind, Enforcement, Outcome,
    RecordKind, UntrustedCorrelation,
};
use crate::context::ExecutionContext;
use crate::credential::CredentialBroker;
use crate::decision::{ActionDigest, ApprovalRequirement, Decision, DenyReason};
use crate::handle::SessionHandle;
use crate::ids::{ApprovalId, DecisionId};
use crate::registry::{OpKind, OperationRegistry, Registered};
use crate::resolution::{race, ResolveTimer};
use crate::resource::ResolvedResource;
use crate::time::{Clock, Timestamp};
use crate::traits::{
    BoxFuture, ExecuteError, ExecutionOutput, GrantStore, PolicyDecisionPoint, ResolveError, Sealed,
};

/// Longest agent-supplied correlation string kept in audit; longer values
/// are truncated.
pub const MAX_CORRELATION_LEN: usize = 256;
/// Largest serialized `client_info` kept in audit; larger values are dropped.
pub const MAX_CLIENT_INFO_BYTES: usize = 4096;

/// Agent-supplied request metadata (e.g. MCP `_meta`). Untrusted: recorded
/// apart from trusted audit fields and never used for a decision, except
/// that a presented `approval_id` is looked up by the [`ApprovalVerifier`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallMeta {
    pub approval_id: Option<String>,
    pub correlation: UntrustedCorrelation,
}

/// What the agent sees (MILESTONE_18 §10.2 step 8). Codes are stable and
/// carry no internal detail: no ancestor grant id, no downstream error
/// text, and the same `resource_not_found` for missing and other-tenant
/// resources.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[non_exhaustive]
pub struct CallOutcome {
    pub decision_id: DecisionId,
    pub result: CallResult,
    /// The last audit record durably written for this call, as an anchor
    /// (MILESTONE_18 §12.2). `None` if nothing could be written.
    pub audit_head: Option<AuditReceipt>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
#[non_exhaustive]
pub enum CallResult {
    /// The operation ran; `content` is its structured result.
    Executed { content: Value },
    /// Nothing ran. `code` is a [`DenyReason::code`].
    Denied { code: &'static str },
    /// Nothing ran. A new request carrying an approval that matches would
    /// be evaluated again (MILESTONE_18 §9.4).
    ApprovalRequired {
        role: String,
        action_digest: ActionDigest,
    },
    /// The executor was invoked and reported failure: `precondition_failed`
    /// (the resource changed after authorization; the downstream performed
    /// no effect) or `execution_failed`.
    Failed { code: &'static str },
}

impl CallOutcome {
    pub fn is_error(&self) -> bool {
        !matches!(self.result, CallResult::Executed { .. })
    }

    /// The stable code for a non-executed result.
    pub fn code(&self) -> Option<&'static str> {
        match &self.result {
            CallResult::Executed { .. } => None,
            CallResult::Denied { code } | CallResult::Failed { code } => Some(code),
            CallResult::ApprovalRequired { .. } => Some("approval_required"),
        }
    }
}

pub const PRECONDITION_FAILED: &str = "precondition_failed";
pub const EXECUTION_FAILED: &str = "execution_failed";

/// Matches a presented approval to the request. Sealed: only this crate
/// builds an [`ApprovalContext`]. `verify` must check that the stored
/// approval is for exactly `digest` (which binds the leaf grant of `ctx`)
/// and `resource`'s version, that its approver is in the principal's
/// tenant, that it is unexpired at `now` and unused, and must mark it used
/// atomically; any failure is `ApprovalInvalid`. Implemented by
/// [`MemoryApprovalStore`](crate::MemoryApprovalStore).
pub trait ApprovalVerifier: Sealed + Send + Sync {
    fn verify<'a>(
        &'a self,
        approval_id: &'a ApprovalId,
        ctx: &'a ExecutionContext,
        digest: &'a ActionDigest,
        resource: &'a ResolvedResource,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ApprovalContext, DenyReason>>;

    /// Called after an `ApprovalRequired` decision record is written, so
    /// the store can keep what is to be approved. The default keeps nothing.
    fn record_pending(
        &self,
        _request: &AuthorizationRequest<'_>,
        _requirement: &ApprovalRequirement,
        _now: Timestamp,
    ) {
    }
}

/// The default: every presented approval is `ApprovalInvalid`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoApprovals;

impl Sealed for NoApprovals {}

impl ApprovalVerifier for NoApprovals {
    fn verify<'a>(
        &'a self,
        _: &'a ApprovalId,
        _: &'a ExecutionContext,
        _: &'a ActionDigest,
        _: &'a ResolvedResource,
        _: Timestamp,
    ) -> BoxFuture<'a, Result<ApprovalContext, DenyReason>> {
        Box::pin(async { Err(DenyReason::ApprovalInvalid) })
    }
}

/// The in-process enforcement point (`crom_guard::Guard`, MILESTONE_18
/// §10.3). For trusted Rust hosts and for `crom-guard-server` (#357).
///
/// Runtime-agnostic: it awaits only the components' futures. `now` always
/// comes from the injected [`Clock`], never from the request.
pub struct Guard {
    registry: OperationRegistry,
    store: Arc<dyn GrantStore>,
    pdp: Arc<dyn PolicyDecisionPoint>,
    broker: Arc<dyn CredentialBroker>,
    audit: Arc<dyn AuditSink>,
    clock: Arc<dyn Clock>,
    approvals: Arc<dyn ApprovalVerifier>,
    resolve_timer: Option<ResolveTimer>,
    /// Set when an allowed call may have had an unrecorded effect: its
    /// outcome record could not be written, or the call was dropped after
    /// its decision record (MILESTONE_18 §12.3). Every later call is denied
    /// until restart.
    failed: Arc<AtomicBool>,
}

impl std::fmt::Debug for Guard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guard")
            .field("registry", &self.registry)
            .field("failed", &self.failed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl Guard {
    /// Build the store's `GrantStoreConfig` from `registry.action_ids()`.
    pub fn new(
        registry: OperationRegistry,
        store: Arc<dyn GrantStore>,
        pdp: Arc<dyn PolicyDecisionPoint>,
        broker: Arc<dyn CredentialBroker>,
        audit: Arc<dyn AuditSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            registry,
            store,
            pdp,
            broker,
            audit,
            clock,
            approvals: Arc::new(NoApprovals),
            resolve_timer: None,
            failed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn with_approval_verifier(mut self, approvals: Arc<dyn ApprovalVerifier>) -> Self {
        self.approvals = approvals;
        self
    }

    /// Bounds every resolution, in addition to any `TimeoutResolver` an
    /// operation uses. A timeout is `engine_unavailable`.
    pub fn with_resolve_timer(
        mut self,
        timer: impl Fn() -> BoxFuture<'static, ()> + Send + Sync + 'static,
    ) -> Self {
        self.resolve_timer = Some(Box::new(timer));
        self
    }

    pub fn registry(&self) -> &OperationRegistry {
        &self.registry
    }

    /// True after an allowed call may have had an unrecorded effect (see
    /// `docs/guard/pipeline.md`); Guard then denies every call with
    /// `engine_unavailable` until restarted.
    pub fn is_failed_closed(&self) -> bool {
        self.failed.load(Ordering::SeqCst)
    }

    /// Runs one `tools/call` through MILESTONE_18 §10.2 steps 1–8.
    /// `bearer` is the presented session handle (empty if none).
    pub fn call<'a>(
        &'a self,
        bearer: &'a str,
        tool: &'a str,
        arguments: Value,
        meta: CallMeta,
    ) -> BoxFuture<'a, CallOutcome> {
        Box::pin(async move {
            let decision_id = new_decision_id();
            let outcome = Call {
                guard: self,
                rec: AuditRecord::new(RecordKind::Decision, decision_id.clone(), self.clock.now()),
                head: None,
                latch: None,
            }
            .run(bearer, tool, arguments, meta)
            .await;
            tracing::debug!(
                decision_id = decision_id.as_str(),
                code = outcome.code().unwrap_or("executed"),
                "guard call"
            );
            outcome
        })
    }
}

/// Per-call state: the decision record being filled and the audit head.
struct Call<'g> {
    guard: &'g Guard,
    rec: AuditRecord,
    head: Option<AuditReceipt>,
    /// Armed from the `Allow` decision record until the outcome record has
    /// been attempted.
    latch: Option<CancelLatch>,
}

/// Fails Guard closed if dropped while armed: the call was cancelled after
/// its `Allow` decision record, so an effect may have happened with no
/// outcome record.
struct CancelLatch {
    failed: Arc<AtomicBool>,
    decision_id: DecisionId,
    armed: bool,
}

impl Drop for CancelLatch {
    fn drop(&mut self) {
        if self.armed {
            fail_closed(
                &self.failed,
                &self.decision_id,
                "call dropped after its decision record, before its outcome record",
            );
        }
    }
}

/// Sets the latch and reports it to `tracing` and stderr (§12.3).
fn fail_closed(failed: &AtomicBool, decision_id: &DecisionId, why: &str) {
    failed.store(true, Ordering::SeqCst);
    tracing::error!(
        decision_id = decision_id.as_str(),
        "{why}; Guard is failed closed until restart"
    );
    eprintln!(
        "crom-guard: {why} (decision {}); failed closed until restart",
        decision_id.as_str()
    );
}

/// What the agent sees for `reason`. Audit unavailability is reported as
/// `engine_unavailable` (§12.3), so agents cannot tell the log is down.
fn agent_code(reason: &DenyReason) -> &'static str {
    match reason {
        DenyReason::AuditUnavailable => DenyReason::EngineUnavailable.code(),
        r => r.code(),
    }
}

/// The stage that stopped a call, for the record's `enforcement`.
fn enforcement_for(reason: &DenyReason) -> Enforcement {
    match reason {
        DenyReason::NoGrant
        | DenyReason::GrantNotYetValid
        | DenyReason::GrantExpired
        | DenyReason::GrantRevoked => Enforcement::GuardSession,
        DenyReason::ChainDeny { .. } => Enforcement::GuardChain,
        DenyReason::ApprovalInvalid => Enforcement::GuardApproval,
        DenyReason::UnknownAction
        | DenyReason::ActionNotGranted
        | DenyReason::InvalidArguments
        | DenyReason::ResourceNotFound => Enforcement::GuardValidation,
        _ => Enforcement::GuardPolicy,
    }
}

impl Call<'_> {
    async fn run(
        mut self,
        bearer: &str,
        tool: &str,
        arguments: Value,
        meta: CallMeta,
    ) -> CallOutcome {
        let g = self.guard;
        self.rec.untrusted = sanitize(meta.correlation);
        if g.is_failed_closed() {
            // The log cannot be trusted to record this call; refuse it.
            return self.respond(CallResult::Denied {
                code: agent_code(&DenyReason::AuditUnavailable),
            });
        }
        let now = self.rec.time;

        // 1. Authenticate.
        let Ok(handle) = SessionHandle::parse(bearer) else {
            return self.deny(DenyReason::NoGrant).await;
        };
        let ctx = match guarded(|| g.store.authenticate(&handle, now)).await {
            Ok(Ok(ctx)) => ctx,
            Ok(Err(reason)) => return self.deny(reason).await,
            Err(Panicked) => return self.panicked("authenticate").await,
        };
        drop(handle);
        self.rec.identity = Some(AuditIdentity::from(ctx.principal()));

        // 2. Normalize and validate.
        let Some(entry) = g.registry.get(tool) else {
            self.rec.untrusted.requested_tool = Some(truncate(tool));
            return self.deny(DenyReason::UnknownAction).await;
        };
        self.rec.action = Some(entry.action.clone());
        let Some(args) = entry.validate(arguments) else {
            return self.deny(DenyReason::InvalidArguments).await;
        };
        self.rec.args_digest = Some(crate::pdp::args_digest_hex(&args));
        self.rec.args_logged = logged(entry, &args);
        if !ctx
            .chain()
            .iter()
            .all(|gr| gr.actions().contains(&entry.action))
        {
            return self.deny(DenyReason::ActionNotGranted).await;
        }
        let op = match &entry.kind {
            OpKind::Protected(op) => op.clone(),
            OpKind::Builtin(_) => return self.run_builtin(entry, ctx, args).await,
        };
        let context: AuthorizationContext = match guarded_sync(|| op.context(&args)) {
            Ok(Ok(c)) => c,
            Ok(Err(reason)) => return self.deny(reason).await,
            Err(Panicked) => return self.panicked("context").await,
        };

        // 3. Resolve.
        let t = Instant::now();
        let resolved = guarded(|| {
            let fut = op.resolver().resolve(ctx.principal().tenant(), &args);
            match &g.resolve_timer {
                Some(timer) => Box::pin(race(fut, timer())) as BoxFuture<'_, _>,
                None => fut,
            }
        })
        .await;
        self.rec.duration_us.resolve_us = Some(micros(t));
        let resource = match resolved {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return self.deny(DenyReason::from(e)).await,
            Err(Panicked) => {
                return self
                    .panicked_as("resolver", DenyReason::from(ResolveError::Unavailable))
                    .await
            }
        };
        self.rec.resource = Some(resource.uid().clone());

        // Presented approval (#354): digest-bound to this exact request.
        let approval = match meta.approval_id {
            None => None,
            Some(raw) => {
                let Ok(id) = ApprovalId::new(raw) else {
                    return self.deny(DenyReason::ApprovalInvalid).await;
                };
                let digest = ActionDigest::compute(
                    ctx.principal().grant_id(),
                    &entry.action,
                    resource.uid(),
                    &args,
                );
                // Fresh: expiry is judged after resolution, not at call start.
                let now = g.clock.now();
                match guarded(|| g.approvals.verify(&id, &ctx, &digest, &resource, now)).await {
                    Ok(Ok(a)) => Some(a),
                    Ok(Err(_)) | Err(Panicked) => {
                        return self.deny(DenyReason::ApprovalInvalid).await
                    }
                }
            }
        };
        self.rec.approval_id = approval.as_ref().map(|a| a.approval_id().clone());

        // 4. Authorize.
        let request = AuthorizationRequest::new(
            &ctx,
            &entry.action,
            &resource,
            &args,
            &context,
            approval.as_ref(),
        );
        let t = Instant::now();
        let decision = guarded_sync(|| g.pdp.authorize(&request))
            .unwrap_or_else(|Panicked| Decision::deny(DenyReason::PolicyError));
        self.rec.duration_us.policy_us = Some(micros(t));
        self.rec.policy_set_hash = Some(g.pdp.policy_set_hash().to_owned());
        self.rec.schema_hash = Some(g.pdp.schema_hash().to_owned());

        // 5. Bind. Only `Allow` yields an `AuthorizedAction`.
        let authorized = AuthorizedAction::bind(self.rec.decision_id.clone(), &decision, &request);
        let authorized = match (decision, authorized) {
            (
                Decision::Allow {
                    determining_policies,
                },
                Some(a),
            ) => {
                self.rec.determining_policies = determining_policies;
                a
            }
            (Decision::ApprovalRequired { requirement }, _) => {
                self.rec.decision = Some(DecisionKind::ApprovalRequired);
                self.rec.enforcement = Some(Enforcement::GuardApproval);
                self.rec.outcome = Some(Outcome::Blocked);
                if let Err(r) = self.write_decision().await {
                    return *r;
                }
                let now = g.clock.now();
                if guarded_sync(|| g.approvals.record_pending(&request, &requirement, now)).is_err()
                {
                    tracing::warn!(stage = "record_pending", "approval store panicked");
                }
                return self.respond(CallResult::ApprovalRequired {
                    role: requirement.role,
                    action_digest: requirement.action_digest,
                });
            }
            (
                Decision::Deny {
                    reason,
                    determining_policies,
                },
                _,
            ) => {
                self.rec.determining_policies = determining_policies;
                return self.deny(reason).await;
            }
            // `Allow` without a binding, or a decision kind this pipeline
            // does not know: fail closed.
            _ => return self.deny(DenyReason::PolicyError).await,
        };

        // 7a. Durable decision record before anything executes.
        self.rec.decision = Some(DecisionKind::Allow);
        self.rec.enforcement = Some(Enforcement::GuardPolicy);
        if let Err(r) = self.write_decision().await {
            return *r;
        }
        self.arm();

        // 6. Lease, recheck immediately before execution, execute.
        if g.is_failed_closed() {
            return self
                .blocked_after_decision(DenyReason::AuditUnavailable)
                .await;
        }
        let lease = match guarded(|| g.broker.lease(&entry.action)).await {
            Ok(Ok(l)) => l,
            Ok(Err(_)) | Err(Panicked) => {
                return self
                    .blocked_after_decision(DenyReason::EngineUnavailable)
                    .await
            }
        };
        if let Err(reason) = self.recheck(&ctx).await {
            return self.blocked_after_decision(reason).await;
        }
        if g.is_failed_closed() {
            return self
                .blocked_after_decision(DenyReason::AuditUnavailable)
                .await;
        }
        let t = Instant::now();
        let result = guarded(|| op.execute(authorized, lease)).await;
        let execute_us = micros(t);
        self.finish(result, execute_us).await
    }

    /// Builtins (#353): no resolver and no Cedar evaluation. The builtin's
    /// own `check` decides; the decision record is still written first and
    /// the chain rechecked.
    async fn run_builtin(
        mut self,
        entry: &Registered,
        ctx: ExecutionContext,
        args: ValidatedArgs,
    ) -> CallOutcome {
        let OpKind::Builtin(op) = &entry.kind else {
            return self.deny(DenyReason::PolicyError).await;
        };
        let now = self.rec.time;
        match guarded_sync(|| op.check(&ctx, &args, now)) {
            Ok(Ok(())) => {}
            Ok(Err(reason)) => return self.deny(reason).await,
            Err(Panicked) => return self.panicked("builtin check").await,
        }
        self.rec.decision = Some(DecisionKind::Allow);
        self.rec.enforcement = Some(Enforcement::GuardValidation);
        if let Err(r) = self.write_decision().await {
            return *r;
        }
        self.arm();
        if let Err(reason) = self.recheck(&ctx).await {
            return self.blocked_after_decision(reason).await;
        }
        let g = self.guard;
        if g.is_failed_closed() {
            return self
                .blocked_after_decision(DenyReason::AuditUnavailable)
                .await;
        }
        let now = g.clock.now();
        let t = Instant::now();
        let result = guarded(|| op.run(&ctx, g.store.as_ref(), &args, now)).await;
        let execute_us = micros(t);
        self.finish(result, execute_us).await
    }

    fn arm(&mut self) {
        self.latch = Some(CancelLatch {
            failed: self.guard.failed.clone(),
            decision_id: self.rec.decision_id.clone(),
            armed: true,
        });
    }

    fn disarm(&mut self) {
        if let Some(mut l) = self.latch.take() {
            l.armed = false;
        }
    }

    /// Re-reads expiry and revocation on a fresh clock reading
    /// (MILESTONE_18 §10.2 step 6).
    async fn recheck(&self, ctx: &ExecutionContext) -> Result<(), DenyReason> {
        let g = self.guard;
        match guarded(|| g.store.recheck(ctx, g.clock.now())).await {
            Ok(r) => r,
            Err(Panicked) => Err(DenyReason::EngineUnavailable),
        }
    }

    async fn finish(
        mut self,
        result: Result<Result<ExecutionOutput, ExecuteError>, Panicked>,
        execute_us: u64,
    ) -> CallOutcome {
        let mut out = self.outcome_record();
        out.duration_us.execute_us = Some(execute_us);
        let response = match result {
            Ok(Ok(output)) => {
                out.outcome = Some(Outcome::Executed);
                out.downstream_ref = output.downstream_ref;
                CallResult::Executed {
                    content: output.content,
                }
            }
            Ok(Err(ExecuteError::Precondition)) => {
                out.outcome = Some(Outcome::Failed);
                out.enforcement = Some(Enforcement::ExecutorPrecondition);
                CallResult::Failed {
                    code: PRECONDITION_FAILED,
                }
            }
            Ok(Err(ExecuteError::Failed(detail))) => {
                // The text is never emitted: an executor may have put
                // downstream data in it. Length and digest prefix only.
                let digest = hex::encode(Sha256::digest(detail.as_bytes()));
                tracing::debug!(
                    decision_id = out.decision_id.as_str(),
                    detail_len = detail.len(),
                    detail_sha256 = &digest[..16],
                    "protected operation failed"
                );
                out.outcome = Some(Outcome::Failed);
                CallResult::Failed {
                    code: EXECUTION_FAILED,
                }
            }
            Err(Panicked) => {
                tracing::warn!(
                    decision_id = out.decision_id.as_str(),
                    "protected operation panicked during execute"
                );
                out.outcome = Some(Outcome::Failed);
                CallResult::Failed {
                    code: EXECUTION_FAILED,
                }
            }
        };
        if self.append(&out).await.is_err() {
            // The effect may have happened and is unrecorded: stop serving.
            fail_closed(
                &self.guard.failed,
                &out.decision_id,
                "audit outcome record could not be written",
            );
        }
        self.disarm();
        self.respond(response)
    }

    /// The decision record was `Allow`, but nothing executed.
    async fn blocked_after_decision(mut self, reason: DenyReason) -> CallOutcome {
        let mut out = self.outcome_record();
        out.outcome = Some(Outcome::Blocked);
        out.enforcement = Some(enforcement_for(&reason));
        let code = agent_code(&reason);
        out.reason = Some(reason);
        if self.append(&out).await.is_err() {
            fail_closed(
                &self.guard.failed,
                &out.decision_id,
                "audit outcome record could not be written",
            );
        }
        self.disarm();
        self.respond(CallResult::Denied { code })
    }

    fn outcome_record(&self) -> AuditRecord {
        let mut out = self.rec.clone();
        out.record = RecordKind::Outcome;
        out.time = self.guard.clock.now();
        out.outcome = None;
        out.reason = None;
        out
    }

    async fn deny(mut self, reason: DenyReason) -> CallOutcome {
        self.rec.decision = Some(DecisionKind::Deny);
        self.rec.enforcement = Some(enforcement_for(&reason));
        self.rec.outcome = Some(Outcome::Blocked);
        let code = agent_code(&reason);
        self.rec.reason = Some(reason);
        if let Err(r) = self.write_decision().await {
            return *r;
        }
        self.respond(CallResult::Denied { code })
    }

    async fn panicked(self, stage: &'static str) -> CallOutcome {
        self.panicked_as(stage, DenyReason::EngineUnavailable).await
    }

    async fn panicked_as(self, stage: &'static str, reason: DenyReason) -> CallOutcome {
        tracing::warn!(
            decision_id = self.rec.decision_id.as_str(),
            stage,
            "guard component panicked; denying"
        );
        self.deny(reason).await
    }

    /// Writes the decision record. `Err` carries the answer (internally
    /// `audit_unavailable`, shown as `engine_unavailable`): nothing may
    /// execute.
    async fn write_decision(&mut self) -> Result<(), Box<CallOutcome>> {
        let rec = self.rec.clone();
        if self.append(&rec).await.is_ok() {
            return Ok(());
        }
        tracing::error!(
            decision_id = rec.decision_id.as_str(),
            reason = DenyReason::AuditUnavailable.code(),
            "audit decision record could not be written; denying"
        );
        Err(Box::new(CallOutcome {
            decision_id: rec.decision_id,
            result: CallResult::Denied {
                code: agent_code(&DenyReason::AuditUnavailable),
            },
            audit_head: self.head.clone(),
        }))
    }

    async fn append(&mut self, rec: &AuditRecord) -> Result<(), ()> {
        match guarded(|| self.guard.audit.append(rec)).await {
            Ok(Ok(receipt)) => {
                self.head = Some(receipt);
                Ok(())
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "audit append failed");
                Err(())
            }
            Err(Panicked) => Err(()),
        }
    }

    fn respond(self, result: CallResult) -> CallOutcome {
        CallOutcome {
            decision_id: self.rec.decision_id,
            result,
            audit_head: self.head,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Panicked;

fn guarded_sync<T>(f: impl FnOnce() -> T) -> Result<T, Panicked> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| Panicked)
}

/// A component future that is always dropped under `catch_unwind`, also
/// when the call itself is cancelled.
struct Slot<'a, T>(Option<BoxFuture<'a, T>>);

impl<T> Slot<'_, T> {
    /// Drops the future; true if its `Drop` panicked.
    fn clear(&mut self) -> bool {
        let f = self.0.take();
        catch_unwind(AssertUnwindSafe(move || drop(f))).is_err()
    }
}

impl<T> Drop for Slot<'_, T> {
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

/// Builds, polls, and drops a component future, turning a panic in any of
/// them into `Panicked`. A future that completes and then panics in `Drop`
/// is `Panicked` too: for an executor, a failed outcome.
async fn guarded<'a, T>(make: impl FnOnce() -> BoxFuture<'a, T>) -> Result<T, Panicked> {
    let mut slot = Slot(Some(guarded_sync(make)?));
    std::future::poll_fn(move |cx| {
        let Some(fut) = slot.0.as_mut() else {
            return Poll::Ready(Err(Panicked));
        };
        match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => {
                if slot.clear() {
                    Poll::Ready(Err(Panicked))
                } else {
                    Poll::Ready(Ok(v))
                }
            }
            Err(_) => {
                slot.clear();
                Poll::Ready(Err(Panicked))
            }
        }
    })
    .await
}

fn micros(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// `dec-` plus 128 random bits. If the OS RNG fails, a process-unique
/// fallback keeps records distinct; ids are correlation keys, not secrets.
fn new_decision_id() -> DecisionId {
    use std::sync::atomic::AtomicU64;
    static FALLBACK: AtomicU64 = AtomicU64::new(0);
    let mut b = [0u8; 16];
    let s = match getrandom::fill(&mut b) {
        Ok(()) => format!("dec-{}", hex::encode(b)),
        Err(_) => format!(
            "dec-local-{}-{}",
            std::process::id(),
            FALLBACK.fetch_add(1, Ordering::Relaxed)
        ),
    };
    DecisionId::new(s).expect("decision ids use the id charset")
}

fn truncate(s: &str) -> String {
    let mut end = s.len().min(MAX_CORRELATION_LEN);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Bounds agent-supplied correlation values; `requested_tool` is set by
/// the pipeline, not the agent.
fn sanitize(c: UntrustedCorrelation) -> UntrustedCorrelation {
    UntrustedCorrelation {
        tool_use_id: c.tool_use_id.as_deref().map(truncate),
        client_request_id: c.client_request_id.as_deref().map(truncate),
        requested_tool: None,
        client_info: c
            .client_info
            .filter(|v| serde_json::to_vec(v).is_ok_and(|b| b.len() <= MAX_CLIENT_INFO_BYTES)),
    }
}

/// Only the operation's allowlisted top-level fields.
fn logged(entry: &Registered, args: &ValidatedArgs) -> serde_json::Map<String, Value> {
    let Some(obj) = args.as_json().as_object() else {
        return serde_json::Map::new();
    };
    entry
        .logged_fields()
        .iter()
        .filter_map(|f| obj.get(*f).map(|v| ((*f).to_owned(), v.clone())))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    use futures::executor::block_on;
    use serde_json::json;

    use super::*;
    use crate::audit::AuditError;
    use crate::credential::{BrokerError, CredentialLease};
    use crate::grant::tests::spec;
    use crate::ids::{ActionId, AppId};
    use crate::pdp::CedarPdp;
    use crate::registry::BuiltinOperation;
    use crate::store::{GrantStoreConfig, MemoryGrantStore};

    const SCHEMA: &str =
        include_str!("../../../examples/guard-support/policies/support.cedarschema");
    const POLICIES: &str = include_str!("../../../examples/guard-support/policies/support.cedar");

    struct Echo {
        action: ActionId,
        schema: Value,
        runs: AtomicUsize,
    }

    impl BuiltinOperation for Echo {
        fn action(&self) -> &ActionId {
            &self.action
        }
        fn description(&self) -> &str {
            "echo"
        }
        fn input_schema(&self) -> &Value {
            &self.schema
        }
        fn check(
            &self,
            _: &ExecutionContext,
            args: &ValidatedArgs,
            _: Timestamp,
        ) -> Result<(), DenyReason> {
            match args.as_json().get("check").and_then(Value::as_str) {
                Some("deny") => Err(DenyReason::PolicyDeny),
                Some("panic") => panic!("builtin check bug"),
                _ => Ok(()),
            }
        }
        fn run<'a>(
            &'a self,
            ctx: &'a ExecutionContext,
            _store: &'a dyn GrantStore,
            _args: &'a ValidatedArgs,
            _now: Timestamp,
        ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>> {
            Box::pin(async move {
                self.runs.fetch_add(1, Ordering::SeqCst);
                Ok(ExecutionOutput {
                    content: json!({"grant": ctx.principal().grant_id().as_str()}),
                    downstream_ref: None,
                })
            })
        }
    }

    #[derive(Default)]
    struct Sink(Mutex<Vec<AuditRecord>>);

    impl AuditSink for Sink {
        fn append<'a>(
            &'a self,
            r: &'a AuditRecord,
        ) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
            Box::pin(async move {
                let mut v = self.0.lock().unwrap();
                v.push(r.clone());
                Ok(AuditReceipt {
                    seq: v.len() as u64,
                    head_hash: String::new(),
                })
            })
        }
    }

    struct NoBroker;

    impl CredentialBroker for NoBroker {
        fn lease<'a>(
            &'a self,
            _: &'a ActionId,
        ) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
            Box::pin(async { Err(BrokerError::Unavailable) })
        }
    }

    struct Fixed;

    impl Clock for Fixed {
        fn now(&self) -> Timestamp {
            Timestamp::from_unix_millis(5_000)
        }
    }

    fn echo(action: &str) -> Arc<Echo> {
        Arc::new(Echo {
            action: ActionId::new(action).unwrap(),
            schema: json!({"type": "object", "additionalProperties": false,
                           "properties": {"check": {"type": "string"}}}),
            runs: AtomicUsize::new(0),
        })
    }

    fn setup(grant_echo: bool) -> (Guard, Arc<Echo>, Arc<Sink>, String) {
        let op = echo("guard.echo");
        let mut reg = OperationRegistry::new();
        reg.register_builtin(op.clone()).unwrap();
        let mut actions = reg.action_ids();
        actions.insert(ActionId::new("customer.read").unwrap());
        let store = Arc::new(MemoryGrantStore::new(GrantStoreConfig::new(actions)));
        let mut s = spec();
        if grant_echo {
            s.actions.insert(op.action.clone());
        }
        let app = AppId::new("support-app").unwrap();
        let issued = block_on(store.issue(&app, s, Fixed.now())).unwrap();
        let sink = Arc::new(Sink::default());
        let guard = Guard::new(
            reg,
            store,
            Arc::new(CedarPdp::from_sources("Support", SCHEMA, POLICIES).unwrap()),
            Arc::new(NoBroker),
            sink.clone(),
            Arc::new(Fixed),
        );
        (guard, op, sink, issued.handle.expose_secret().to_owned())
    }

    /// A granted builtin runs once, after its decision record and before its outcome record, without a credential lease.
    #[test]
    fn builtin_runs_between_decision_and_outcome() {
        let (guard, op, sink, handle) = setup(true);
        let out = block_on(guard.call(&handle, "guard.echo", json!({}), CallMeta::default()));
        assert!(!out.is_error(), "{out:?}");
        assert_eq!(op.runs.load(Ordering::SeqCst), 1);
        let recs = sink.0.lock().unwrap();
        assert_eq!(recs.len(), 2);
        assert_eq!(recs[0].record, RecordKind::Decision);
        assert_eq!(recs[1].outcome, Some(Outcome::Executed));
    }

    /// A builtin whose `check` denies or panics writes one deny decision record and never runs.
    #[test]
    fn builtin_check_decides_before_the_decision_record() {
        for (mode, code, reason) in [
            ("deny", "policy_deny", DenyReason::PolicyDeny),
            ("panic", "engine_unavailable", DenyReason::EngineUnavailable),
        ] {
            let (guard, op, sink, handle) = setup(true);
            let out = block_on(guard.call(
                &handle,
                "guard.echo",
                json!({"check": mode}),
                CallMeta::default(),
            ));
            assert_eq!(out.code(), Some(code), "{mode}");
            assert_eq!(op.runs.load(Ordering::SeqCst), 0, "{mode}");
            let recs = sink.0.lock().unwrap();
            assert_eq!(recs.len(), 1, "{mode}");
            assert_eq!(recs[0].decision, Some(DecisionKind::Deny), "{mode}");
            assert_eq!(recs[0].reason, Some(reason), "{mode}");
            assert!(!guard.is_failed_closed());
        }
    }

    /// Latches Guard when it records a decision, as a concurrent failure would.
    struct LatchingSink(Arc<AtomicBool>);

    impl AuditSink for LatchingSink {
        fn append<'a>(
            &'a self,
            r: &'a AuditRecord,
        ) -> BoxFuture<'a, Result<AuditReceipt, AuditError>> {
            if r.record == RecordKind::Decision {
                self.0.store(true, Ordering::SeqCst);
            }
            Box::pin(async {
                Ok(AuditReceipt {
                    seq: 1,
                    head_hash: String::new(),
                })
            })
        }
    }

    /// A builtin does not run if Guard failed closed after its decision record.
    #[test]
    fn builtin_stops_if_latched_after_decision() {
        let (mut guard, op, _sink, handle) = setup(true);
        guard.audit = Arc::new(LatchingSink(guard.failed.clone()));
        let out = block_on(guard.call(&handle, "guard.echo", json!({}), CallMeta::default()));
        assert_eq!(out.code(), Some("engine_unavailable"));
        assert_eq!(op.runs.load(Ordering::SeqCst), 0);
    }

    /// A builtin the grant does not list is `action_not_granted` and does not run, and builtins must use the reserved prefix.
    #[test]
    fn builtin_requires_grant_and_reserved_prefix() {
        let (guard, op, _sink, handle) = setup(false);
        let out = block_on(guard.call(&handle, "guard.echo", json!({}), CallMeta::default()));
        assert_eq!(out.code(), Some("action_not_granted"));
        assert_eq!(op.runs.load(Ordering::SeqCst), 0);
        let mut reg = OperationRegistry::new();
        assert!(reg.register_builtin(echo("echo")).is_err());
    }
}
