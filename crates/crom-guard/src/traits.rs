use std::future::Future;
use std::pin::Pin;

use crate::action::{AuthorizationContext, AuthorizationRequest, AuthorizedAction, ValidatedArgs};
use crate::context::ExecutionContext;
use crate::credential::CredentialLease;
use crate::decision::{Decision, DenyReason};
use crate::grant::{DelegationSpec, Grant, GrantSpec};
use crate::handle::SessionHandle;
use crate::ids::{ActionId, AppId, GrantId, TenantId};
use crate::resource::ResolvedResource;
use crate::time::Timestamp;

/// Boxed future, so async traits stay object-safe without extra
/// dependencies in the TCB.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

mod sealed {
    /// Implemented only inside `crom-guard`. Traits that produce authority
    /// (an [`ExecutionContext`](crate::ExecutionContext) or a
    /// [`Decision`](crate::Decision)) require it.
    pub trait Sealed {}
}
#[allow(unused_imports)] // implemented by the store (#348) and PDP (#349)
pub(crate) use sealed::Sealed;

/// A newly issued grant and the handle for it. The handle is returned to
/// the trusted application only; Guard keeps its hash.
#[allow(missing_debug_implementations)] // holds a secret; deliberately not Debug
pub struct IssuedGrant {
    pub grant: Grant,
    pub handle: SessionHandle,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GrantStoreError {
    #[error("unknown grant")]
    UnknownGrant,
    #[error("grant spec rejected: {0}")]
    InvalidSpec(String),
    #[error("grant store unavailable")]
    Unavailable,
}

/// Grants and session handles (#348). Sealed: an `ExecutionContext` can
/// only come from this crate's stores.
///
/// ```compile_fail
/// struct Fake;
/// impl crom_guard::GrantStore for Fake {
///     fn issue<'a>(&'a self, _: &'a crom_guard::AppId, _: crom_guard::GrantSpec, _: crom_guard::Timestamp)
///         -> crom_guard::BoxFuture<'a, Result<crom_guard::IssuedGrant, crom_guard::GrantStoreError>> { todo!() }
///     fn delegate<'a>(&'a self, _: &'a crom_guard::ExecutionContext, _: crom_guard::DelegationSpec, _: crom_guard::Timestamp)
///         -> crom_guard::BoxFuture<'a, Result<crom_guard::IssuedGrant, crom_guard::GrantStoreError>> { todo!() }
///     fn authenticate<'a>(&'a self, _: &'a crom_guard::SessionHandle, _: crom_guard::Timestamp)
///         -> crom_guard::BoxFuture<'a, Result<crom_guard::ExecutionContext, crom_guard::DenyReason>> { todo!() }
///     fn recheck<'a>(&'a self, _: &'a crom_guard::ExecutionContext, _: crom_guard::Timestamp)
///         -> crom_guard::BoxFuture<'a, Result<(), crom_guard::DenyReason>> { todo!() }
///     fn revoke<'a>(&'a self, _: &'a crom_guard::AppId, _: &'a crom_guard::GrantId, _: crom_guard::Timestamp)
///         -> crom_guard::BoxFuture<'a, Result<(), crom_guard::GrantStoreError>> { todo!() }
/// }
/// ```
pub trait GrantStore: sealed::Sealed + Send + Sync {
    /// Issues a root grant for `issuer`, the authenticated admin caller.
    fn issue<'a>(
        &'a self,
        issuer: &'a AppId,
        spec: GrantSpec,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<IssuedGrant, GrantStoreError>>;

    /// Issues a child of `parent.leaf()` (#353): actions and constraints
    /// must narrow, expiry is capped at the parent's, and depth decreases.
    /// Tenant, delegator, issuer, and run are copied from the parent.
    fn delegate<'a>(
        &'a self,
        parent: &'a ExecutionContext,
        spec: DelegationSpec,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<IssuedGrant, GrantStoreError>>;

    /// Maps a presented handle to its live grant chain. Unknown handles,
    /// and chains with any expired, not-yet-valid, or revoked grant, are
    /// denied.
    fn authenticate<'a>(
        &'a self,
        handle: &'a SessionHandle,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<ExecutionContext, DenyReason>>;

