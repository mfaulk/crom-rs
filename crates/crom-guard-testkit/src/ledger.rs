use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What the business API did. The ledger is the M18 effect oracle
/// (MILESTONE_18 §13.2), so every successful read, refund, and export
/// appends exactly one entry; rejected requests append none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Effect {
    CustomerRead {
        customer_id: String,
    },
    InvoiceRead {
        invoice_id: String,
    },
    InvoiceList {
        customer_id: String,
        /// Every invoice id returned in the response.
        invoice_ids: Vec<String>,
    },
    Refund {
        invoice_id: String,
        amount_cents: u64,
        refund_id: String,
    },
    CustomerExport {
        /// Every customer id returned in the response.
        customer_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub seq: u64,
    pub tenant: String,
    /// Id of the credential that authenticated the request (never the secret).
    pub credential_id: String,
    /// `X-Guard-Decision-Id` sent with the request; `None` when called
    /// without Guard (direct, trusted mode).
    pub guard_decision_id: Option<String>,
    #[serde(flatten)]
    pub effect: Effect,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("ledger io at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Append-only effect ledger, kept in memory and mirrored to a JSONL file
/// when a path is configured.
#[derive(Debug, Default)]
pub struct Ledger {
    path: Option<PathBuf>,
    entries: Mutex<Vec<LedgerEntry>>,
}

impl Ledger {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Starts a fresh ledger at `path`, truncating any previous file.
    pub fn with_file(path: &Path) -> Result<Self, LedgerError> {
        std::fs::write(path, b"").map_err(|source| LedgerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self {
            path: Some(path.to_path_buf()),
            entries: Mutex::default(),
        })
    }

    /// Appends one entry. The file write happens under the same lock as the
    /// in-memory push so file order and `seq` agree.
    pub(crate) fn append(
        &self,
        tenant: &str,
        credential_id: &str,
        guard_decision_id: Option<String>,
        effect: Effect,
    ) -> Result<LedgerEntry, LedgerError> {
        let mut entries = self.entries.lock().unwrap();
        let entry = LedgerEntry {
            seq: entries.len() as u64,
            tenant: tenant.into(),
            credential_id: credential_id.into(),
            guard_decision_id,
            effect,
        };
        if let Some(path) = &self.path {
            let io = |source| LedgerError::Io {
                path: path.clone(),
                source,
            };
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(path)
                .map_err(io)?;
            let mut line = serde_json::to_vec(&entry).expect("ledger entry serializes");
            line.push(b'\n');
            f.write_all(&line).map_err(io)?;
        }
        entries.push(entry.clone());
        Ok(entry)
    }

    pub fn entries(&self) -> Vec<LedgerEntry> {
        self.entries.lock().unwrap().clone()
    }

    pub(crate) fn clear(&self) -> Result<(), LedgerError> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(path) = &self.path {
            std::fs::write(path, b"").map_err(|source| LedgerError::Io {
                path: path.clone(),
                source,
            })?;
        }
        entries.clear();
        Ok(())
    }

    /// Reads a ledger file back, e.g. after a run in another process.
    pub fn read_file(path: &Path) -> Result<Vec<LedgerEntry>, LedgerError> {
        let text = std::fs::read_to_string(path).map_err(|source| LedgerError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("ledger line parses"))
            .collect())
    }
}
