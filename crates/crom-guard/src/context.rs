use serde::Serialize;

use crate::grant::{Delegator, Grant};
use crate::ids::{AgentId, AppId, GrantId, RunId, TenantId};
use crate::time::Timestamp;

/// Who is acting, derived entirely from the grant chain (MILESTONE_18 §8.4).
/// Nothing here comes from the agent's request.
///
/// There is no public constructor and no `Deserialize`:
///
/// ```compile_fail
/// let p: crom_guard::TrustedPrincipal = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// let p = crom_guard::TrustedPrincipal {
///     agent: "a".parse().unwrap(),
///     tenant: "t".parse().unwrap(),
///     delegator: todo!(),
///     issuer: "app".parse().unwrap(),
///     run_id: "r".parse().unwrap(),
///     grant_chain: vec![],
/// };
/// ```
///
/// By contrast `GrantSpec`, which carries no authority, deserializes:
///
/// ```
/// # fn _f() -> Result<crom_guard::GrantSpec, serde_json::Error> {
/// serde_json::from_str::<crom_guard::GrantSpec>("{}")
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TrustedPrincipal {
    agent: AgentId,
    tenant: TenantId,
    delegator: Delegator,
    issuer: AppId,
    run_id: RunId,
    /// Leaf first.
    grant_chain: Vec<GrantId>,
}

impl TrustedPrincipal {
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }
    pub fn delegator(&self) -> &Delegator {
        &self.delegator
    }
    pub fn issuer(&self) -> &AppId {
        &self.issuer
    }
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
    /// The grant the presented handle maps to.
    pub fn grant_id(&self) -> &GrantId {
        &self.grant_chain[0]
    }
    /// Leaf first, root last.
    pub fn grant_chain(&self) -> &[GrantId] {
        &self.grant_chain
    }
}

/// Why a grant chain is structurally invalid. The store refuses to
/// authenticate such a chain; it indicates store corruption or a bug.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChainError {
    #[error("grant chain is empty")]
    Empty,
    #[error("grant {child} does not name {parent} as its parent")]
    BrokenLink { child: GrantId, parent: GrantId },
    #[error("root grant {0} has a parent")]
    RootHasParent(GrantId),
    #[error("grant {0} differs from its parent in tenant, delegator, issuer, or run")]
    IdentityMismatch(GrantId),
    #[error("grant {0} does not have less delegation depth than its parent")]
    DepthNotDecreasing(GrantId),
}

/// Authority for one request: the authenticated grant chain and the
/// principal derived from it. Only the grant store builds one, from a
/// validated session handle. No public constructor, no `Deserialize`:
///
/// ```compile_fail
/// let c: crom_guard::ExecutionContext = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// let c = crom_guard::ExecutionContext { chain: vec![], principal: todo!() };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExecutionContext {
    /// Leaf first, root last.
    chain: Vec<Grant>,
    principal: TrustedPrincipal,
}

impl ExecutionContext {
    /// Builds a context from a chain loaded by the store, leaf first.
    /// Checks structure only: parent links, shared identity, and
    /// decreasing depth. Liveness is checked separately with
    /// [`check_live`](Self::check_live).
    #[allow(dead_code)] // used by the grant store (#348)
    pub(crate) fn from_chain(chain: Vec<Grant>) -> Result<Self, ChainError> {
        let leaf = chain.first().ok_or(ChainError::Empty)?;
        for pair in chain.windows(2) {
            let (child, parent) = (&pair[0], &pair[1]);
            if child.parent() != Some(parent.grant_id()) {
                return Err(ChainError::BrokenLink {
                    child: child.grant_id().clone(),
                    parent: parent.grant_id().clone(),
                });
            }
            if child.tenant() != parent.tenant()
                || child.delegator() != parent.delegator()
                || child.issuer() != parent.issuer()
                || child.run_id() != parent.run_id()
            {
                return Err(ChainError::IdentityMismatch(child.grant_id().clone()));
            }
            if child.max_depth() >= parent.max_depth() {
                return Err(ChainError::DepthNotDecreasing(child.grant_id().clone()));
            }
        }
        let root = chain.last().expect("non-empty");
        if root.parent().is_some() {
            return Err(ChainError::RootHasParent(root.grant_id().clone()));
        }
        let principal = TrustedPrincipal {
            agent: leaf.agent().clone(),
            tenant: leaf.tenant().clone(),
            delegator: leaf.delegator().clone(),
            issuer: leaf.issuer().clone(),
            run_id: leaf.run_id().clone(),
            grant_chain: chain.iter().map(|g| g.grant_id().clone()).collect(),
        };
        Ok(Self { chain, principal })
    }

