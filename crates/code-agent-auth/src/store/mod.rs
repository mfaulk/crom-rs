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

use fs4::FileExt;
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
///
/// Read-modify-write cycles against `index_path` (`put` / `delete` /
/// `set_active`) are serialized at two levels:
///
/// * In-process: the `Mutex<AccountIndex>` serializes callers that share
///   this `CredentialStore` (or any clone of it) and guards the in-memory
///   index snapshot.
/// * Cross-process: an exclusive advisory lock (`fs4` / `flock`) on a
///   sibling `<index_path>.lock` file is held across the whole cycle.
///   While holding it the store re-reads the on-disk index so it mutates
///   the *current* persisted state rather than a snapshot captured at
///   `build` time — without this a second process (e.g. the CLI and the
///   TUI side by side) could clobber an update committed in between. The
///   lock is released on drop.
///
/// The write itself still uses the atomic temp-file + rename in
/// `file::write_secret_file` with `0o600` perms, so the lock only
/// serializes the read-modify-write; it does not change the on-disk
/// format or durability guarantees.
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

    /// Acquire an exclusive cross-process advisory lock on
    /// `<index_path>.lock`, held until the returned guard is dropped.
    /// Returns `Ok(None)` for the in-memory (`index_path == None`) store,
    /// where there is no on-disk file to race over.
    fn lock_index_file(&self) -> Result<Option<IndexFileLock>, AuthError> {
        let Some(path) = self.index_path.as_ref() else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("create index dir {}: {}", parent.display(), e),
                )
            })?;
        }
        let lock_path = lock_path_for(path);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("open index lock {}: {}", lock_path.display(), e),
                )
            })?;
        // UFCS through `fs4::FileExt` to disambiguate from the inherent
        // `std::fs::File::lock` stabilized in Rust 1.89: `unstable_name_collisions`
        // is denied under `-D warnings`, and the inherent method would otherwise
        // silently shadow fs4's on newer toolchains.
        FileExt::lock(&file).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("lock index {}: {}", lock_path.display(), e),
            )
        })?;
        Ok(Some(IndexFileLock { file: Some(file) }))
    }

    /// Re-read the on-disk index into the in-memory copy. Must be called
    /// while holding both the in-process mutex (`idx`) and the
    /// cross-process flock, so the subsequent read-modify-write applies to
    /// the freshest persisted state rather than the snapshot captured at
    /// `build` time. A missing or unparsable index is treated as empty —
    /// matching `build`'s best-effort load — but only when no on-disk
    /// index file exists; if the file is present we must not silently drop
    /// another process's committed accounts.
    fn reload_index_into(&self, idx: &mut AccountIndex) -> Result<(), AuthError> {
        let Some(path) = self.index_path.as_ref() else {
            return Ok(());
        };
        if !path.exists() {
            return Ok(());
        }
        let bytes = std::fs::read(path).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("read account index at {}: {}", path.display(), e),
            )
        })?;
        // Preserve the existing build-time tolerance for a corrupt index
        // (treat as default) so a partially written file from an older
        // version does not hard-fail an otherwise valid write.
        *idx = serde_json::from_slice::<AccountIndex>(&bytes).unwrap_or_default();
        Ok(())
    }

    pub fn put(
        &self,
        account: StoredAccount,
        secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        let provider = account.provider.clone();
        let account_id = account.account_id.clone();
        self.backend.put(&provider, &account_id, secret)?;
        let mut idx = self.index.lock().unwrap();

        // Capture whether this key was already known *before* we touch the
        // index, so a rollback can tell a new account (orphaned secret →
        // delete, #222) from an update (secret still reachable → keep).
        // This is read from the in-memory index, which reflects the prior
        // committed state shared by every clone of this store, and is
        // captured here so it is available even if the lock cannot be
        // acquired below.
        let was_update = idx.get(&provider, &account_id).is_some();

        // Run the locked read-modify-write. The lock guard is bound inside
        // the block so it is held across the reload + persist and released
        // when the block returns.
        let result = (|| -> Result<(), AuthError> {
            let _file_lock = self.lock_index_file()?;
            // Re-sync the in-memory index with the current persisted state
            // under the lock so we don't clobber another process's
            // committed accounts when we persist below.
            self.reload_index_into(&mut idx)?;
            idx.upsert(account);
            self.persist_index(&idx)
        })();

        if let Err(e) = result {
            // The secret is committed to the backend but the locked index
            // update failed (lock, reload, or persist). Roll back so we
            // neither orphan a new secret nor destroy an existing one, then
            // surface the original error.
            if !was_update {
                let _ = self.backend.delete(&provider, &account_id);
            }
            // Restore the in-memory index to match what is on disk.
            self.reload_index_into(&mut idx).ok();
            return Err(e);
        }
        Ok(())
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
        let _file_lock = self.lock_index_file()?;
        let mut idx = self.index.lock().unwrap();
        // Re-sync with on-disk state under the lock so removing one
        // account does not drop another process's concurrently added one.
        self.reload_index_into(&mut idx)?;
        let removed = idx.remove(provider, account_id).is_some();
        self.persist_index(&idx)?;
        Ok(removed)
    }

    pub fn set_active(
        &self,
        provider: &ProviderId,
        account_id: AccountId,
    ) -> Result<(), AuthError> {
        let _file_lock = self.lock_index_file()?;
        let mut idx = self.index.lock().unwrap();
        // Re-sync with on-disk state under the lock so changing the active
        // selection preserves accounts another process committed.
        self.reload_index_into(&mut idx)?;
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
        file::write_secret_file(path, &bytes)?;
        Ok(())
    }
}

