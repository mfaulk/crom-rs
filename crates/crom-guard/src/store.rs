//! In-memory grant store (#348). Restart invalidates every grant and handle
//! by construction (MILESTONE_18 §19 risk 10: fail closed); persistence is
//! deliberately not provided in M18.

use std::collections::{BTreeSet, HashMap};
use std::sync::RwLock;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::context::ExecutionContext;
use crate::decision::DenyReason;
use crate::grant::{DelegationSpec, Grant, GrantIssue, GrantSpec};
use crate::handle::SessionHandle;
use crate::ids::{ActionId, AgentId, AppId, GrantId};
use crate::time::Timestamp;
use crate::traits::{BoxFuture, GrantStore, GrantStoreError, IssuedGrant, Sealed};

/// Default and upper bound for [`GrantStoreConfig::max_ttl_seconds`]
/// (MILESTONE_18 §8.2: `expires_at ≤ now + max_ttl`, default 1 h).
pub const DEFAULT_MAX_TTL_SECONDS: u64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStoreConfig {
    /// Actions registered as protected operations; grants may name no other.
    pub registered_actions: BTreeSet<ActionId>,
    /// Longest lifetime a grant may request. Longer requests are rejected,
    /// not capped, so the application learns its grant is shorter.
    pub max_ttl_seconds: u64,
}

impl GrantStoreConfig {
    pub fn new(registered_actions: BTreeSet<ActionId>) -> Self {
        Self {
            registered_actions,
            max_ttl_seconds: DEFAULT_MAX_TTL_SECONDS,
        }
    }
}

type HandleHash = [u8; 32];

fn hash_handle(handle: &SessionHandle) -> HandleHash {
    Sha256::digest(handle.expose_secret().as_bytes()).into()
}

/// Index key: the first 8 bytes of the handle hash. The full hash is then
/// compared in constant time, so lookup timing reveals nothing about the
/// part of a stored hash an attacker has not already matched by chance.
fn selector(h: &HandleHash) -> u64 {
    u64::from_be_bytes(h[..8].try_into().expect("8 bytes"))
}

struct HandleEntry {
    hash: HandleHash,
    grant_id: GrantId,
}

#[derive(Default)]
struct State {
    grants: HashMap<GrantId, Grant>,
    handles: HashMap<u64, Vec<HandleEntry>>,
}

impl State {
    /// Loads the chain for `leaf` (leaf first) and validates its structure.
    fn chain(&self, leaf: &GrantId) -> Result<ExecutionContext, DenyReason> {
        let mut chain = Vec::new();
        let mut next = Some(leaf.clone());
        while let Some(id) = next {
            // A missing ancestor is store corruption: deny, never skip it.
            let g = self.grants.get(&id).ok_or(DenyReason::EngineUnavailable)?;
            if chain.len() > usize::from(u8::MAX) {
                return Err(DenyReason::EngineUnavailable);
            }
            next = g.parent().cloned();
            chain.push(g.clone());
        }
        ExecutionContext::from_chain(chain).map_err(|_| DenyReason::EngineUnavailable)
    }

    fn insert(&mut self, grant: Grant, hash: HandleHash) {
        self.handles
            .entry(selector(&hash))
            .or_default()
            .push(HandleEntry {
                hash,
                grant_id: grant.grant_id().clone(),
            });
        self.grants.insert(grant.grant_id().clone(), grant);
    }

    fn lookup(&self, hash: &HandleHash) -> Option<&GrantId> {
        let mut found = None;
        for e in self.handles.get(&selector(hash))? {
            if bool::from(e.hash.ct_eq(hash)) {
                found = Some(&e.grant_id);
            }
        }
        found
    }
}

/// Process-local grant store. A single lock orders `revoke` against
/// `authenticate`/`recheck`: once `revoke` returns, no later call observes
/// the grant (or any descendant) as live.
pub struct MemoryGrantStore {
    config: GrantStoreConfig,
    state: RwLock<State>,
}

