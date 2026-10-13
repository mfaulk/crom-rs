//! On-disk JSON file credential backend. Opt-in.
//!
//! The directory layout is:
//!
//! ```text
//! <secrets_dir>/<provider>/<account_id_hash>.json
//! ```
//!
//! On Unix the directory is created with `0700` and per-account files
//! with `0600`. The hashed filename keeps the on-disk listing free of
//! identifiable account ids while still producing a stable lookup key.
//! The hash input is `sha256(<provider_id> || 0x00 || <account_id>)` so
//! the same email under two providers maps to two different filenames,
//! making a single rainbow table built from a list of plain emails
//! useless. A truly random per-store salt is tracked as a follow-up.
//!
//! Before the salted layout, the filename was the unsalted
//! `acct_<sha256(account_id)[..12]>.json`. To avoid silently logging
//! existing users out across that upgrade, `get` and `delete` fall back
//! to the legacy unsalted filename when the salted file is absent, and
//! `put` removes the legacy file after writing the salted file so the
//! store self-migrates on the next credential refresh or login.
//!
//! Writes go through a sibling temp file in the same directory and are
//! renamed over the target via `tempfile::persist`, so concurrent
//! readers never observe a torn write and a crash mid-write leaves the
//! previous payload intact. On Unix the parent directory is `fsync`ed
//! after the rename so the new directory entry is durable across power
//! loss; on other platforms only the file payload is fsynced before the
//! rename.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AuthError, AuthErrorKind};
use crate::types::{AccountId, ProviderId, SecretString, StoredCredentialSecret};

use super::SecretBackend;

pub struct FileCredentialBackend {
    dir: PathBuf,
}

impl FileCredentialBackend {
    pub fn new(dir: PathBuf) -> Result<Self, AuthError> {
        std::fs::create_dir_all(&dir).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("create secrets dir {}: {}", dir.display(), e),
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }

        // After create_dir_all returns the directory still needs to be
        // audited: a pre-existing symlink that pointed at an
        // attacker-controlled directory, or an entry owned by another
        // uid, would otherwise capture the next secret we write.
        verify_secrets_dir(&dir)?;

        Ok(Self { dir })
    }

    fn account_path(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<PathBuf, AuthError> {
        let provider_dir = self.provider_dir(provider)?;
        // Mix the provider id into the hash as a poor-man's salt so a
        // directory listing across providers cannot be cracked with a
        // single rainbow table keyed on plain email. A truly random
        // per-store salt (persisted as `.salt`, 0o600) is tracked as a
        // follow-up — the provider-id salt is zero-migration and
        // strictly better than the previous unsalted hash.
        Ok(provider_dir.join(format!(
            "{}.json",
            account_id.redacted_hash_with_salt(provider.as_str())
        )))
    }

    /// Path the previous (pre-salt) layout would have used for
    /// `(provider, account_id)`. Read-only fallback: used by `get` and
    /// `delete` so an upgrade from the unsalted layout does not silently
    /// log the user out. `put` migrates the file to the new path on
    /// next write by writing to [`Self::account_path`] and removing the
    /// legacy path.
    fn legacy_account_path(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<PathBuf, AuthError> {
        let provider_dir = self.provider_dir(provider)?;
        Ok(provider_dir.join(format!("{}.json", account_id.redacted_hash())))
    }

    fn provider_dir(&self, provider: &ProviderId) -> Result<PathBuf, AuthError> {
        let provider_dir = self.dir.join(provider.as_str());
        // Create the per-provider subdirectory and then audit it. The
        // audit catches the case where the parent is fine but a
        // provider subdirectory was pre-planted as a symlink to an
        // attacker-controlled location between `new` and this call.
        std::fs::create_dir_all(&provider_dir).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("create provider dir {}: {}", provider_dir.display(), e),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&provider_dir, std::fs::Permissions::from_mode(0o700));
        }
        verify_secrets_dir(&provider_dir)?;
        Ok(provider_dir)
    }
}

