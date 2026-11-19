//! Approvals for `ApprovalRequired` decisions (MILESTONE_18 §9.4, #354).
//!
//! Only the trusted application's admin path calls [`MemoryApprovalStore::approve`];
//! no agent-facing operation reaches it. What an approval covers (version,
//! fields) is copied from the pending request Guard recorded when it
//! returned `ApprovalRequired`, never from the admin call. The approval id
//! is an identifier, not a credential: it authorizes nothing without the
//! leaf grant's session handle and the exact digested action, so it may
//! appear in audit.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::action::{ApprovalContext, AuthorizationRequest};
use crate::context::ExecutionContext;
use crate::decision::{ActionDigest, ApprovalRequirement, DenyReason};
use crate::grant::Delegator;
use crate::ids::{ActionId, AppId, ApprovalId, GrantId, TenantId};
use crate::pipeline::ApprovalVerifier;
use crate::resource::{ResolvedResource, ResourceVersion};
use crate::time::Timestamp;
use crate::traits::{BoxFuture, Sealed};
use crate::value::{Attrs, EntityUid};

/// Default and upper bound for [`ApprovalStoreConfig::max_ttl_seconds`].
pub const DEFAULT_APPROVAL_MAX_TTL_SECONDS: u64 = 900;
/// Default for [`ApprovalStoreConfig::pending_ttl_seconds`].
pub const DEFAULT_PENDING_TTL_SECONDS: u64 = 900;
/// Default for [`ApprovalStoreConfig::max_approvals`].
pub const DEFAULT_MAX_APPROVALS: usize = 10_000;
/// Default for both per-issuer quotas in [`ApprovalStoreConfig`].
pub const DEFAULT_MAX_PER_ISSUER: usize = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStoreConfig {
    pub max_ttl_seconds: u64,
    /// How long a pending request can be approved after Guard records it.
    pub pending_ttl_seconds: u64,
    /// Total cap, applied separately to approvals and pending requests.
    pub max_approvals: usize,
    pub max_approvals_per_issuer: usize,
    pub max_pending_per_issuer: usize,
}

impl Default for ApprovalStoreConfig {
    fn default() -> Self {
        Self {
            max_ttl_seconds: DEFAULT_APPROVAL_MAX_TTL_SECONDS,
            pending_ttl_seconds: DEFAULT_PENDING_TTL_SECONDS,
            max_approvals: DEFAULT_MAX_APPROVALS,
            max_approvals_per_issuer: DEFAULT_MAX_PER_ISSUER,
            max_pending_per_issuer: DEFAULT_MAX_PER_ISSUER,
        }
    }
}

/// A human's decision on a pending request. Built by the trusted
/// application from its own authenticated admin session, never from agent
/// arguments. Everything the approval covers comes from the pending
/// request for `action_digest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSpec {
    /// From `CallResult::ApprovalRequired` or [`MemoryApprovalStore::pending`].
    pub action_digest: ActionDigest,
    pub approver: Delegator,
    /// Must equal the principal's tenant at use, or the approval is
    /// rejected before Cedar sees it.
    pub approver_tenant: TenantId,
    pub ttl_seconds: u64,
}

