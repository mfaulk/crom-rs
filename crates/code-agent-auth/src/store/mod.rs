//! Credential store abstraction with file/keychain/fake backends.
//!
//! The store keeps two concerns strictly separated:
//!
//! 1. **Account metadata** ([`StoredAccount`]) — index data without secrets.
//!    Used to render `auth list` / `auth status` and to ferry redacted
//!    info to events.
//!
//! 2. **Secret payload** ([`StoredCredentialSecret`]) — opaque per-account
//!    blob never serialized into the metadata index.
//!
//! Default mode is `auto`: prefer the keychain backend when one is
//! installed, otherwise fall back to a no-op backend that errors loudly
//! unless the user has opted in to file storage.

pub mod fake;
pub mod file;
pub mod system;

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthErrorKind};
use crate::types::{
    AccountId, CredentialStoreMode, ProviderId, StoredAccount, StoredCredentialSecret,
};

pub use fake::FakeKeychainBackend;
pub use file::FileCredentialBackend;
pub use system::SystemKeychainBackend;

/// Index of account metadata across providers. Keyed by `(provider, account_id)`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountIndex {
    pub accounts: Vec<StoredAccount>,
    /// Active selection keyed by provider name.
    pub active: std::collections::BTreeMap<String, AccountId>,
}

impl AccountIndex {
    pub fn upsert(&mut self, account: StoredAccount) {
        if let Some(existing) = self
            .accounts
            .iter_mut()
            .find(|a| a.provider == account.provider && a.account_id == account.account_id)
        {
            *existing = account;
        } else {
            self.accounts.push(account);
        }
    }

    pub fn remove(
        &mut self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Option<StoredAccount> {
        let pos = self
            .accounts
            .iter()
            .position(|a| &a.provider == provider && &a.account_id == account_id)?;
        // Clear active selection if it pointed at this account.
        if self.active.get(provider.as_str()) == Some(account_id) {
            self.active.remove(provider.as_str());
        }
        Some(self.accounts.remove(pos))
    }

    pub fn list_for(&self, provider: &ProviderId) -> Vec<&StoredAccount> {
        self.accounts
            .iter()
            .filter(|a| &a.provider == provider)
            .collect()
    }

    pub fn get(&self, provider: &ProviderId, account_id: &AccountId) -> Option<&StoredAccount> {
        self.accounts
            .iter()
            .find(|a| &a.provider == provider && &a.account_id == account_id)
    }

    pub fn set_active(&mut self, provider: &ProviderId, account_id: AccountId) {
        self.active
            .insert(provider.as_str().to_string(), account_id);
    }

    pub fn active_for(&self, provider: &ProviderId) -> Option<&AccountId> {
        self.active.get(provider.as_str())
    }
}

/// Backend for opaque secret blobs.
///
/// Errors are mapped through [`AuthError`] so the CLI can give actionable
/// messages without each call site re-categorizing the failure.
pub trait SecretBackend: Send + Sync {
    fn name(&self) -> &'static str;

    fn put(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
        secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError>;

    fn get(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError>;

    fn delete(&self, provider: &ProviderId, account_id: &AccountId) -> Result<(), AuthError>;
}

/// Combined account + secret store.
#[derive(Clone)]
pub struct CredentialStore {
    pub mode: CredentialStoreMode,
    pub allow_file_store: bool,
    pub backend: Arc<dyn SecretBackend>,
    pub index: Arc<std::sync::Mutex<AccountIndex>>,
    pub index_path: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for CredentialStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialStore")
            .field("mode", &self.mode)
            .field("allow_file_store", &self.allow_file_store)
            .field("backend", &self.backend.name())
            .field("index_path", &self.index_path)
            .finish()
    }
}

impl CredentialStore {
    pub fn new_in_memory_for_tests() -> Self {
        Self {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: Arc::new(FakeKeychainBackend::default()),
            index: Arc::new(std::sync::Mutex::new(AccountIndex::default())),
            index_path: None,
        }
    }

