//! In-memory grant store (#348). Restart invalidates every grant and handle
//! by construction (MILESTONE_18 §19 risk 10: fail closed); persistence is
//! deliberately not provided in M18.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::RwLock;

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::context::ExecutionContext;
use crate::decision::DenyReason;
use crate::grant::{DelegationSpec, Grant, GrantIssue, GrantSpec};
use crate::handle::SessionHandle;
use crate::ids::{ActionId, AgentId, AppId, GrantId, TenantId};
use crate::time::Timestamp;
use crate::traits::{BoxFuture, GrantStore, GrantStoreError, IssuedGrant, Sealed};

/// Default and upper bound for [`GrantStoreConfig::max_ttl_seconds`]
/// (MILESTONE_18 §8.2: `expires_at ≤ now + max_ttl`, default 1 h).
pub const DEFAULT_MAX_TTL_SECONDS: u64 = 3600;
/// Default [`GrantStoreConfig::max_grants`].
pub const DEFAULT_MAX_GRANTS: usize = 100_000;
/// Default [`GrantStoreConfig::max_children_per_grant`].
pub const DEFAULT_MAX_CHILDREN_PER_GRANT: u32 = 16;
/// Default [`GrantStoreConfig::max_descendants_per_root`].
pub const DEFAULT_MAX_DESCENDANTS_PER_ROOT: u32 = 256;
/// Default [`GrantStoreConfig::max_grants_per_tenant`].
pub const DEFAULT_MAX_GRANTS_PER_TENANT: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantStoreConfig {
    /// Actions registered as protected operations; grants may name no other.
    pub registered_actions: BTreeSet<ActionId>,
    /// Longest lifetime a grant may request. Longer requests are rejected,
    /// not capped, so the application learns its grant is shorter.
    pub max_ttl_seconds: u64,
    /// Live-plus-unswept grants the store holds. When full, expired grants
    /// are swept; if it is still full, `issue` and `delegate` fail with
    /// `Unavailable` (agents can drive `delegate`, so memory must be bounded).
    pub max_grants: usize,
    /// Children one grant may ever create through `delegate`.
    pub max_children_per_grant: u32,
    /// Descendants, at any depth, that delegation under one root grant may
    /// ever create. Bounds what one agent session can allocate, whatever
    /// its `max_depth`.
    pub max_descendants_per_root: u32,
    /// Live-plus-unswept grants one tenant may hold, so one tenant cannot
    /// exhaust `max_grants` for the others. When reached, the store sweeps;
    /// if the tenant is still at quota, `issue` and `delegate` fail with
    /// `InvalidSpec`.
    pub max_grants_per_tenant: usize,
}

