use crate::ids::ActionId;

/// A downstream credential lent to one executor call. It never leaves the
/// Guard process: no `Debug`, `Display`, `Serialize`, or `Clone`, and the
/// secret is reachable only inside [`with_secret`](Self::with_secret),
/// which is meant for setting a header on the outgoing request.
///
/// ```compile_fail
/// fn show(l: &crom_guard::CredentialLease) -> String { format!("{:?}", l) }
/// ```
///
/// ```compile_fail
/// fn show(l: &crom_guard::CredentialLease) -> String { format!("{}", l) }
/// ```
///
/// ```compile_fail
/// fn dup(l: &crom_guard::CredentialLease) -> crom_guard::CredentialLease { l.clone() }
/// ```
///
/// ```compile_fail
/// fn ser(l: &crom_guard::CredentialLease) -> String { serde_json::to_string(l).unwrap() }
/// ```
#[allow(missing_debug_implementations)] // holds a secret; deliberately not Debug
pub struct CredentialLease {
    credential_id: String,
    secret: String,
}

impl CredentialLease {
    /// For [`CredentialBroker`] implementations.
    pub fn new(credential_id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self {
            credential_id: credential_id.into(),
            secret: secret.into(),
        }
    }

    /// Non-secret id, recorded in audit and the business ledger.
    pub fn credential_id(&self) -> &str {
        &self.credential_id
    }

    /// Runs `apply` with the secret, e.g. to set `Authorization` on the
    /// downstream request. Do not return or store the secret from `apply`.
    pub fn with_secret<R>(&self, apply: impl FnOnce(&str) -> R) -> R {
        apply(&self.secret)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrokerError {
    #[error("no credential is configured for action {0}")]
    NotConfigured(ActionId),
    #[error("credential source unavailable")]
    Unavailable,
}

/// Holds downstream credentials inside the Guard process and lends them to
/// executors (#355). The agent never receives one.
pub trait CredentialBroker: Send + Sync {
    fn lease(&self, action: &ActionId) -> Result<CredentialLease, BrokerError>;
}
