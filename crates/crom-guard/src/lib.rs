//! Crom Guard: the enforcement point for agent-initiated operations.
//!
//! Trusted computing base. This crate must not depend on any other `crom-*`
//! crate so it can be used without the Crom runtime and extracted to its own
//! repository (ADR 0002, `docs/security/decisions/0002-guard-architecture.md`).
//!
//! Authority is carried only by types this crate constructs:
//! [`ExecutionContext`] / [`TrustedPrincipal`] (from the grant store) and
//! [`AuthorizedAction`] (from the pipeline). Executors accept only
//! `AuthorizedAction`, so a non-`Allow` [`Decision`] cannot execute.
//! See `docs/guard/README.md`.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

mod action;
mod audit;
mod context;
mod credential;
mod decision;
mod error;
mod grant;
mod handle;
mod ids;
mod resource;
mod time;
mod traits;
mod value;

pub use action::{
    ApprovalContext, AuthorizationContext, AuthorizationRequest, AuthorizedAction,
    ReservedContextKey, ValidatedArgs, RESERVED_CONTEXT_KEYS,
};
pub use audit::{
    verify_file, verify_log, verify_log_with, AuditEntry, AuditStorage, ChainSummary,
    ChainedAuditSink, FileAuditSink, VerifyError, VerifyErrorKind, GENESIS_HASH,
};
pub use audit::{
    AuditError, AuditIdentity, AuditReceipt, AuditRecord, AuditSink, DecisionKind, Durations,
    Enforcement, Outcome, RecordKind, UntrustedCorrelation, AUDIT_FORMAT,
};
pub use context::{ChainError, ExecutionContext, TrustedPrincipal};
pub use credential::{BrokerError, CredentialBroker, CredentialLease};
pub use decision::{ActionDigest, ApprovalRequirement, Decision, DenyReason};
pub use error::GuardError;
pub use grant::{
    Constraint, ConstraintError, Constraints, DelegationSpec, Delegator, Grant, GrantSpec,
    ReservedUserAttr, GRANT_SCHEMA_VERSION, RESERVED_USER_ATTRS,
};
pub use handle::{HandleError, SessionHandle, HANDLE_PREFIX};
pub use ids::{
    ActionId, AgentId, AppId, ApprovalId, DecisionId, GrantId, IdError, PolicyId, RunId, TenantId,
    UserId, MAX_ID_LEN, RESERVED_ID_PREFIX,
};
pub use resource::{ResolvedResource, ResourceVersion};
pub use time::{Clock, SystemClock, Timestamp};
pub use traits::{
    BoxFuture, ExecuteError, ExecutionOutput, GrantStore, GrantStoreError, IssuedGrant,
    PolicyDecisionPoint, ProtectedOperation, ResolveError, ResourceResolver,
};
pub use value::{AttrValue, Attrs, EntityUid, EntityUidError, MAX_ENTITY_ID_LEN};