impl std::fmt::Debug for MemoryGrantStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryGrantStore")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], GrantStoreError> {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).map_err(|_| GrantStoreError::Unavailable)?;
    Ok(b)
}

fn random_id(prefix: &str) -> Result<String, GrantStoreError> {
    let b: [u8; 16] = random_bytes()?;
    let mut s = String::with_capacity(prefix.len() + 32);
    s.push_str(prefix);
    for byte in b {
        s.push_str(&format!("{byte:02x}"));
    }
    Ok(s)
}

impl MemoryGrantStore {
    pub fn new(config: GrantStoreConfig) -> Self {
        Self {
            config,
            state: RwLock::new(State::default()),
        }
    }

    fn check_request(
        &self,
        actions: &BTreeSet<ActionId>,
        ttl_seconds: u64,
    ) -> Result<(), GrantStoreError> {
        if actions.is_empty() {
            return Err(GrantStoreError::InvalidSpec("actions is empty".into()));
        }
        if let Some(a) = actions
            .iter()
            .find(|a| !self.config.registered_actions.contains(*a))
        {
            return Err(GrantStoreError::InvalidSpec(format!("unknown action {a}")));
        }
        if ttl_seconds == 0 {
            return Err(GrantStoreError::InvalidSpec("ttl_seconds is 0".into()));
        }
        if ttl_seconds > self.config.max_ttl_seconds {
            return Err(GrantStoreError::InvalidSpec(format!(
                "ttl_seconds exceeds the maximum of {}",
                self.config.max_ttl_seconds
            )));
        }
        Ok(())
    }

    fn store_new(
        &self,
        spec: GrantSpec,
        issue: impl FnOnce(GrantId, AgentId) -> GrantIssue,
    ) -> Result<IssuedGrant, GrantStoreError> {
        let grant_id = GrantId::new(random_id("g_")?).expect("valid id");
        let agent = AgentId::new(random_id("a_")?).expect("valid id");
        let handle = SessionHandle::from_random_bytes(random_bytes()?);
        let grant = Grant::issue(spec, issue(grant_id, agent));
        let hash = hash_handle(&handle);
        self.state
            .write()
            .map_err(|_| GrantStoreError::Unavailable)?
            .insert(grant.clone(), hash);
        Ok(IssuedGrant { grant, handle })
    }

    fn issue_sync(
        &self,
        issuer: &AppId,
        spec: GrantSpec,
        now: Timestamp,
    ) -> Result<IssuedGrant, GrantStoreError> {
        self.check_request(&spec.actions, spec.ttl_seconds)?;
        let expires_at = now.saturating_add_secs(spec.ttl_seconds);
        self.store_new(spec, |grant_id, agent| GrantIssue {
            grant_id,
            issuer: issuer.clone(),
            agent,
            not_before: now,
            expires_at,
            parent: None,
        })
    }