/// What Guard evaluated when it returned `ApprovalRequired`: the exact
/// request a human is asked to approve. Contains no secrets.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub action_digest: ActionDigest,
    pub tenant: TenantId,
    pub grant_id: GrantId,
    pub action: ActionId,
    pub resource: EntityUid,
    pub resource_version: ResourceVersion,
    /// The validated (agent-supplied) arguments.
    pub args: serde_json::Value,
    /// The operation's Cedar context fields; become `context.approval.<field>`.
    pub fields: Attrs,
    /// The `@approval` role required.
    pub role: String,
    pub created_at: Timestamp,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedApproval {
    pub approval_id: ApprovalId,
    pub expires_at: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Used,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApprovalError {
    #[error("approval spec rejected: {0}")]
    InvalidSpec(String),
    /// No unexpired pending request from this issuer has the digest.
    #[error("no pending approval request matches")]
    UnknownRequest,
    #[error("approval store unavailable")]
    Unavailable,
}

struct Stored {
    issuer: AppId,
    tenant: TenantId,
    approver: Delegator,
    digest: ActionDigest,
    version: ResourceVersion,
    fields: Attrs,
    expires_at: Timestamp,
    used: bool,
}

impl Stored {
    fn status(&self, now: Timestamp) -> ApprovalStatus {
        if self.used {
            ApprovalStatus::Used
        } else if now >= self.expires_at {
            ApprovalStatus::Expired
        } else {
            ApprovalStatus::Pending
        }
    }
}

#[derive(Default)]
struct State {
    approvals: HashMap<ApprovalId, Stored>,
    pending: HashMap<(AppId, ActionDigest), PendingApproval>,
}

impl State {
    fn approvals_of(&self, issuer: &AppId) -> usize {
        self.approvals
            .values()
            .filter(|s| s.issuer == *issuer)
            .count()
    }
    fn pending_of(&self, issuer: &AppId) -> usize {
        self.pending.keys().filter(|(i, _)| i == issuer).count()
    }
}

/// In-memory, single-use, expiring approvals; the [`ApprovalVerifier`] to
/// install with `Guard::with_approval_verifier`. Lost on restart, which
/// fails closed. `verify` checks and consumes under one lock, so of
/// concurrent uses of one approval exactly one passes.
pub struct MemoryApprovalStore {
    config: ApprovalStoreConfig,
    state: Mutex<State>,
}

impl std::fmt::Debug for MemoryApprovalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryApprovalStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn random_approval_id() -> Result<ApprovalId, ApprovalError> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(|_| ApprovalError::Unavailable)?;
    ApprovalId::new(format!("ap_{}", hex::encode(b))).map_err(|_| ApprovalError::Unavailable)
}

impl MemoryApprovalStore {
    pub fn new(config: ApprovalStoreConfig) -> Self {
        Self {
            config,
            state: Mutex::new(State::default()),
        }
    }

    /// The unexpired pending request `issuer`'s grant produced for
    /// `digest`, for the admin path to show the approver.
    pub fn pending(
        &self,
        issuer: &AppId,
        digest: &ActionDigest,
        now: Timestamp,
    ) -> Option<PendingApproval> {
        let state = self.state.lock().ok()?;
        let p = state.pending.get(&(issuer.clone(), *digest))?;
        (now < p.expires_at).then(|| p.clone())
    }

    /// Approves the pending request for `spec.action_digest` and returns a
    /// random id for the agent to present as `CallMeta::approval_id`. The
    /// pending request is consumed; the approval binds its resource version
    /// and operation fields.
    pub fn approve(
        &self,
        issuer: &AppId,
        spec: ApprovalSpec,
        now: Timestamp,
    ) -> Result<IssuedApproval, ApprovalError> {
        if spec.ttl_seconds == 0 {
            return Err(ApprovalError::InvalidSpec("ttl_seconds is 0".into()));
        }
        if spec.ttl_seconds > self.config.max_ttl_seconds {
            return Err(ApprovalError::InvalidSpec(format!(
                "ttl_seconds exceeds the maximum of {}",
                self.config.max_ttl_seconds
            )));
        }
        let mut state = self.state.lock().map_err(|_| ApprovalError::Unavailable)?;
        let key = (issuer.clone(), spec.action_digest);
        match state.pending.get(&key) {
            Some(p) if now < p.expires_at => {
                if p.fields.contains_key("approver") {
                    return Err(ApprovalError::InvalidSpec(
                        "operation context has an `approver` field".into(),
                    ));
                }
            }
            _ => return Err(ApprovalError::UnknownRequest),
        }
        if state.approvals.len() >= self.config.max_approvals
            || state.approvals_of(issuer) >= self.config.max_approvals_per_issuer
        {
            state
                .approvals
                .retain(|_, s| s.status(now) == ApprovalStatus::Pending);
            if state.approvals.len() >= self.config.max_approvals
                || state.approvals_of(issuer) >= self.config.max_approvals_per_issuer
            {
                return Err(ApprovalError::Unavailable);
            }
        }
        let approval_id = loop {
            let id = random_approval_id()?;
            if !state.approvals.contains_key(&id) {
                break id;
            }
        };
        let p = state
            .pending
            .remove(&key)
            .ok_or(ApprovalError::UnknownRequest)?;
        let expires_at = now.saturating_add_secs(spec.ttl_seconds);
        state.approvals.insert(
            approval_id.clone(),
            Stored {
                issuer: issuer.clone(),
                tenant: spec.approver_tenant,
                approver: spec.approver,
                digest: p.action_digest,
                version: p.resource_version,
                fields: p.fields,
                expires_at,
                used: false,
            },
        );
        Ok(IssuedApproval {
            approval_id,
            expires_at,
        })
    }