    pub fn principal(&self) -> &TrustedPrincipal {
        &self.principal
    }

    /// Leaf first, root last. Enforcement evaluates every grant.
    pub fn chain(&self) -> &[Grant] {
        &self.chain
    }

    /// The grant the presented handle maps to.
    pub fn leaf(&self) -> &Grant {
        &self.chain[0]
    }

    /// Every grant in the chain must be live; revoking a parent therefore
    /// disables every descendant (MILESTONE_18 §8.2).
    pub fn check_live(&self, now: Timestamp) -> Result<(), crate::DenyReason> {
        self.chain.iter().try_for_each(|g| g.check_live(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grant::tests::{grant, spec};
    use crate::{DenyReason, RunId, TenantId};

    fn child_spec() -> crate::GrantSpec {
        let mut s = spec();
        s.max_depth = 0;
        s
    }

    /// A valid two-grant chain yields a principal from the leaf with the full chain, leaf first.
    #[test]
    fn valid_chain_builds_principal() {
        let ctx = ExecutionContext::from_chain(vec![
            grant("g2", Some("g1"), child_spec()),
            grant("g1", None, spec()),
        ])
        .unwrap();
        let p = ctx.principal();
        assert_eq!(p.agent().as_str(), "agent-g2");
        assert_eq!(p.grant_id().as_str(), "g2");
        let chain: Vec<&str> = p.grant_chain().iter().map(|g| g.as_str()).collect();
        assert_eq!(chain, ["g2", "g1"]);
        assert_eq!(p.tenant().as_str(), "acme");
        assert_eq!(ctx.leaf().grant_id().as_str(), "g2");
    }

    /// Empty chains, broken links, parented roots, identity changes, and non-decreasing depth are rejected.
    #[test]
    fn malformed_chains_rejected() {
        assert_eq!(ExecutionContext::from_chain(vec![]), Err(ChainError::Empty));
        let broken = ExecutionContext::from_chain(vec![
            grant("g2", Some("gX"), child_spec()),
            grant("g1", None, spec()),
        ]);
        assert!(matches!(broken, Err(ChainError::BrokenLink { .. })));
        let orphan = ExecutionContext::from_chain(vec![grant("g2", Some("g1"), child_spec())]);
        assert!(matches!(orphan, Err(ChainError::RootHasParent(_))));

        let mut other_tenant = child_spec();
        other_tenant.tenant = TenantId::new("globex").unwrap();
        let r = ExecutionContext::from_chain(vec![
            grant("g2", Some("g1"), other_tenant),
            grant("g1", None, spec()),
        ]);
        assert!(matches!(r, Err(ChainError::IdentityMismatch(_))));
        let mut other_run = child_spec();
        other_run.run_id = RunId::new("run-2").unwrap();
        let r = ExecutionContext::from_chain(vec![
            grant("g2", Some("g1"), other_run),
            grant("g1", None, spec()),
        ]);
        assert!(matches!(r, Err(ChainError::IdentityMismatch(_))));

        let r = ExecutionContext::from_chain(vec![
            grant("g2", Some("g1"), spec()),
            grant("g1", None, spec()),
        ]);
        assert!(matches!(r, Err(ChainError::DepthNotDecreasing(_))));
    }

    /// Revoking the root makes the whole chain unusable.
    #[test]
    fn parent_revocation_disables_child() {
        let mut root = grant("g1", None, spec());
        let now = root.not_before();
        root.revoke(now);
        let ctx = ExecutionContext::from_chain(vec![grant("g2", Some("g1"), child_spec()), root])
            .unwrap();
        assert_eq!(ctx.leaf().check_live(now), Ok(()));
        assert_eq!(ctx.check_live(now), Err(DenyReason::GrantRevoked));
    }
}