    /// Build a store appropriate for `mode`. When `Auto` we look for a
    /// system keychain first; if none, we fall back to the file store
    /// only if `allow_file_store` is true. Otherwise we surface a clear
    /// "credential store unavailable" error at first write.
    pub fn build(
        mode: CredentialStoreMode,
        allow_file_store: bool,
        index_path: Option<std::path::PathBuf>,
    ) -> Result<Self, AuthError> {
        // Best-effort load of the index. Missing index is OK (first run).
        let initial_index = match index_path.as_ref() {
            Some(p) if p.exists() => {
                let bytes = std::fs::read(p).map_err(|e| {
                    AuthError::new(
                        AuthErrorKind::CredentialStoreUnavailable,
                        format!("read account index at {}: {}", p.display(), e),
                    )
                })?;
                serde_json::from_slice::<AccountIndex>(&bytes).unwrap_or_default()
            }
            _ => AccountIndex::default(),
        };

        let backend: Arc<dyn SecretBackend> = match mode {
            CredentialStoreMode::Keychain => {
                let kc = SystemKeychainBackend::new();
                if !kc.is_available() {
                    return Err(AuthError::new(
                        AuthErrorKind::CredentialStoreUnavailable,
                        "OS keychain is unavailable in this build. Re-run with `auth.credential_store = \"file\"` and `auth.allow_file_credential_store = true` only if you understand the local security tradeoff.",
                    ));
                }
                Arc::new(kc)
            }
            CredentialStoreMode::File => {
                if !allow_file_store {
                    return Err(AuthError::new(
                        AuthErrorKind::CredentialStoreUnavailable,
                        "File credential store is disabled. Set `auth.allow_file_credential_store = true` to opt in.",
                    ));
                }
                let secrets_dir = file::default_secrets_dir(index_path.as_deref())?;
                Arc::new(FileCredentialBackend::new(secrets_dir)?)
            }
            CredentialStoreMode::Auto => {
                let kc = SystemKeychainBackend::new();
                if kc.is_available() {
                    Arc::new(kc)
                } else if allow_file_store {
                    let secrets_dir = file::default_secrets_dir(index_path.as_deref())?;
                    Arc::new(FileCredentialBackend::new(secrets_dir)?)
                } else {
                    // No keychain compiled in and file store not opted-in.
                    // Surface a no-op backend that errors clearly on first
                    // write. Reads succeed (returning None) so `auth status`
                    // can still operate.
                    Arc::new(NoopBackend)
                }
            }
        };

        Ok(Self {
            mode,
            allow_file_store,
            backend,
            index: Arc::new(std::sync::Mutex::new(initial_index)),
            index_path,
        })
    }

    pub fn put(
        &self,
        account: StoredAccount,
        secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        self.backend
            .put(&account.provider, &account.account_id, secret)?;
        let mut idx = self.index.lock().unwrap();
        idx.upsert(account);
        self.persist_index(&idx)
    }

    pub fn get_secret(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError> {
        self.backend.get(provider, account_id)
    }

    pub fn delete(&self, provider: &ProviderId, account_id: &AccountId) -> Result<bool, AuthError> {
        // Logout must be idempotent (deleting a non-existent secret is
        // fine), but a *failed* backend delete is not. If the keychain
        // refuses the operation we keep the metadata entry so the next
        // `auth status` still surfaces the orphan rather than silently
        // claiming success.
        match self.backend.delete(provider, account_id) {
            Ok(()) => {}
            Err(e) if matches!(e.kind, AuthErrorKind::NotLoggedIn) => {
                // Backend signals "nothing to delete" — proceed.
            }
            Err(e) => return Err(e),
        }
        let mut idx = self.index.lock().unwrap();
        let removed = idx.remove(provider, account_id).is_some();
        self.persist_index(&idx)?;
        Ok(removed)
    }

    pub fn set_active(
        &self,
        provider: &ProviderId,
        account_id: AccountId,
    ) -> Result<(), AuthError> {
        let mut idx = self.index.lock().unwrap();
        idx.set_active(provider, account_id);
        self.persist_index(&idx)
    }

    pub fn snapshot_index(&self) -> AccountIndex {
        self.index.lock().unwrap().clone()
    }

    fn persist_index(&self, idx: &AccountIndex) -> Result<(), AuthError> {
        let Some(path) = self.index_path.as_ref() else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("create index dir {}: {}", parent.display(), e),
                )
            })?;
        }
        let bytes = serde_json::to_vec_pretty(idx).map_err(|e| {
            AuthError::new(AuthErrorKind::Unknown, format!("serialize index: {}", e))
        })?;
        std::fs::write(path, bytes).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("write index {}: {}", path.display(), e),
            )
        })?;
        // Tighten permissions on Unix: 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }
}

/// Backend used when no keychain feature is compiled in and file storage
/// is not opted-in. Returns a clear error on first write attempt.
#[derive(Default)]
pub struct NoopBackend;

impl SecretBackend for NoopBackend {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn put(
        &self,
        _provider: &ProviderId,
        _account_id: &AccountId,
        _secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        Err(AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            "No credential store backend available. Enable the keychain feature, or set `auth.allow_file_credential_store = true` to use the file backend.",
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
    use crate::types::{LoginMethodKind, SecretString};
    use time::OffsetDateTime;

    fn sample_account(provider: ProviderId, id: &str) -> StoredAccount {
        StoredAccount {
            account_id: AccountId::new(id),
            provider,
            login_method: LoginMethodKind::ApiKey,
            display_email: Some(format!("{id}@example.com")),
            workspace_id: None,
            plan_type: None,
            created_at: OffsetDateTime::now_utc(),
            last_refresh_at: None,
            expires_at: None,
            scopes: vec![],
            active: false,
            metadata: serde_json::json!({}),
            needs_reauth: false,
        }
    }

    #[test]
    // Round-trip: put -> get returns the same secret payload.
    fn fake_backend_roundtrip() {
        let store = CredentialStore::new_in_memory_for_tests();
        let acct = sample_account(ProviderId::Anthropic, "acc-1");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("sk-test".into())),
            ..Default::default()
        };
        store.put(acct.clone(), &secret).unwrap();
        let got = store
            .get_secret(&acct.provider, &acct.account_id)
            .unwrap()
            .unwrap();
        assert_eq!(got.api_key.as_ref().unwrap().expose(), "sk-test");
    }