    /// `None` if `issuer` has no approval with this id (or it was swept).
    pub fn status(
        &self,
        issuer: &AppId,
        approval_id: &ApprovalId,
        now: Timestamp,
    ) -> Option<ApprovalStatus> {
        let state = self.state.lock().ok()?;
        let s = state.approvals.get(approval_id)?;
        (s.issuer == *issuer).then(|| s.status(now))
    }

    /// Stored approvals, including used and expired ones not yet swept.
    pub fn len(&self) -> usize {
        self.state.lock().map_or(0, |s| s.approvals.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn record_pending_sync(
        &self,
        request: &AuthorizationRequest<'_>,
        requirement: &ApprovalRequirement,
        now: Timestamp,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let p = request.execution_context().principal();
        // Recomputed with the function the pipeline uses before `verify`.
        let digest = ActionDigest::compute(
            p.grant_id(),
            request.action(),
            request.resource().uid(),
            request.args(),
        );
        let key = (p.issuer().clone(), digest);
        // The first unexpired record wins, so what a human reviews cannot
        // be swapped (e.g. for a newer resource version) before approval.
        if state.pending.get(&key).is_some_and(|x| now < x.expires_at) {
            return;
        }
        let full = |s: &State| {
            s.pending.len() >= self.config.max_approvals
                || s.pending_of(&key.0) >= self.config.max_pending_per_issuer
        };
        if full(&state) {
            state.pending.retain(|_, x| now < x.expires_at);
            if full(&state) {
                tracing::warn!("approval request not recorded: pending quota reached");
                return;
            }
        }
        state.pending.insert(
            key,
            PendingApproval {
                action_digest: digest,
                tenant: p.tenant().clone(),
                grant_id: p.grant_id().clone(),
                action: request.action().clone(),
                resource: request.resource().uid().clone(),
                resource_version: request.resource().version().clone(),
                args: request.args().as_json().clone(),
                fields: request.context().fields().clone(),
                role: requirement.role.clone(),
                created_at: now,
                expires_at: now.saturating_add_secs(self.config.pending_ttl_seconds),
            },
        );
    }

    fn verify_sync(
        &self,
        approval_id: &ApprovalId,
        ctx: &ExecutionContext,
        digest: &ActionDigest,
        resource: &ResolvedResource,
        now: Timestamp,
    ) -> Result<ApprovalContext, DenyReason> {
        let mut state = self.state.lock().map_err(|_| DenyReason::ApprovalInvalid)?;
        let s = state
            .approvals
            .get_mut(approval_id)
            .ok_or(DenyReason::ApprovalInvalid)?;
        let p = ctx.principal();
        let matches = s.status(now) == ApprovalStatus::Pending
            && s.issuer == *p.issuer()
            // Before any `ApprovalContext` exists: the PDP gives the
            // approver the principal's tenant, so Cedar cannot catch this.
            && s.tenant == *p.tenant()
            && s.digest == *digest
            && s.version == *resource.version();
        if !matches {
            return Err(DenyReason::ApprovalInvalid);
        }
        s.used = true;
        Ok(ApprovalContext::new(
            approval_id.clone(),
            s.approver.clone(),
            s.tenant.clone(),
            s.fields.clone(),
        ))
    }
}

impl Sealed for MemoryApprovalStore {}

impl ApprovalVerifier for MemoryApprovalStore {
    fn verify<'a>(
        &'a self,
        approval_id: &'a ApprovalId,
        ctx: &'a ExecutionContext,
        digest: &'a ActionDigest,
        resource: &'a ResolvedResource,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ApprovalContext, DenyReason>> {
        let r = self.verify_sync(approval_id, ctx, digest, resource, now);
        Box::pin(async move { r })
    }

    fn record_pending(
        &self,
        request: &AuthorizationRequest<'_>,
        requirement: &ApprovalRequirement,
        now: Timestamp,
    ) {
        self.record_pending_sync(request, requirement, now);
    }
}
