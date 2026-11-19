use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::Deserialize;
use zeroize::Zeroizing;

use super::{BrokerError, CredentialBroker, CredentialLease};
use crate::ids::ActionId;
use crate::BoxFuture;

/// Largest credentials file accepted.
pub const MAX_CREDENTIALS_FILE_BYTES: u64 = 64 * 1024;

/// Why credentials could not be loaded. No variant carries secret material:
/// parse errors report only a position, because `serde_json` messages can
/// quote the offending value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum CredentialLoadError {
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("{path}: not a regular file (symlinks are refused)")]
    NotRegularFile { path: PathBuf },
    #[error("{path}: mode {mode:o} is accessible to group or others; expected 0600")]
    Permissions { path: PathBuf, mode: u32 },
    #[error("{path}: owned by uid {owner}, not Guard's uid {expected}")]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },
    #[error("{path}: larger than {MAX_CREDENTIALS_FILE_BYTES} bytes")]
    TooLarge { path: PathBuf },
    #[error("credentials are malformed at line {line}, column {column}")]
    Malformed { line: usize, column: usize },
    #[error("credential configuration invalid: {0}")]
    Invalid(String),
    #[error("environment variable {0} is not set or not valid UTF-8")]
    MissingEnv(String),
    #[error("file credentials are not supported on this platform; use EnvCredentialBroker")]
    Unsupported,
}

struct Credential {
    id: String,
    secret: Zeroizing<String>,
}

/// Action → credential map. Swapped whole on reload, so a lease always
/// comes from one consistent snapshot.
#[derive(Default)]
struct Table {
    by_action: BTreeMap<ActionId, Arc<Credential>>,
}

impl Table {
    fn build(
        entries: Vec<(String, Zeroizing<String>, Vec<ActionId>)>,
    ) -> Result<Self, CredentialLoadError> {
        let mut ids = std::collections::BTreeSet::new();
        let mut by_action = BTreeMap::new();
        for (index, (id, secret, actions)) in entries.into_iter().enumerate() {
            // Report the position, not the value: a misplaced secret in
            // `id` must not reach logs.
            if id.is_empty()
                || !id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
            {
                return Err(CredentialLoadError::Invalid(format!(
                    "credential #{index}: id is not [A-Za-z0-9._-]+"
                )));
            }
            if !ids.insert(id.clone()) {
                return Err(CredentialLoadError::Invalid(format!(
                    "duplicate credential id {id}"
                )));
            }
            if secret.is_empty() {
                return Err(CredentialLoadError::Invalid(format!(
                    "credential {id} has an empty secret"
                )));
            }
            if actions.is_empty() {
                return Err(CredentialLoadError::Invalid(format!(
                    "credential {id} names no actions"
                )));
            }
            let cred = Arc::new(Credential {
                id: id.clone(),
                secret,
            });
            for a in actions {
                if by_action.insert(a.clone(), cred.clone()).is_some() {
                    return Err(CredentialLoadError::Invalid(format!(
                        "action {a} has more than one credential"
                    )));
                }
            }
        }
        Ok(Self { by_action })
    }

    fn lease(&self, action: &ActionId) -> Result<CredentialLease, BrokerError> {
        let c = self
            .by_action
            .get(action)
            .ok_or_else(|| BrokerError::NotConfigured(action.clone()))?;
        Ok(CredentialLease::new(c.id.clone(), c.secret.as_str()))
    }

    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.by_action.values().map(|c| c.id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

struct Snapshot(RwLock<Arc<Table>>);

impl Snapshot {
    fn new(t: Table) -> Self {
        Self(RwLock::new(Arc::new(t)))
    }
    fn current(&self) -> Result<Arc<Table>, BrokerError> {
        self.0
            .read()
            .map(|t| t.clone())
            .map_err(|_| BrokerError::Unavailable)
    }
    fn replace(&self, t: Table) -> Result<(), CredentialLoadError> {
        *self
            .0
            .write()
            .map_err(|_| CredentialLoadError::Invalid("credential table lock poisoned".into()))? =
            Arc::new(t);
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileFormat {
    credentials: Vec<FileEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileEntry {
    id: String,
    secret: Zeroizing<String>,
    actions: Vec<ActionId>,
}

/// Credentials from a JSON file owned by Guard's uid with mode 0600
/// (MILESTONE_18 §11). Format:
///
/// ```json
/// {"credentials": [{"id": "business-api", "secret": "…", "actions": ["refund.issue"]}]}
/// ```
///
/// Each action maps to at most one credential. [`reload`](Self::reload)
/// re-reads the file for rotation; leases already handed out keep the old
/// value, and a failed reload keeps the previous table.
pub struct FileCredentialBroker {
    path: PathBuf,
    table: Snapshot,
}

impl std::fmt::Debug for FileCredentialBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ids = self.table.current().map(|t| t.ids()).unwrap_or_default();
        f.debug_struct("FileCredentialBroker")
            .field("path", &self.path)
            .field("credential_ids", &ids)
            .finish_non_exhaustive()
    }
}

impl FileCredentialBroker {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CredentialLoadError> {
        let path = path.as_ref().to_path_buf();
        let table = load_file(&path)?;
        Ok(Self {
            path,
            table: Snapshot::new(table),
        })
    }

    /// Re-reads the file. On error the previous credentials stay in use.
    pub fn reload(&self) -> Result<(), CredentialLoadError> {
        let t = load_file(&self.path)?;
        self.table.replace(t)
    }

    /// Ids (never secrets) of the loaded credentials.
    pub fn credential_ids(&self) -> Vec<String> {
        self.table.current().map(|t| t.ids()).unwrap_or_default()
    }
}

impl CredentialBroker for FileCredentialBroker {
    fn lease<'a>(
        &'a self,
        action: &'a ActionId,
    ) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async move { self.table.current()?.lease(action) })
    }
}

