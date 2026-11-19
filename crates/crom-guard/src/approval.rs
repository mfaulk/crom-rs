//! Approvals for `ApprovalRequired` decisions (MILESTONE_18 §9.4, #354).
//!
//! Only the trusted application's admin path calls [`MemoryApprovalStore::approve`];
//! no agent-facing operation reaches it. The approval id is an identifier,
//! not a credential: it authorizes nothing without the leaf grant's session
//! handle and the exact digested action, so it may appear in audit.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::action::ApprovalContext;
use crate::context::ExecutionContext;
use crate::decision::{ActionDigest, DenyReason};
use crate::grant::Delegator;
use crate::ids::{AppId, ApprovalId, TenantId};
use crate::pipeline::ApprovalVerifier;
use crate::resource::{ResolvedResource, ResourceVersion};
use crate::time::Timestamp;
use crate::traits::{BoxFuture, Sealed};
use crate::value::Attrs;

/// Default and upper bound for [`ApprovalStoreConfig::max_ttl_seconds`].
pub const DEFAULT_APPROVAL_MAX_TTL_SECONDS: u64 = 900;
/// Default for [`ApprovalStoreConfig::max_approvals`].
pub const DEFAULT_MAX_APPROVALS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalStoreConfig {
    pub max_ttl_seconds: u64,
    /// When full, `approve` first drops used and expired approvals, then
    /// fails with `Unavailable`.
    pub max_approvals: usize,
}

impl Default for ApprovalStoreConfig {
    fn default() -> Self {
        Self {
            max_ttl_seconds: DEFAULT_APPROVAL_MAX_TTL_SECONDS,
            max_approvals: DEFAULT_MAX_APPROVALS,
        }
    }
}

/// What a human approved. Built by the trusted application from its own
/// authenticated admin session, never from agent arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalSpec {
    /// From `CallResult::ApprovalRequired`; binds leaf grant, action,
    /// resource uid, and validated arguments.
    pub action_digest: ActionDigest,
    /// The resource version the approver saw; a retry against any other
    /// version is `approval_invalid`.
    pub resource_version: ResourceVersion,
    pub approver: Delegator,
    /// Must equal the principal's tenant at use, or the approval is
    /// rejected before Cedar sees it.
    pub approver_tenant: TenantId,
    /// Approved values exposed as `context.approval.<field>`, e.g.
    /// `amount_cents`. May not contain `approver`.
    pub fields: Attrs,
    pub ttl_seconds: u64,
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

/// In-memory, single-use, expiring approvals; the [`ApprovalVerifier`] to
/// install with `Guard::with_approval_verifier`. Lost on restart, which
/// fails closed. `verify` checks and consumes under one lock, so of
/// concurrent uses of one approval exactly one passes.
pub struct MemoryApprovalStore {
    config: ApprovalStoreConfig,
    state: Mutex<HashMap<ApprovalId, Stored>>,
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
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Records an approval by `issuer` (the trusted application) and
    /// returns its random id for the agent to present as
    /// `CallMeta::approval_id`. The id is the only output; nothing about
    /// the approval is echoed in errors.
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
        if spec.fields.contains_key("approver") {
            return Err(ApprovalError::InvalidSpec(
                "fields may not contain `approver`".into(),
            ));
        }
        let expires_at = now.saturating_add_secs(spec.ttl_seconds);
        let mut state = self.state.lock().map_err(|_| ApprovalError::Unavailable)?;
        if state.len() >= self.config.max_approvals {
            state.retain(|_, s| s.status(now) == ApprovalStatus::Pending);
            if state.len() >= self.config.max_approvals {
                return Err(ApprovalError::Unavailable);
            }
        }
        let approval_id = loop {
            let id = random_approval_id()?;
            if !state.contains_key(&id) {
                break id;
            }
        };
        state.insert(
            approval_id.clone(),
            Stored {
                issuer: issuer.clone(),
                tenant: spec.approver_tenant,
                approver: spec.approver,
                digest: spec.action_digest,
                version: spec.resource_version,
                fields: spec.fields,
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
        let s = state.get(approval_id)?;
        (s.issuer == *issuer).then(|| s.status(now))
    }

    /// Stored approvals, including used and expired ones not yet swept.
    pub fn len(&self) -> usize {
        self.state.lock().map_or(0, |s| s.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::UserId;

    fn spec(ttl: u64) -> ApprovalSpec {
        ApprovalSpec {
            action_digest: ActionDigest::from_bytes([7; 32]),
            resource_version: ResourceVersion::new("1"),
            approver: Delegator::new(UserId::new("u").unwrap(), "supervisor", Attrs::new())
                .unwrap(),
            approver_tenant: TenantId::new("t").unwrap(),
            fields: Attrs::new(),
            ttl_seconds: ttl,
        }
    }

    /// Ids are random `ap_` + 32 hex digits and distinct across calls.
    #[test]
    fn ids_are_random() {
        let s = MemoryApprovalStore::new(ApprovalStoreConfig::default());
        let app = AppId::new("app").unwrap();
        let a = s
            .approve(&app, spec(1), Timestamp::from_unix_millis(0))
            .unwrap();
        let b = s
            .approve(&app, spec(1), Timestamp::from_unix_millis(0))
            .unwrap();
        assert_ne!(a.approval_id, b.approval_id);
        assert_eq!(a.approval_id.as_str().len(), 3 + 32);
        assert_eq!(a.expires_at, Timestamp::from_unix_millis(1000));
    }

    /// A full store drops expired approvals to make room, and fails `Unavailable` when every one is still pending.
    #[test]
    fn capacity_sweeps_then_fails() {
        let s = MemoryApprovalStore::new(ApprovalStoreConfig {
            max_approvals: 2,
            ..ApprovalStoreConfig::default()
        });
        let app = AppId::new("app").unwrap();
        let t0 = Timestamp::from_unix_millis(0);
        s.approve(&app, spec(1), t0).unwrap();
        s.approve(&app, spec(10), t0).unwrap();
        assert_eq!(
            s.approve(&app, spec(1), t0),
            Err(ApprovalError::Unavailable)
        );
        let t1 = Timestamp::from_unix_millis(1000);
        s.approve(&app, spec(1), t1).unwrap();
        assert_eq!(s.len(), 2);
    }
}