/// Path of the sibling advisory-lock file for an index at `path`.
/// Kept separate from the index file itself so the `flock` never sits on
/// a file that the atomic temp-file + rename writes replace out from
/// under it.
fn lock_path_for(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

/// RAII guard for the exclusive cross-process advisory lock on the
/// credential index's `.lock` file. Released when dropped: `unlock` is
/// best-effort and, even if it fails, closing the descriptor releases the
/// underlying `flock`.
struct IndexFileLock {
    file: Option<std::fs::File>,
}

impl Drop for IndexFileLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = FileExt::unlock(&file);
        }
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
    // If index persistence fails after the secret is written to the
    // backend, `put` best-effort rolls back the backend write so no
    // orphaned secret is left behind, and surfaces the original error.
    fn put_rolls_back_secret_when_index_persist_fails() {
        let dir = tempfile::TempDir::new().unwrap();
        // Make `index_path`'s parent a regular file so `create_dir_all`
        // (and thus persist_index) fails while the backend write succeeds.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let index_path = blocker.join("index.json");

        let backend = Arc::new(FakeKeychainBackend::default());
        let store = CredentialStore {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: backend.clone(),
            index: Arc::new(std::sync::Mutex::new(AccountIndex::default())),
            index_path: Some(index_path),
        };
        let acct = sample_account(ProviderId::Anthropic, "orphan-check");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("sk-orphan".into())),
            ..Default::default()
        };
        let err = store.put(acct.clone(), &secret).unwrap_err();
        // Original persistence error is surfaced.
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
        // The secret must not be orphaned in the backend.
        assert!(
            backend
                .get(&acct.provider, &acct.account_id)
                .unwrap()
                .is_none(),
            "secret should have been rolled back, not orphaned"
        );
    }

    #[test]
    // #222 regression: rollback must only remove a *newly orphaned* secret.
    // `put` is also used for updates (e.g. an OAuth token refresh); on an
    // update the on-disk index still references the key, so the freshly
    // written secret is reachable and must survive a failed index persist
    // rather than being deleted (which would destroy a valid credential).
    fn put_keeps_existing_secret_when_index_persist_fails_on_update() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = Arc::new(FakeKeychainBackend::default());
        let index = Arc::new(std::sync::Mutex::new(AccountIndex::default()));
        let acct = sample_account(ProviderId::Anthropic, "refresh-me");

        // Phase 1: seed an existing account whose index persists cleanly.
        let store_ok = CredentialStore {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: backend.clone(),
            index: index.clone(),
            index_path: Some(dir.path().join("index.json")),
        };
        let old_secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("sk-old".into())),
            ..Default::default()
        };
        store_ok.put(acct.clone(), &old_secret).unwrap();

        // Phase 2: an *update* to the same account whose index persist fails
        // (the parent of `index_path` is a regular file, so create_dir_all
        // errors). Shares the same backend + in-memory index as phase 1.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let store_bad = CredentialStore {
            mode: CredentialStoreMode::Auto,
            allow_file_store: true,
            backend: backend.clone(),
            index: index.clone(),
            index_path: Some(blocker.join("index.json")),
        };
        let new_secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("sk-new".into())),
            ..Default::default()
        };
        let err = store_bad.put(acct.clone(), &new_secret).unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);

        // The credential must NOT be destroyed by the failed update — and it
        // reflects the freshly written value (still referenced by the
        // unchanged on-disk index, so it is reachable rather than orphaned).
        let got = backend
            .get(&acct.provider, &acct.account_id)
            .unwrap()
            .expect("an existing secret must survive a failed index persist on update");
        assert_eq!(got.api_key.as_ref().unwrap().expose(), "sk-new");
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

    #[test]
    // Two threads concurrently writing different accounts through a
    // shared `CredentialStore` must not lose updates: the in-process
    // mutex serializes the read-modify-write of the on-disk index, and
    // both accounts must be present after both joins.
    fn concurrent_puts_do_not_lose_updates() {
        let dir = tempfile::TempDir::new().unwrap();
        let store = CredentialStore::build(
            CredentialStoreMode::File,
            true,
            Some(dir.path().join("index.json")),
        )
        .unwrap();
        let a = sample_account(ProviderId::Anthropic, "concurrent-a");
        let b = sample_account(ProviderId::OpenAi, "concurrent-b");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("k".into())),
            ..Default::default()
        };

        let s1 = store.clone();
        let s2 = store.clone();
        let a_clone = a.clone();
        let b_clone = b.clone();
        let secret1 = secret.clone();
        let secret2 = secret.clone();
        let t1 = std::thread::spawn(move || s1.put(a_clone, &secret1).unwrap());
        let t2 = std::thread::spawn(move || s2.put(b_clone, &secret2).unwrap());
        t1.join().unwrap();
        t2.join().unwrap();

        // Reload from disk to ensure the persisted index — not just the
        // in-memory copy — contains both accounts.
        let reloaded = CredentialStore::build(
            CredentialStoreMode::File,
            true,
            Some(dir.path().join("index.json")),
        )
        .unwrap();
        let snap = reloaded.snapshot_index();
        assert!(
            snap.get(&a.provider, &a.account_id).is_some(),
            "account A missing from reloaded index"
        );
        assert!(
            snap.get(&b.provider, &b.account_id).is_some(),
            "account B missing from reloaded index"
        );
    }

    // Env-var protocol for the cross-process index-lock test.
    const XPROC_INDEX: &str = "AUTH_STORE_XPROC_INDEX";
    const XPROC_ID: &str = "AUTH_STORE_XPROC_ID";
    const XPROC_CHILD: &str = "AUTH_STORE_XPROC_CHILD";

    #[test]
    // Cross-process serialization: several OS processes each `put` a
    // distinct account into the SAME on-disk index concurrently. The
    // `<index>.lock` advisory flock plus the under-lock reload must make
    // every put a read-modify-write against the current persisted index,
    // so all accounts survive with no lost updates. (Without the lock the
    // last writer would clobber the others — each loaded its snapshot at
    // `build` time.)
    fn concurrent_puts_across_processes_do_not_lose_updates() {
        // Worker branch: re-exec'd by the parent with env vars set, it
        // performs a single `put` of its assigned account and exits.
        if std::env::var(XPROC_CHILD).is_ok() {
            let index_path = std::path::PathBuf::from(std::env::var(XPROC_INDEX).unwrap());
            let id = std::env::var(XPROC_ID).unwrap();
            let store =
                CredentialStore::build(CredentialStoreMode::File, true, Some(index_path)).unwrap();
            let acct = sample_account(ProviderId::Anthropic, &id);
            let secret = StoredCredentialSecret {
                api_key: Some(SecretString::new("k".into())),
                ..Default::default()
            };
            store.put(acct, &secret).unwrap();
            std::process::exit(0);
        }

        let dir = tempfile::TempDir::new().unwrap();
        let index_path = dir.path().join("index.json");
        // Materialize the store/index/secrets dir once up front so the
        // children share an already-initialized layout.
        let _seed =
            CredentialStore::build(CredentialStoreMode::File, true, Some(index_path.clone()))
                .unwrap();

        let procs: u32 = 6;
        let exe = std::env::current_exe().unwrap();
        let ids: Vec<String> = (0..procs).map(|i| format!("acc-{i}")).collect();
        let mut children = Vec::new();
        for id in &ids {
            let child = std::process::Command::new(&exe)
                .args([
                    "--exact",
                    "store::tests::concurrent_puts_across_processes_do_not_lose_updates",
                    "--nocapture",
                ])
                .env(XPROC_CHILD, "1")
                .env(XPROC_INDEX, &index_path)
                .env(XPROC_ID, id)
                .spawn()
                .expect("spawn worker process");
            children.push(child);
        }
        for mut child in children {
            let status = child.wait().expect("wait for worker");
            assert!(status.success(), "worker process failed: {status:?}");
        }

        // Every account each process wrote must be present in the
        // persisted index.
        let reloaded =
            CredentialStore::build(CredentialStoreMode::File, true, Some(index_path)).unwrap();
        let snap = reloaded.snapshot_index();
        for id in &ids {
            assert!(
                snap.get(&ProviderId::Anthropic, &AccountId::new(id))
                    .is_some(),
                "account {id} missing from reloaded index — lost update across processes"
            );
        }
    }
}