impl GrantStoreConfig {
    pub fn new(registered_actions: BTreeSet<ActionId>) -> Self {
        Self {
            registered_actions,
            max_ttl_seconds: DEFAULT_MAX_TTL_SECONDS,
            max_grants: DEFAULT_MAX_GRANTS,
            max_children_per_grant: DEFAULT_MAX_CHILDREN_PER_GRANT,
            max_descendants_per_root: DEFAULT_MAX_DESCENDANTS_PER_ROOT,
            max_grants_per_tenant: DEFAULT_MAX_GRANTS_PER_TENANT,
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
    children: HashMap<GrantId, u32>,
    /// Descendants ever created under each root, keyed by root id.
    descendants: HashMap<GrantId, u32>,
    /// Stored grants per tenant.
    per_tenant: HashMap<TenantId, usize>,
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

    /// Drops grants that can never be live again: expired (`expires_at <=
    /// now`) or revoked, together with every descendant, so no stored grant
    /// loses an ancestor. A child never expires after its parent; revoked
    /// subtrees are found by walking parents. Swept handles then fail with
    /// `NoGrant` instead of `GrantExpired` / `GrantRevoked`.
    fn sweep(&mut self, now: Timestamp) {
        let mut dead: HashSet<GrantId> = self
            .grants
            .values()
            .filter(|g| g.expires_at() <= now || g.revoked_at().is_some())
            .map(|g| g.grant_id().clone())
            .collect();
        loop {
            let more: Vec<GrantId> = self
                .grants
                .values()
                .filter(|g| {
                    !dead.contains(g.grant_id()) && g.parent().is_some_and(|p| dead.contains(p))
                })
                .map(|g| g.grant_id().clone())
                .collect();
            if more.is_empty() {
                break;
            }
            dead.extend(more);
        }
        self.grants.retain(|id, _| !dead.contains(id));
        let grants = &self.grants;
        self.handles.retain(|_, entries| {
            entries.retain(|e| grants.contains_key(&e.grant_id));
            !entries.is_empty()
        });
        self.children.retain(|id, _| grants.contains_key(id));
        self.descendants.retain(|id, _| grants.contains_key(id));
        self.per_tenant.clear();
        for g in self.grants.values() {
            *self.per_tenant.entry(g.tenant().clone()).or_insert(0) += 1;
        }
    }

    fn tenant_count(&self, tenant: &TenantId) -> usize {
        self.per_tenant.get(tenant).copied().unwrap_or(0)
    }

    fn insert(&mut self, grant: Grant, hash: HandleHash) {
        self.handles
            .entry(selector(&hash))
            .or_default()
            .push(HandleEntry {
                hash,
                grant_id: grant.grant_id().clone(),
            });
        *self.per_tenant.entry(grant.tenant().clone()).or_insert(0) += 1;
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

/// The attenuation rules of MILESTONE_18 §8.3 for a child of `parent`
/// created at `now`; returns the child's `expires_at`. Shared by
/// `delegate` and the `guard.delegate` pre-decision check, so both apply
/// exactly the same rules.
pub(crate) fn check_narrowing(
    parent: &Grant,
    spec: &DelegationSpec,
    now: Timestamp,
) -> Result<Timestamp, GrantStoreError> {
    if !spec.actions.is_subset(parent.actions()) {
        return Err(GrantStoreError::InvalidSpec(
            "actions exceed the parent's".into(),
        ));
    }
    spec.constraints
        .narrows(parent.constraints())
        .map_err(|e| GrantStoreError::InvalidSpec(e.to_string()))?;
    if parent.max_depth() == 0 {
        return Err(GrantStoreError::InvalidSpec(
            "parent grant may not delegate".into(),
        ));
    }
    if spec.max_depth >= parent.max_depth() {
        return Err(GrantStoreError::InvalidSpec(
            "max_depth must be below the parent's".into(),
        ));
    }
    let expires_at = now.saturating_add_secs(spec.ttl_seconds);
    if expires_at > parent.expires_at() {
        return Err(GrantStoreError::InvalidSpec(
            "expiry exceeds the parent's".into(),
        ));
    }
    Ok(expires_at)
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

    /// Generates ids and a handle, then inserts under the caller's write
    /// guard, so checks and insertion are one atomic step.
    fn store_new(
        &self,
        state: &mut State,
        now: Timestamp,
        spec: GrantSpec,
        issue: impl FnOnce(GrantId, AgentId) -> GrantIssue,
    ) -> Result<IssuedGrant, GrantStoreError> {
        let full = |s: &State| {
            if s.grants.len() >= self.config.max_grants {
                Some(GrantStoreError::Unavailable)
            } else if s.tenant_count(&spec.tenant) >= self.config.max_grants_per_tenant {
                Some(GrantStoreError::InvalidSpec(
                    "tenant has reached its grant quota".into(),
                ))
            } else {
                None
            }
        };
        if full(state).is_some() {
            state.sweep(now);
            if let Some(e) = full(state) {
                return Err(e);
            }
        }
        let grant_id = GrantId::new(random_id("g_")?).expect("valid id");
        let agent = AgentId::new(random_id("a_")?).expect("valid id");
        let handle = SessionHandle::from_random_bytes(random_bytes()?);
        let grant = Grant::issue(spec, issue(grant_id, agent));
        state.insert(grant.clone(), hash_handle(&handle));
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
        let mut state = self
            .state
            .write()
            .map_err(|_| GrantStoreError::Unavailable)?;
        self.store_new(&mut state, now, spec, |grant_id, agent| GrantIssue {
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
        self.check_request(&spec.actions, spec.ttl_seconds)?;
        // One write guard covers re-reading the parent, the checks, and the
        // insert, so a delegation cannot succeed after `revoke` returns.
        let mut state = self
            .state
            .write()
            .map_err(|_| GrantStoreError::Unavailable)?;
        // Re-read the chain: the caller's snapshot may predate a revocation.
        let current = state
            .chain(parent.leaf().grant_id())
            .map_err(|_| GrantStoreError::UnknownGrant)?;
        current
            .check_live(now)
            .map_err(|r| GrantStoreError::InvalidSpec(format!("parent grant is not live: {r}")))?;
        let p = current.leaf();
        let expires_at = check_narrowing(p, &spec, now)?;
        let used = state.children.get(p.grant_id()).copied().unwrap_or(0);
        if used >= self.config.max_children_per_grant {
            return Err(GrantStoreError::InvalidSpec(
                "parent grant has delegated its maximum number of children".into(),
            ));
        }
        let root_id = current
            .chain()
            .last()
            .expect("non-empty chain")
            .grant_id()
            .clone();
        let under_root = state.descendants.get(&root_id).copied().unwrap_or(0);
        if under_root >= self.config.max_descendants_per_root {
            return Err(GrantStoreError::InvalidSpec(
                "root grant has reached its maximum number of descendants".into(),
            ));
        }
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
        let issued = self.store_new(&mut state, now, child_spec, |grant_id, agent| GrantIssue {
            grant_id,
            issuer,
            agent,
            not_before: now,
            expires_at,
            parent: Some(parent_id.clone()),
        })?;
        *state.children.entry(parent_id).or_insert(0) += 1;
        *state.descendants.entry(root_id).or_insert(0) += 1;
        Ok(issued)
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