/// Verify that `dir` is a real directory the current user owns and that
/// no other user has write access to. Rejects symlinks (regardless of
/// where they point) so a pre-planted symlink cannot redirect secret
/// writes to an attacker-controlled location. On Unix the mode is
/// forced to `0o700` if it is not already, and ownership is checked
/// against the current uid. The check is a no-op on non-Unix targets
/// because the file-store path is mostly used in CI and local dev there;
/// the keychain is preferred elsewhere.
fn verify_secrets_dir(dir: &Path) -> Result<(), AuthError> {
    let meta = std::fs::symlink_metadata(dir).map_err(|e| {
        AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!("stat secrets dir {}: {}", dir.display(), e),
        )
    })?;
    if meta.file_type().is_symlink() {
        return Err(AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!(
                "refusing to use secrets dir {}: path is a symlink",
                dir.display()
            ),
        ));
    }
    if !meta.file_type().is_dir() {
        return Err(AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!(
                "refusing to use secrets dir {}: not a directory",
                dir.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: libc::getuid is always-safe (returns the real uid of
        // the calling process; cannot fail). We avoid pulling in `nix`
        // just for this one call.
        let current_uid = unsafe { libc::getuid() };
        let dir_uid = meta.uid();
        if dir_uid != current_uid {
            return Err(AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!(
                    "refusing to use secrets dir {}: owned by uid {} but current uid is {}",
                    dir.display(),
                    dir_uid,
                    current_uid
                ),
            ));
        }
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o700 {
            // Try to tighten in place — common case is an inherited
            // 0o755 from `create_dir_all` on a system with a tight
            // umask. If chmod itself fails the directory is unsafe.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!(
                        "refusing to use secrets dir {}: mode is {:o} (expected 0o700) and chmod failed: {}",
                        dir.display(),
                        mode,
                        e,
                    ),
                )
            })?;
        }
    }
    Ok(())
}

#[derive(Serialize, Deserialize, Default)]
struct OnDiskSecret {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

impl SecretBackend for FileCredentialBackend {
    fn name(&self) -> &'static str {
        "file_store"
    }

    fn put(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
        secret: &StoredCredentialSecret,
    ) -> Result<(), AuthError> {
        let path = self.account_path(provider, account_id)?;
        let dst = OnDiskSecret {
            access_token: secret.access_token.as_ref().map(|s| s.expose().to_string()),
            refresh_token: secret
                .refresh_token
                .as_ref()
                .map(|s| s.expose().to_string()),
            api_key: secret.api_key.as_ref().map(|s| s.expose().to_string()),
            id_token: secret.id_token.as_ref().map(|s| s.expose().to_string()),
        };
        let bytes = serde_json::to_vec(&dst).map_err(|e| {
            AuthError::new(AuthErrorKind::Unknown, format!("serialize secret: {}", e))
        })?;
        write_secret_file(&path, &bytes)?;
        // Migrate-on-write: if a legacy (unsalted) record exists for the
        // same (provider, account_id) tuple, remove it now that the
        // canonical salted file has been written. Tolerate a missing
        // legacy file — the migration is best-effort.
        let legacy = self.legacy_account_path(provider, account_id)?;
        if legacy != path && legacy.exists() {
            let _ = std::fs::remove_file(&legacy);
        }
        Ok(())
    }

    fn get(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError> {
        let path = self.account_path(provider, account_id)?;
        let resolved = if path.exists() {
            path
        } else {
            // Fallback: an upgrade across the unsalted-to-salted layout
            // would otherwise read `None` here and quietly log the user
            // out. Try the legacy filename so existing on-disk records
            // remain reachable. `put` migrates to the new path next time
            // a write happens.
            let legacy = self.legacy_account_path(provider, account_id)?;
            if legacy.exists() {
                legacy
            } else {
                return Ok(None);
            }
        };
        let bytes = std::fs::read(&resolved).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("read secret {}: {}", resolved.display(), e),
            )
        })?;
        let on_disk: OnDiskSecret = serde_json::from_slice(&bytes)
            .map_err(|e| AuthError::new(AuthErrorKind::Unknown, format!("parse secret: {}", e)))?;
        Ok(Some(StoredCredentialSecret {
            access_token: on_disk.access_token.map(SecretString::new),
            refresh_token: on_disk.refresh_token.map(SecretString::new),
            api_key: on_disk.api_key.map(SecretString::new),
            id_token: on_disk.id_token.map(SecretString::new),
        }))
    }

    fn delete(&self, provider: &ProviderId, account_id: &AccountId) -> Result<(), AuthError> {
        let path = self.account_path(provider, account_id)?;
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("remove secret {}: {}", path.display(), e),
                )
            })?;
        }
        // Also clear any legacy unsalted record so a stale credential is
        // not left behind on disk after a logout.
        let legacy = self.legacy_account_path(provider, account_id)?;
        if legacy != path && legacy.exists() {
            std::fs::remove_file(&legacy).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("remove legacy secret {}: {}", legacy.display(), e),
                )
            })?;
        }
        Ok(())
    }
}

