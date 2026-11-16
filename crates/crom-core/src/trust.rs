//! Persisted workspace trust (threat model P9, #290).
//!
//! Repository-local configuration (`<cwd>/.agent/settings.json`,
//! `<cwd>/.agent/mcp.json`) is untrusted until the operator trusts the
//! workspace. Trust is an exact match on the canonical workspace path; a
//! trusted parent does not trust its subdirectories. The store lives in the
//! user config dir, outside any workspace, so a repository cannot trust
//! itself.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::events::{EnforcementLayer, SecurityDecision, SecurityOutcome, SecurityPrincipal};
use crate::options::SessionId;
use crate::policy::{PolicyDecisionSource, PolicyOutcome};
use crate::run_id::RunId;

pub const TRUST_STORE_FILE: &str = "trusted-workspaces.json";

/// Whether repo-local configuration may loosen policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTrust {
    Trusted,
    #[default]
    Untrusted,
}

impl WorkspaceTrust {
    pub fn is_trusted(self) -> bool {
        matches!(self, Self::Trusted)
    }
}

/// One piece of repo-local configuration that was ignored because the
/// workspace is untrusted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UntrustedConfigDenial {
    /// File the setting came from.
    pub source: PathBuf,
    /// Setting key, e.g. `permission_mode`, `allowed_tools`, `mcpServers.foo`.
    pub key: String,
    pub reason: String,
}

impl UntrustedConfigDenial {
    pub fn new(source: &Path, key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            source: source.to_path_buf(),
            key: key.into(),
            reason: reason.into(),
        }
    }

    /// Blocked `SecurityDecision` for this denial. `tool_name` is
    /// `workspace_config`; `resource` is `<workspace-relative file>#<key>`.
    pub fn to_security_decision(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        cwd: &Path,
        index: usize,
    ) -> SecurityDecision {
        let file = self
            .source
            .strip_prefix(cwd)
            .unwrap_or(&self.source)
            .display()
            .to_string();
        SecurityDecision {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
            turn_index: 0,
            principal: SecurityPrincipal::Session,
            tool_use_id: format!("workspace-trust-{}-{index}", run_id.0),
            tool_name: "workspace_config".to_string(),
            action_class: None,
            resource: Some(format!("{file}#{}", self.key)),
            policy_source: Some(PolicyDecisionSource::WorkspaceTrust),
            rule: None,
            decision: PolicyOutcome::Deny,
            enforcement: EnforcementLayer::Policy,
            outcome: SecurityOutcome::Blocked,
            reason: Some(crate::builtin_hooks::redact_secrets_in_str(&self.reason)),
        }
    }
}