    /// Re-reads expiry and revocation for every grant in `ctx` from the
    /// store, immediately before execution (MILESTONE_18 §10.2 step 6).
    fn recheck<'a>(
        &'a self,
        ctx: &'a ExecutionContext,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<(), DenyReason>>;

    /// Revokes `grant` and, through the chain walk, every descendant.
    /// Fails with `UnknownGrant` unless `issuer` issued it, so one
    /// application cannot revoke another's grants.
    fn revoke<'a>(
        &'a self,
        issuer: &'a AppId,
        grant: &'a GrantId,
        now: Timestamp,
    ) -> BoxFuture<'a, Result<(), GrantStoreError>>;
}

/// The Cedar policy decision point (#349). Sealed: only this crate decides.
/// Infallible by signature: every engine error becomes a `Deny`.
/// Synchronous: evaluation is CPU-bound and does no I/O.
///
/// ```compile_fail
/// struct Fake;
/// impl crom_guard::PolicyDecisionPoint for Fake {
///     fn authorize(&self, _: &crom_guard::AuthorizationRequest<'_>) -> crom_guard::Decision { todo!() }
///     fn policy_set_hash(&self) -> &str { "" }
///     fn schema_hash(&self) -> &str { "" }
/// }
/// ```
pub trait PolicyDecisionPoint: sealed::Sealed + Send + Sync {
    fn authorize(&self, request: &AuthorizationRequest<'_>) -> Decision;
    /// Hex SHA-256 of the loaded policy set, recorded in audit.
    fn policy_set_hash(&self) -> &str;
    /// Hex SHA-256 of the loaded schema, recorded in audit.
    fn schema_hash(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// Not found, or found in another tenant. The two must be reported
    /// identically.
    #[error("resource not found")]
    NotFound,
    #[error("resource source unavailable")]
    Unavailable,
}

/// Maps the identifier in validated arguments to a resource, using trusted
/// application data scoped to the principal's tenant (#350). Never copies
/// attribute values from the arguments.
pub trait ResourceResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        tenant: &'a TenantId,
        args: &'a ValidatedArgs,
    ) -> BoxFuture<'a, Result<ResolvedResource, ResolveError>>;
}

/// What an executor reports after a downstream call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutput {
    /// Returned to the agent as MCP structured content.
    pub content: serde_json::Value,
    /// Business-API effect id, for ledger correlation.
    pub downstream_ref: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecuteError {
    /// The downstream precondition failed (e.g. `If-Match` mismatch).
    #[error("downstream precondition failed")]
    Precondition,
    /// The detail is for the audit log; agents see only a generic code.
    #[error("downstream call failed: {0}")]
    Failed(String),
}

/// One business operation Guard exposes as an MCP tool (#351). Application
/// code, trusted (ADR 0002 §2.5).
pub trait ProtectedOperation: Send + Sync {
    /// The single action id and MCP tool name; no aliases.
    fn action(&self) -> &ActionId;

    /// Tool description shown to agents. Informational only.
    fn description(&self) -> &str;

    /// JSON Schema for the arguments. The registry rejects schemas without
    /// `additionalProperties: false` (#351).
    fn input_schema(&self) -> &serde_json::Value;

    /// Argument fields that may appear in audit `args_logged`.
    fn logged_fields(&self) -> &[&'static str] {
        &[]
    }

    fn resolver(&self) -> &dyn ResourceResolver;

    /// Request-specific Cedar context (e.g. `amount_cents`) from validated
    /// arguments.
    fn context(&self, args: &ValidatedArgs) -> Result<AuthorizationContext, DenyReason>;

    /// Performs the downstream call for exactly `action.resource()`,
    /// conditioned on `action.resource_version()`, authenticated with
    /// `credential`, and tagged with `action.decision_id()`.
    fn execute<'a>(
        &'a self,
        action: AuthorizedAction,
        credential: CredentialLease,
    ) -> BoxFuture<'a, Result<ExecutionOutput, ExecuteError>>;
}