/// Atomically write `bytes` to `path` with strict permissions.
///
/// The write goes to a sibling temp file in the same directory and is
/// then `persist`ed (renamed) over `path`. Concurrent readers either
/// see the previous contents or the new contents — never a torn write —
/// and a crash mid-write leaves the canonical path untouched.
///
/// On Unix the temp file is created with mode `0o600` so the target
/// also has `0o600` after the rename, with no window where it would be
/// world-readable under a default umask. After the rename the parent
/// directory itself is opened and `sync_all`ed so the new directory
/// entry is durable across power loss — without this step the rename
/// can return success but the entry may be lost on crash. Windows does
/// not expose (or need) a directory fsync for this guarantee, so the
/// directory sync is `#[cfg(unix)]`-gated.
pub(crate) fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let io_err = |e: std::io::Error| {
        AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!("write secret {}: {}", path.display(), e),
        )
    };
    let dir = path.parent().ok_or_else(|| {
        AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!("secret path {} has no parent directory", path.display()),
        )
    })?;
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(io_err)?;
    }

    let mut builder = tempfile::Builder::new();
    builder.prefix(".tmp-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        builder.permissions(std::fs::Permissions::from_mode(0o600));
    }
    let mut tmp = builder.tempfile_in(dir).map_err(io_err)?;

    use std::io::Write as _;
    tmp.write_all(bytes).map_err(io_err)?;
    tmp.as_file().sync_data().map_err(io_err)?;
    tmp.persist(path).map_err(|e| io_err(e.error))?;

    // Persist the rename itself: without an fsync on the parent
    // directory the new directory entry may not survive a crash even
    // though `persist` returned `Ok`.
    #[cfg(unix)]
    {
        let dir_file = std::fs::File::open(dir).map_err(io_err)?;
        dir_file.sync_all().map_err(io_err)?;
    }

    Ok(())
}