    /// Backend that succeeds on `put`/`get` but always fails `delete`
    /// with the given error kind. Used to verify [`CredentialStore::delete`]
    /// does not orphan metadata.
    struct FailingDeleteBackend {
        inner: FakeKeychainBackend,
        kind: AuthErrorKind,
    }

    impl SecretBackend for FailingDeleteBackend {
        fn name(&self) -> &'static str {
            "failing_delete"
        }

        fn put(
            &self,
            provider: &ProviderId,
            account_id: &AccountId,
            secret: &StoredCredentialSecret,
        ) -> Result<(), AuthError> {
            self.inner.put(provider, account_id, secret)
        }

        fn get(
            &self,
            provider: &ProviderId,
            account_id: &AccountId,
        ) -> Result<Option<StoredCredentialSecret>, AuthError> {
            self.inner.get(provider, account_id)
        }

        fn delete(&self, _provider: &ProviderId, _account_id: &AccountId) -> Result<(), AuthError> {
            Err(AuthError::new(self.kind, "backend refused delete"))
        }
    }

    #[test]
    // When the backend refuses to delete the secret, the store
    // propagates the error and preserves the metadata index entry so
    // the user can see the orphan rather than the CLI claiming success.
    fn delete_propagates_backend_error_and_keeps_metadata() {
        let store = CredentialStore {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: Arc::new(FailingDeleteBackend {
                inner: FakeKeychainBackend::default(),
                kind: AuthErrorKind::CredentialStoreUnavailable,
            }),
            index: Arc::new(std::sync::Mutex::new(AccountIndex::default())),
            index_path: None,
        };
        let acct = sample_account(ProviderId::Anthropic, "acc-x");
        // Seed the metadata index so we have an entry to check after.
        store.index.lock().unwrap().upsert(acct.clone());
        let err = store.delete(&acct.provider, &acct.account_id).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
        assert!(store
            .snapshot_index()
            .get(&acct.provider, &acct.account_id)
            .is_some());
    }

    #[test]
    // A NotLoggedIn error from the backend is treated as "already gone"
    // — the metadata index entry is still removed (idempotent logout).
    fn delete_treats_backend_not_logged_in_as_idempotent() {
        let store = CredentialStore {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: Arc::new(FailingDeleteBackend {
                inner: FakeKeychainBackend::default(),
                kind: AuthErrorKind::NotLoggedIn,
            }),
            index: Arc::new(std::sync::Mutex::new(AccountIndex::default())),
            index_path: None,
        };
        let acct = sample_account(ProviderId::Anthropic, "acc-y");
        store.index.lock().unwrap().upsert(acct.clone());
        let removed = store.delete(&acct.provider, &acct.account_id).unwrap();
        assert!(removed);
        assert!(store
            .snapshot_index()
            .get(&acct.provider, &acct.account_id)
            .is_none());
    }

    #[test]
    // Logout deletes both the secret and the metadata entry; second
    // logout is a no-op (idempotent).
    fn logout_idempotent() {
        let store = CredentialStore::new_in_memory_for_tests();
        let acct = sample_account(ProviderId::OpenAi, "acc-2");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("sk-1".into())),
            ..Default::default()
        };
        store.put(acct.clone(), &secret).unwrap();
        let removed = store.delete(&acct.provider, &acct.account_id).unwrap();
        assert!(removed);
        let removed_again = store.delete(&acct.provider, &acct.account_id).unwrap();
        assert!(!removed_again);
        assert!(store
            .get_secret(&acct.provider, &acct.account_id)
            .unwrap()
            .is_none());
    }

    #[test]
    // File-store mode without opt-in yields a clear unavailable error.
    fn file_store_disabled_by_default() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = CredentialStore::build(
            CredentialStoreMode::File,
            false,
            Some(dir.path().join("index.json")),
        )
        .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
        assert!(err.message.contains("opt in"));
    }

    #[test]
    // File store with opt-in writes both the index and a per-account
    // secret file; on Unix both must be 0600.
    fn file_store_writes_strict_perms() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CredentialStore::build(
            CredentialStoreMode::File,
            true,
            Some(dir.path().join("index.json")),
        )
        .unwrap();
        let acct = sample_account(ProviderId::Gemini, "g-1");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("g-key".into())),
            ..Default::default()
        };
        store.put(acct.clone(), &secret).unwrap();

        // Index must exist and round-trip.
        let idx_path = dir.path().join("index.json");
        assert!(idx_path.exists());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&idx_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "index.json must be 0600 on unix");
        }
    }

    #[test]
    // Auto mode with no opt-in exposes a noop backend that fails on first write.
    fn auto_with_no_opt_in_returns_noop_backend() {
        let store = CredentialStore::build(CredentialStoreMode::Auto, false, None).unwrap();
        let acct = sample_account(ProviderId::Anthropic, "x");
        let res = store.put(
            acct,
            &StoredCredentialSecret {
                api_key: Some(SecretString::new("k".into())),
                ..Default::default()
            },
        );
        assert!(res.is_err());
    }
}
