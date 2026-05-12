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

        Ok(Self { dir })
    }

    fn account_path(&self, provider: &ProviderId, account_id: &AccountId) -> PathBuf {
        let provider_dir = self.dir.join(provider.as_str());
        let _ = std::fs::create_dir_all(&provider_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&provider_dir, std::fs::Permissions::from_mode(0o700));
        }
        provider_dir.join(format!("{}.json", account_id.redacted_hash()))
    }
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
        let path = self.account_path(provider, account_id);
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
        Ok(())
    }

    fn get(
        &self,
        provider: &ProviderId,
        account_id: &AccountId,
    ) -> Result<Option<StoredCredentialSecret>, AuthError> {
        let path = self.account_path(provider, account_id);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path).map_err(|e| {
            AuthError::new(
                AuthErrorKind::CredentialStoreUnavailable,
                format!("read secret {}: {}", path.display(), e),
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
        let path = self.account_path(provider, account_id);
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| {
                AuthError::new(
                    AuthErrorKind::CredentialStoreUnavailable,
                    format!("remove secret {}: {}", path.display(), e),
                )
            })?;
        }
        Ok(())
    }
}

/// Write `bytes` to `path` with strict permissions.
///
/// On Unix the file is created with mode `0o600` from the start via
/// `OpenOptions::mode`, closing the window where the file would briefly
/// be world-readable under a default umask. The bytes are flushed with
/// `sync_data` before the handle is dropped. On non-Unix platforms this
/// falls back to `std::fs::write` (best-effort).
pub(crate) fn write_secret_file(path: &Path, bytes: &[u8]) -> Result<(), AuthError> {
    let io_err = |e: std::io::Error| {
        AuthError::new(
            AuthErrorKind::CredentialStoreUnavailable,
            format!("write secret {}: {}", path.display(), e),
        )
    };

    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(io_err)?;
        f.write_all(bytes).map_err(io_err)?;
        f.sync_data().map_err(io_err)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes).map_err(io_err)?;
    }
    Ok(())
}

/// Pick a default secrets directory next to the account index file. If
/// `index_path` is `None`, the secrets directory defaults to
/// `<XDG_DATA_HOME or ~/.local/share>/code-agent/secrets`.
pub fn default_secrets_dir(index_path: Option<&Path>) -> Result<PathBuf, AuthError> {
    if let Some(p) = index_path {
        let parent = p
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        return Ok(parent.join("secrets"));
    }
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        return Ok(PathBuf::from(dir).join("code-agent").join("secrets"));
    }
    if let Ok(home) = std::env::var("HOME") {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("code-agent")
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
    // Per-account secret files are created with mode 0o600 from the
    // start (via OpenOptions::mode), so there is no window during which
    // the file is readable by other users under a default umask.
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
        let path = dir
            .path()
            .join(provider.as_str())
            .join(format!("{}.json", acct.redacted_hash()));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be 0o600 on unix");
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
}