/// Pick a default secrets directory next to the account index file. If
/// `index_path` is `None`, the secrets directory defaults to
/// `<XDG_DATA_HOME or ~/.local/share>/crom/secrets`.
pub fn default_secrets_dir(index_path: Option<&Path>) -> Result<PathBuf, AuthError> {
    if let Some(p) = index_path {
        let parent = p
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        return Ok(parent.join("secrets"));
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("crom").join("secrets"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("crom")
            .join("secrets"));
    }
    Err(AuthError::new(
        AuthErrorKind::CredentialStoreUnavailable,
        "Cannot determine secrets directory: HOME and XDG_DATA_HOME both unset.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // File backend writes and reads back exact secret bytes, even when
    // multiple secret fields are present.
    fn file_backend_roundtrips_multiple_fields() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Codex;
        let acct = AccountId::new("u-1");
        let secret = StoredCredentialSecret {
            access_token: Some(SecretString::new("at".into())),
            refresh_token: Some(SecretString::new("rt".into())),
            api_key: Some(SecretString::new("ak".into())),
            id_token: Some(SecretString::new("id".into())),
        };
        backend.put(&provider, &acct, &secret).unwrap();
        let got = backend.get(&provider, &acct).unwrap().unwrap();
        assert_eq!(got.access_token.as_ref().unwrap().expose(), "at");
        assert_eq!(got.refresh_token.as_ref().unwrap().expose(), "rt");
        assert_eq!(got.api_key.as_ref().unwrap().expose(), "ak");
        assert_eq!(got.id_token.as_ref().unwrap().expose(), "id");
    }

    #[test]
    // The on-disk filename for the same account id under two different
    // providers must differ, so a directory listing cannot be cracked
    // with a single rainbow table keyed on a plain email. This is the
    // observable consequence of mixing the provider id into the hash
    // input as a poor-man's salt.
    fn filename_differs_across_providers_for_same_account_id() {
        let dir1 = tempfile::TempDir::new().unwrap();
        let dir2 = tempfile::TempDir::new().unwrap();
        let backend1 = FileCredentialBackend::new(dir1.path().to_path_buf()).unwrap();
        let backend2 = FileCredentialBackend::new(dir2.path().to_path_buf()).unwrap();
        let acct = AccountId::new("user@example.com");
        let secret = StoredCredentialSecret {
            api_key: Some(SecretString::new("k".into())),
            ..Default::default()
        };
        backend1
            .put(&ProviderId::Anthropic, &acct, &secret)
            .unwrap();
        backend2.put(&ProviderId::OpenAi, &acct, &secret).unwrap();
        let name1 = std::fs::read_dir(dir1.path().join(ProviderId::Anthropic.as_str()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        let name2 = std::fs::read_dir(dir2.path().join(ProviderId::OpenAi.as_str()))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        assert_ne!(
            name1, name2,
            "same email under two providers must hash to different filenames"
        );
        assert!(name1.starts_with("acct_"));
        assert!(name2.starts_with("acct_"));
    }

    #[test]
    // Filename uses the redacted hash, not the raw account id, so a
    // directory listing does not reveal user identifiers.
    fn filename_uses_hashed_account_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::OpenAi;
        let acct = AccountId::new("user@example.com");
        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("k".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let listed = std::fs::read_dir(dir.path().join(provider.as_str())).unwrap();
        for entry in listed {
            let name = entry.unwrap().file_name().to_string_lossy().to_string();
            assert!(name.starts_with("acct_"), "filename {name} not redacted");
            assert!(!name.contains("user"));
            assert!(!name.contains("example"));
        }
    }

    #[cfg(unix)]
    #[test]
    // Per-account secret files end up at mode 0o600 because the temp
    // file is created with that mode before being renamed over the
    // canonical path, so there is no window during which the file
    // would be readable by other users under a default umask.
    fn secret_file_is_strictly_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Anthropic;
        let acct = AccountId::new("strict-perm");
        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("k".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let path = dir.path().join(provider.as_str()).join(format!(
            "{}.json",
            acct.redacted_hash_with_salt(provider.as_str())
        ));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be 0o600 on unix");
    }

    #[test]
    // `get` falls back to the legacy unsalted filename when no
    // salted-layout file exists, so upgrading across the salting
    // change does not silently log existing users out.
    fn get_falls_back_to_legacy_unsalted_filename() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Anthropic;
        let acct = AccountId::new("legacy-user@example.com");
        // Manually pre-populate the directory with a legacy unsalted
        // record under the OLD filename, exactly as a previous version
        // of the backend would have written it.
        let provider_dir = dir.path().join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).unwrap();
        let legacy_path = provider_dir.join(format!("{}.json", acct.redacted_hash()));
        let payload = br#"{"api_key":"legacy-secret"}"#;
        std::fs::write(&legacy_path, payload).unwrap();

        let got = backend
            .get(&provider, &acct)
            .unwrap()
            .expect("legacy record should be visible to get()");
        assert_eq!(got.api_key.as_ref().unwrap().expose(), "legacy-secret");
    }

    #[test]
    // `put` migrates an existing legacy unsalted file to the new
    // salted layout by writing the new path and removing the old one
    // on the next write.
    fn put_migrates_legacy_unsalted_file_to_salted_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Anthropic;
        let acct = AccountId::new("migrating@example.com");
        let provider_dir = dir.path().join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).unwrap();
        let legacy_path = provider_dir.join(format!("{}.json", acct.redacted_hash()));
        let salted_path = provider_dir.join(format!(
            "{}.json",
            acct.redacted_hash_with_salt(provider.as_str())
        ));
        assert_ne!(legacy_path, salted_path);
        std::fs::write(&legacy_path, br#"{"api_key":"old"}"#).unwrap();

        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("new".into())),
                    ..Default::default()
                },
            )
            .unwrap();

        assert!(salted_path.exists(), "salted file should be written");
        assert!(
            !legacy_path.exists(),
            "legacy file should be removed after migration write"
        );
    }

    #[test]
    // `delete` removes a legacy unsalted file as well, so a logout
    // does not leave a stale credential behind on disk during the
    // upgrade window.
    fn delete_removes_legacy_unsalted_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::OpenAi;
        let acct = AccountId::new("deleting@example.com");
        let provider_dir = dir.path().join(provider.as_str());
        std::fs::create_dir_all(&provider_dir).unwrap();
        let legacy_path = provider_dir.join(format!("{}.json", acct.redacted_hash()));
        std::fs::write(&legacy_path, br#"{"api_key":"x"}"#).unwrap();

        backend.delete(&provider, &acct).unwrap();
        assert!(!legacy_path.exists());
    }

    #[test]
    // Delete is idempotent.
    fn file_backend_delete_idempotent() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Anthropic;
        let acct = AccountId::new("a");
        backend.delete(&provider, &acct).unwrap();
        backend.delete(&provider, &acct).unwrap();
    }

    #[test]
    // A successful `put` leaves only the canonical secret file in the
    // provider directory — no `.tmp-*` debris from `tempfile::persist`,
    // confirming the temp-and-rename flow ran end to end.
    fn put_leaves_no_temp_file_debris() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::OpenAi;
        let acct = AccountId::new("debris");
        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("k".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let canonical = format!("{}.json", acct.redacted_hash_with_salt(provider.as_str()));
        let entries: Vec<String> = std::fs::read_dir(dir.path().join(provider.as_str()))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec![canonical],
            "expected only the canonical secret file"
        );
    }

    #[test]
    // Two writes to the same secret never produce a torn read: after a
    // successful overwrite, readers either see the previous payload or
    // the new one in full — verified here by reading immediately after
    // the second put and asserting it matches the second payload.
    fn overwrite_is_atomic() {
        let dir = tempfile::TempDir::new().unwrap();
        let backend = FileCredentialBackend::new(dir.path().to_path_buf()).unwrap();
        let provider = ProviderId::Anthropic;
        let acct = AccountId::new("overwrite");
        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("first".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        backend
            .put(
                &provider,
                &acct,
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("second".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let got = backend.get(&provider, &acct).unwrap().unwrap();
        assert_eq!(got.api_key.as_ref().unwrap().expose(), "second");
    }

    // True crash-durability cannot be observed from a process-level
    // unit test — verifying that the directory entry survives a real
    // power loss would require either kernel-level fault injection or
    // a refactor that mocks the sync call (per codex P1 review). The
    // test below is a pragmatic sanity check: it drives
    // `write_secret_file` end to end against a real tempdir and
    // asserts the call returns `Ok`, which is enough to confirm the
    // post-rename parent-directory `sync_all` path is exercised on
    // Unix without panicking or returning an error on a normal
    // filesystem. A regression that removed the directory sync would
    // not be caught here directly, but the doc comment on
    // `write_secret_file` documents the guarantee and the codex
    // finding.
    #[test]
    fn write_secret_file_succeeds_against_real_tempdir() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("secret.json");
        write_secret_file(&path, b"{\"api_key\":\"k\"}").unwrap();
        let read_back = std::fs::read(&path).unwrap();
        assert_eq!(read_back, b"{\"api_key\":\"k\"}");
    }

    #[cfg(unix)]
    #[test]
    // The directory audit rejects a path that is a symlink, even one
    // pointing at a perfectly valid directory the current user owns.
    // This is the defense against a pre-planted symlink swap during
    // first-time creation of `<secrets_dir>/`.
    fn new_rejects_symlink_secrets_dir() {
        let parent = tempfile::TempDir::new().unwrap();
        let target = parent.path().join("real");
        std::fs::create_dir(&target).unwrap();
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = match FileCredentialBackend::new(link.clone()) {
            Ok(_) => panic!("expected symlink rejection"),
            Err(e) => e,
        };
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
        assert!(
            err.message.contains("symlink"),
            "unexpected error: {}",
            err.message
        );
    }

    #[cfg(unix)]
    #[test]
    // The directory audit chmods a too-permissive existing directory
    // back to 0o700 instead of failing outright, so a fresh secrets
    // dir created under a loose umask still ends up locked down.
    fn new_tightens_loose_directory_mode_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::TempDir::new().unwrap();
        let dir = parent.path().join("secrets");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = FileCredentialBackend::new(dir.clone()).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[cfg(unix)]
    #[test]
    // A per-provider subdirectory that turns into a symlink between
    // `new` and the first write is rejected at write time, so the
    // secret is never persisted to the attacker-controlled target.
    fn put_rejects_symlinked_provider_subdir() {
        let parent = tempfile::TempDir::new().unwrap();
        let dir = parent.path().join("secrets");
        std::fs::create_dir(&dir).unwrap();
        let backend = FileCredentialBackend::new(dir.clone()).unwrap();
        // Pre-plant a symlink at <dir>/<provider> pointing somewhere
        // outside the secrets root.
        let evil_target = parent.path().join("attacker-controlled");
        std::fs::create_dir(&evil_target).unwrap();
        std::os::unix::fs::symlink(&evil_target, dir.join(ProviderId::Anthropic.as_str())).unwrap();
        let err = backend
            .put(
                &ProviderId::Anthropic,
                &AccountId::new("a"),
                &StoredCredentialSecret {
                    api_key: Some(SecretString::new("k".into())),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert_eq!(err.kind, AuthErrorKind::CredentialStoreUnavailable);
        assert!(
            err.message.contains("symlink"),
            "unexpected error: {}",
            err.message
        );
        // And the attacker-controlled directory must still be empty:
        // the audit fired before any write.
        let attacker_entries: Vec<_> = std::fs::read_dir(&evil_target).unwrap().collect();
        assert!(attacker_entries.is_empty());
    }
}
