/// Any error from this crate's public API, for callers that do not need
/// to distinguish the source.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GuardError {
    #[error(transparent)]
    Id(#[from] crate::IdError),
    #[error(transparent)]
    EntityUid(#[from] crate::EntityUidError),
    #[error(transparent)]
    Constraint(#[from] crate::ConstraintError),
    #[error(transparent)]
    Chain(#[from] crate::ChainError),
    #[error(transparent)]
    ReservedUserAttr(#[from] crate::ReservedUserAttr),
    #[error(transparent)]
    Handle(#[from] crate::HandleError),
    #[error(transparent)]
    ReservedContextKey(#[from] crate::ReservedContextKey),
    #[error(transparent)]
    GrantStore(#[from] crate::GrantStoreError),
    #[error(transparent)]
    Resolve(#[from] crate::ResolveError),
    #[error(transparent)]
    Execute(#[from] crate::ExecuteError),
    #[error(transparent)]
    Broker(#[from] crate::BrokerError),
    #[error(transparent)]
    Audit(#[from] crate::AuditError),
    #[error("denied: {0}")]
    Denied(crate::DenyReason),
}
