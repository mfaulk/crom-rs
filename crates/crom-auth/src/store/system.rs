//! Stub for the OS keychain backend.
//!
//! Keeping this as a structural stub means consumers can write `auth.credential_store = "keychain"`
//! today and get a clear "credential store unavailable" error if no keychain
//! crate is compiled in, instead of a confusing "feature not enabled"
//! message that mentions Cargo features. A future commit can drop in a
//! `keyring`-backed implementation behind a Cargo feature without changing
//! any callers.

use crate::error::AuthError;
use crate::types::{AccountId, ProviderId, StoredCredentialSecret};

use super::SecretBackend;

pub struct SystemKeychainBackend {
    available: bool,
}

impl SystemKeychainBackend {
    pub fn new() -> Self {
        // No keychain integration is compiled in by default. We expose
        // this stub so the surface area stays stable; once a keychain
        // crate is added behind a feature, flip `available` to `true`
        // when initialization succeeds.
        Self { available: false }
    }

    pub fn is_available(&self) -> bool {
        self.available
    }
}

impl Default for SystemKeychainBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretBackend for SystemKeychainBackend {
    fn name(&self) -> &'static str {
        "system_keychain"
    }

    fn put(
        &self,
        _provider: &ProviderId,
        _account_id: &AccountId,
        _secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        Err(AuthError::new(
            crate::error::AuthErrorKind::CredentialStoreUnavailable,
            "OS keychain backend is not enabled in this build.",
        ))
    }

    fn get(
        &self,
        _provider: &ProviderId,
        _account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError> {
        Ok(None)
    }

    fn delete(&self, _provider: &ProviderId, _account_id: &AccountId) -> Result<(), AuthError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Stub keychain reports unavailable so the credential store falls
    // back to file or noop per the configured policy.
    fn stub_reports_unavailable() {
        let kc = SystemKeychainBackend::new();
        assert!(!kc.is_available());
    }
}