#[cfg(unix)]
fn load_file(path: &Path) -> Result<Table, CredentialLoadError> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let io = |e: std::io::Error| CredentialLoadError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    };
    // O_NOFOLLOW, then fstat the opened file: the checks apply to exactly
    // the bytes read, with no window to swap the path.
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            Some(code) if code == rustix::io::Errno::LOOP.raw_os_error() => {
                CredentialLoadError::NotRegularFile {
                    path: path.to_path_buf(),
                }
            }
            _ => io(e),
        })?;
    let meta = file.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(CredentialLoadError::NotRegularFile {
            path: path.to_path_buf(),
        });
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(CredentialLoadError::Permissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    let expected = rustix::process::geteuid().as_raw();
    if meta.uid() != expected {
        return Err(CredentialLoadError::WrongOwner {
            path: path.to_path_buf(),
            owner: meta.uid(),
            expected,
        });
    }
    if meta.len() > MAX_CREDENTIALS_FILE_BYTES {
        return Err(CredentialLoadError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let mut text = Zeroizing::new(String::new());
    (&mut file)
        .take(MAX_CREDENTIALS_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(io)?;
    if text.len() as u64 > MAX_CREDENTIALS_FILE_BYTES {
        return Err(CredentialLoadError::TooLarge {
            path: path.to_path_buf(),
        });
    }
    let parsed: FileFormat =
        serde_json::from_str(&text).map_err(|e| CredentialLoadError::Malformed {
            line: e.line(),
            column: e.column(),
        })?;
    Table::build(
        parsed
            .credentials
            .into_iter()
            .map(|e| (e.id, e.secret, e.actions))
            .collect(),
    )
}

#[cfg(not(unix))]
fn load_file(_path: &Path) -> Result<Table, CredentialLoadError> {
    Err(CredentialLoadError::Unsupported)
}

/// One credential read from an environment variable, for containers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCredential {
    pub id: String,
    /// Name of the variable holding the secret.
    pub var: String,
    pub actions: Vec<ActionId>,
}

/// Credentials read from Guard's own environment at startup and on
/// [`reload`](Self::reload). The variables stay in Guard's environment,
/// so Guard must never spawn agent processes that inherit it.
pub struct EnvCredentialBroker {
    spec: Vec<EnvCredential>,
    table: Snapshot,
}

impl std::fmt::Debug for EnvCredentialBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvCredentialBroker")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

fn load_env(spec: &[EnvCredential]) -> Result<Table, CredentialLoadError> {
    let mut entries = Vec::with_capacity(spec.len());
    for c in spec {
        let secret =
            std::env::var(&c.var).map_err(|_| CredentialLoadError::MissingEnv(c.var.clone()))?;
        entries.push((c.id.clone(), Zeroizing::new(secret), c.actions.clone()));
    }
    Table::build(entries)
}

impl EnvCredentialBroker {
    pub fn from_env(spec: Vec<EnvCredential>) -> Result<Self, CredentialLoadError> {
        let table = load_env(&spec)?;
        Ok(Self {
            spec,
            table: Snapshot::new(table),
        })
    }

    /// Re-reads the variables. On error the previous credentials stay in use.
    pub fn reload(&self) -> Result<(), CredentialLoadError> {
        let t = load_env(&self.spec)?;
        self.table.replace(t)
    }
}

impl CredentialBroker for EnvCredentialBroker {
    fn lease<'a>(
        &'a self,
        action: &'a ActionId,
    ) -> BoxFuture<'a, Result<CredentialLease, BrokerError>> {
        Box::pin(async move { self.table.current()?.lease(action) })
    }
}
