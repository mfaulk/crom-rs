use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

use super::chain::{encode_line, verify_log, GENESIS_HASH};
use super::{AuditError, AuditReceipt, AuditRecord, AuditSink};
use crate::HANDLE_PREFIX;

/// Append-only byte store under a [`ChainedAuditSink`].
pub trait AuditStorage: Send {
    /// Appends one complete line and returns only once it is durable. On
    /// `Err` the line may be partially written; the sink then refuses all
    /// further appends.
    fn append_durable(&mut self, line: &[u8]) -> io::Result<()>;
}

/// `write_all` then `sync_data`. Open the file with `append(true)`.
impl AuditStorage for File {
    fn append_durable(&mut self, line: &[u8]) -> io::Result<()> {
        self.write_all(line)?;
        self.sync_data()
    }
}

/// Hash-chained `guard-audit/v1` sink over any [`AuditStorage`].
///
/// Appends are serialized by a mutex, so `seq` is strictly increasing with
/// no gaps. The write and fsync run synchronously on the thread that polls
/// the `append` future; async callers should poll it off the reactor
/// (e.g. `spawn_blocking`). After a storage error the sink is failed
/// closed: every later append returns [`AuditError::Unavailable`] until the
/// process restarts and reopens (and so re-verifies) the log.
pub struct ChainedAuditSink<S> {
    state: Mutex<State<S>>,
}

/// The production sink: a single `0600` JSONL file.
pub type FileAuditSink = ChainedAuditSink<File>;

struct State<S> {
    storage: S,
    head: Option<AuditReceipt>,
    failed: bool,
}

impl<S> std::fmt::Debug for ChainedAuditSink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let head = self.state.lock().ok().and_then(|s| s.head.clone());
        f.debug_struct("ChainedAuditSink")
            .field("head", &head)
            .finish_non_exhaustive()
    }
}

impl<S: AuditStorage> ChainedAuditSink<S> {
    /// Continues a chain whose last record is `head` (`None` starts at
    /// genesis). The caller vouches that `head` matches `storage`;
    /// [`FileAuditSink::open`] establishes this by verifying the file.
    pub fn with_storage(storage: S, head: Option<AuditReceipt>) -> Self {
        Self {
            state: Mutex::new(State {
                storage,
                head,
                failed: false,
            }),
        }
    }

    /// The last durable record, for returning to clients as an anchor.
    pub fn head(&self) -> Option<AuditReceipt> {
        self.state.lock().ok().and_then(|s| s.head.clone())
    }

    /// Chains and durably appends `record`. [`AuditSink::append`] wraps
    /// this.
    pub fn append_blocking(&self, record: &AuditRecord) -> Result<AuditReceipt, AuditError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AuditError::Unavailable("audit sink lock poisoned".into()))?;
        if state.failed {
            return Err(AuditError::Unavailable(
                "audit sink failed closed after a write error; restart required".into(),
            ));
        }
        let seq = state.head.as_ref().map_or(0, |h| h.seq) + 1;
        let prev = state
            .head
            .as_ref()
            .map_or(GENESIS_HASH, |h| h.head_hash.as_str());
        let (line, hash) = encode_line(seq, prev, record)
            .map_err(|e| AuditError::Unavailable(format!("audit record encoding: {e}")))?;
        if contains_plaintext_handle(&line) {
            return Err(AuditError::Unavailable(
                "audit record contains a plaintext session handle".into(),
            ));
        }
        if let Err(e) = state.storage.append_durable(&line) {
            state.failed = true;
            return Err(AuditError::Io(e));
        }
        let receipt = AuditReceipt {
            seq,
            head_hash: hash,
        };
        state.head = Some(receipt.clone());
        Ok(receipt)
    }
}

impl<S: AuditStorage> AuditSink for ChainedAuditSink<S> {
    fn append<'a>(
        &'a self,
        record: &'a AuditRecord,
    ) -> crate::BoxFuture<'a, Result<AuditReceipt, AuditError>> {
        Box::pin(async move { self.append_blocking(record) })
    }
}

impl ChainedAuditSink<File> {
    /// Opens or creates the log at `path` and resumes its chain.
    ///
    /// A new file is created `0600` and its directory is fsynced. An
    /// existing file must be owner-only and must pass [`verify_log`]; a
    /// partial trailing record (a torn write from a crash) also fails, so
    /// Guard does not start until an operator inspects the log. The file
    /// is exclusively locked for the sink's lifetime so a second writer
    /// cannot fork the chain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, AuditError> {
        let path = path.as_ref();
        let unavailable = |m: String| AuditError::Unavailable(format!("{}: {m}", path.display()));
        let mut opts = OpenOptions::new();
        opts.read(true).append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
        let file = opts.open(path)?;
        file.try_lock()
            .map_err(|e| unavailable(format!("cannot lock audit log: {e}")))?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(unavailable("not a regular file".into()));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = meta.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(unavailable(format!(
                    "mode {:o} is accessible to group or others; expected 0600",
                    mode & 0o777
                )));
            }
        }
        let head = if meta.len() == 0 {
            file.sync_all()?;
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                File::open(dir)?.sync_all()?;
            }
            None
        } else {
            verify_log(io::BufReader::new(&file), &[])
                .map_err(|e| unavailable(e.to_string()))?
                .head
        };
        Ok(Self::with_storage(file, head))
    }
}

/// True if `line` contains `gsh_` followed by 64 lowercase hex digits.
fn contains_plaintext_handle(line: &[u8]) -> bool {
    let prefix = HANDLE_PREFIX.as_bytes();
    line.windows(prefix.len() + 64).any(|w| {
        w.starts_with(prefix)
            && w[prefix.len()..]
                .iter()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}