    fn delegate_sync(
        &self,
        parent: &ExecutionContext,
        spec: DelegationSpec,
        now: Timestamp,
    ) -> Result<IssuedGrant, GrantStoreError> {
        // Re-read the parent chain from the store: the caller's snapshot may
        // predate a revocation.
        let current = {
            let state = self
                .state
                .read()
                .map_err(|_| GrantStoreError::Unavailable)?;
            state
                .chain(parent.leaf().grant_id())
                .map_err(|_| GrantStoreError::UnknownGrant)?
        };
        current
            .check_live(now)
            .map_err(|r| GrantStoreError::InvalidSpec(format!("parent grant is not live: {r}")))?;
        let p = current.leaf();
        self.check_request(&spec.actions, spec.ttl_seconds)?;
        if !spec.actions.is_subset(p.actions()) {
            return Err(GrantStoreError::InvalidSpec(
                "actions exceed the parent's".into(),
            ));
        }
        spec.constraints
            .narrows(p.constraints())
            .map_err(|e| GrantStoreError::InvalidSpec(e.to_string()))?;
        if p.max_depth() == 0 {
            return Err(GrantStoreError::InvalidSpec(
                "parent grant may not delegate".into(),
            ));
        }
        if spec.max_depth >= p.max_depth() {
            return Err(GrantStoreError::InvalidSpec(
                "max_depth must be below the parent's".into(),
            ));
        }
        let expires_at = now
            .saturating_add_secs(spec.ttl_seconds)
            .min(p.expires_at());
        let child_spec = GrantSpec {
            tenant: p.tenant().clone(),
            delegator: p.delegator().clone(),
            run_id: p.run_id().clone(),
            actions: spec.actions,
            constraints: spec.constraints,
            ttl_seconds: spec.ttl_seconds,
            max_depth: spec.max_depth,
        };
        let issuer = p.issuer().clone();
        let parent_id = p.grant_id().clone();
        self.store_new(child_spec, |grant_id, agent| GrantIssue {
            grant_id,
            issuer,
            agent,
            not_before: now,
            expires_at,
            parent: Some(parent_id),
        })
    }

    fn authenticate_sync(
        &self,
        handle: &SessionHandle,
        now: Timestamp,
    ) -> Result<ExecutionContext, DenyReason> {
        let hash = hash_handle(handle);
        let state = self
            .state
            .read()
            .map_err(|_| DenyReason::EngineUnavailable)?;
        let leaf = state.lookup(&hash).ok_or(DenyReason::NoGrant)?;
        let ctx = state.chain(leaf)?;
        ctx.check_live(now)?;
        Ok(ctx)
    }

    fn recheck_sync(&self, ctx: &ExecutionContext, now: Timestamp) -> Result<(), DenyReason> {
        let state = self
            .state
            .read()
            .map_err(|_| DenyReason::EngineUnavailable)?;
        let current = state.chain(ctx.leaf().grant_id())?;
        // The chain is immutable apart from revocation, so a different chain
        // under the same leaf id means corruption.
        if current.principal() != ctx.principal() {
            return Err(DenyReason::EngineUnavailable);
        }
        current.check_live(now)
    }

    fn revoke_sync(
        &self,
        issuer: &AppId,
        grant: &GrantId,
        now: Timestamp,
    ) -> Result<(), GrantStoreError> {
        let mut state = self
            .state
            .write()
            .map_err(|_| GrantStoreError::Unavailable)?;
        match state.grants.get_mut(grant) {
            Some(g) if g.issuer() == issuer => {
                g.revoke(now);
                Ok(())
            }
            _ => Err(GrantStoreError::UnknownGrant),
        }
    }
}

impl Sealed for MemoryGrantStore {}

impl GrantStore for MemoryGrantStore {
    fn issue<'a>(
        &'a self,
        issuer: &'a AppId,
        spec: GrantSpec,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<IssuedGrant, GrantStoreError>> {
        Box::pin(async move { self.issue_sync(issuer, spec, now) })
    }

    fn delegate<'a>(
        &'a self,
        parent: &'a ExecutionContext,
        spec: DelegationSpec,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<IssuedGrant, GrantStoreError>> {
        Box::pin(async move { self.delegate_sync(parent, spec, now) })
    }

    fn authenticate<'a>(
        &'a self,
        handle: &'a SessionHandle,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ExecutionContext, DenyReason>> {
        Box::pin(async move { self.authenticate_sync(handle, now) })
    }

    fn recheck<'a>(
        &'a self,
        ctx: &'a ExecutionContext,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<(), DenyReason>> {
        Box::pin(async move { self.recheck_sync(ctx, now) })
    }

    fn revoke<'a>(
        &'a self,
        issuer: &'a AppId,
        grant: &'a GrantId,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<(), GrantStoreError>> {
        Box::pin(async move { self.revoke_sync(issuer, grant, now) })
    }
}

#[cfg(test)]
mod tests;