#[derive(Debug, Error)]
pub enum TrustError {
    #[error("no user config directory (set CROM_CONFIG_HOME or HOME)")]
    NoConfigDir,
    #[error("workspace {path}: {source}")]
    Workspace {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("trust store {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("trust store {path}: invalid JSON: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustStore {
    #[serde(default)]
    workspaces: Vec<PathBuf>,
}

/// `${CROM_CONFIG_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}/crom}/trusted-workspaces.json`.
/// No legacy fallback: the store postdates the rename.
pub fn trust_store_path() -> Option<PathBuf> {
    if let Some(p) = crate::legacy::env_var("CROM_CONFIG_HOME") {
        return Some(PathBuf::from(p).join(TRUST_STORE_FILE));
    }
    let parent = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok()?;
    Some(
        parent
            .join(crate::legacy::CONFIG_DIR_NAME)
            .join(TRUST_STORE_FILE),
    )
}

fn canonical(path: &Path) -> Result<PathBuf, TrustError> {
    path.canonicalize().map_err(|e| TrustError::Workspace {
        path: path.to_path_buf(),
        source: e,
    })
}

fn read_store(path: &Path) -> Result<TrustStore, TrustError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| TrustError::Parse {
            path: path.to_path_buf(),
            source: e,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustStore::default()),
        Err(e) => Err(TrustError::Io {
            path: path.to_path_buf(),
            source: e,
        }),
    }
}

/// Trust state of `cwd`. Any error (no store, unreadable store, missing
/// workspace) resolves to `Untrusted`.
pub fn workspace_trust(cwd: &Path) -> WorkspaceTrust {
    let (Some(store_path), Ok(ws)) = (trust_store_path(), cwd.canonicalize()) else {
        return WorkspaceTrust::Untrusted;
    };
    match read_store(&store_path) {
        Ok(store) if store.workspaces.iter().any(|p| p == &ws) => WorkspaceTrust::Trusted,
        Ok(_) => WorkspaceTrust::Untrusted,
        Err(e) => {
            tracing::warn!("{e}; treating workspace as untrusted");
            WorkspaceTrust::Untrusted
        }
    }
}

/// Persist the trust decision for `cwd`. Returns the canonical path and
/// whether the store changed.
pub fn set_workspace_trust(
    cwd: &Path,
    trust: WorkspaceTrust,
) -> Result<(PathBuf, bool), TrustError> {
    let ws = canonical(cwd)?;
    let store_path = trust_store_path().ok_or(TrustError::NoConfigDir)?;
    let mut store = read_store(&store_path)?;
    let before = store.workspaces.len();
    store.workspaces.retain(|p| p != &ws);
    if trust.is_trusted() {
        store.workspaces.push(ws.clone());
        store.workspaces.sort();
    }
    let changed = store.workspaces.len() != before;
    if changed {
        let io = |e| TrustError::Io {
            path: store_path.clone(),
            source: e,
        };
        if let Some(dir) = store_path.parent() {
            std::fs::create_dir_all(dir).map_err(io)?;
        }
        let body = serde_json::to_vec_pretty(&store).expect("trust store serializes");
        let tmp = store_path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(io)?;
        std::fs::rename(&tmp, &store_path).map_err(io)?;
    }
    Ok((ws, changed))
}

/// All trusted workspace paths, sorted.
pub fn trusted_workspaces() -> Result<Vec<PathBuf>, TrustError> {
    let store_path = trust_store_path().ok_or(TrustError::NoConfigDir)?;
    Ok(read_store(&store_path)?.workspaces)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn with_config_home<R>(home: &Path, f: impl FnOnce() -> R) -> R {
        temp_env::with_vars(
            [
                ("CROM_CONFIG_HOME", Some(home.to_str().unwrap())),
                ("CODE_AGENT_CONFIG_HOME", None),
            ],
            f,
        )
    }

    #[test]
    // A workspace is untrusted until trusted, trusted afterwards, and untrusted again after untrust.
    fn trust_round_trip() {
        let home = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        with_config_home(home.path(), || {
            assert_eq!(workspace_trust(ws.path()), WorkspaceTrust::Untrusted);
            let (_, changed) = set_workspace_trust(ws.path(), WorkspaceTrust::Trusted).unwrap();
            assert!(changed);
            assert_eq!(workspace_trust(ws.path()), WorkspaceTrust::Trusted);
            let (_, changed) = set_workspace_trust(ws.path(), WorkspaceTrust::Trusted).unwrap();
            assert!(!changed);
            set_workspace_trust(ws.path(), WorkspaceTrust::Untrusted).unwrap();
            assert_eq!(workspace_trust(ws.path()), WorkspaceTrust::Untrusted);
        });
    }

    #[test]
    // Trusting a parent directory does not trust a subdirectory.
    fn trust_is_exact_path() {
        let home = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        let child = ws.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        with_config_home(home.path(), || {
            set_workspace_trust(ws.path(), WorkspaceTrust::Trusted).unwrap();
            assert_eq!(workspace_trust(&child), WorkspaceTrust::Untrusted);
        });
    }

    #[test]
    // A corrupt trust store resolves to untrusted rather than trusted.
    fn corrupt_store_is_untrusted() {
        let home = TempDir::new().unwrap();
        let ws = TempDir::new().unwrap();
        std::fs::write(home.path().join(TRUST_STORE_FILE), b"{not json").unwrap();
        with_config_home(home.path(), || {
            assert_eq!(workspace_trust(ws.path()), WorkspaceTrust::Untrusted);
        });
    }
}
