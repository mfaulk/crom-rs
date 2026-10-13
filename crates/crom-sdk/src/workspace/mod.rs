//! Workspace abstraction.
//!
//! Applications can plug in a filesystem workspace, an in-memory map, a
//! read-only wrapper, a patch-only recorder, or an overlay that reads from
//! a base and writes to an overlay layer.

mod filesystem;
mod in_memory;
mod overlay;
mod patch_only;
mod path;
mod read_only;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub use filesystem::FilesystemWorkspace;
pub use in_memory::InMemoryWorkspace;
pub use overlay::OverlayWorkspace;
pub use patch_only::PatchOnlyWorkspace;
pub use path::WorkspacePath;
pub use read_only::ReadOnlyWorkspace;

use crate::error::Diagnostic;
use crate::patch::PatchSet;

/// What can the workspace do? Used by the runtime to gate operations and by
/// the SDK to refuse invalid mode/policy combinations early.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCapabilities {
    pub can_read: bool,
    pub can_write: bool,
    pub can_list: bool,
    pub can_search: bool,
    pub can_execute_commands: bool,
    pub can_apply_patch: bool,
}

impl WorkspaceCapabilities {
    pub fn read_only() -> Self {
        Self {
            can_read: true,
            can_write: false,
            can_list: true,
            can_search: true,
            can_execute_commands: false,
            can_apply_patch: false,
        }
    }

    pub fn full() -> Self {
        Self {
            can_read: true,
            can_write: true,
            can_list: true,
            can_search: true,
            can_execute_commands: true,
            can_apply_patch: true,
        }
    }
}

/// Workspace-level error returned by the [`Workspace`] trait. The runtime
/// converts these to diagnostics on the event stream and result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<WorkspacePath>,
}

impl WorkspaceError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            path: None,
        }
    }

    pub fn with_path(mut self, path: WorkspacePath) -> Self {
        self.path = Some(path);
        self
    }

    pub fn not_found(path: WorkspacePath) -> Self {
        Self::new("workspace.not_found", format!("not found: {path}")).with_path(path)
    }

    pub fn write_denied(path: WorkspacePath) -> Self {
        Self::new("workspace.write_denied", format!("write denied: {path}")).with_path(path)
    }

    pub fn read_failed(path: WorkspacePath, reason: impl Into<String>) -> Self {
        Self::new("workspace.read_failed", reason.into()).with_path(path)
    }

    pub fn write_failed(path: WorkspacePath, reason: impl Into<String>) -> Self {
        Self::new("workspace.write_failed", reason.into()).with_path(path)
    }

    pub fn path_outside_root(raw: impl Into<String>) -> Self {
        let s = raw.into();
        Self::new(
            "workspace.path_outside_root",
            format!("path outside root: {s}"),
        )
    }

    pub fn into_diagnostic(self) -> Diagnostic {
        let mut d = Diagnostic::error(self.code.clone(), self.message.clone());
        if let Some(p) = self.path {
            d = d.with_location(crate::error::DiagnosticLocation::File {
                path: p,
                line: None,
                column: None,
            });
        }
        d
    }
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WorkspaceError {}

/// Result of reading a file from the workspace.
#[derive(Debug, Clone)]
pub struct FileContent {
    pub bytes: Vec<u8>,
    pub is_binary: bool,
    pub content_hash: Option<String>,
    pub language: Option<String>,
}

impl FileContent {
    pub fn new_text(bytes: Vec<u8>) -> Self {
        let is_binary = is_binary_bytes(&bytes);
        let hash = compute_hash(&bytes);
        Self {
            bytes,
            is_binary,
            content_hash: Some(hash),
            language: None,
        }
    }

    pub fn as_str_lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// Cheap heuristic that mirrors what real coding tools use: anything with a
/// NUL byte in the first 8 KiB is treated as binary.
pub(crate) fn is_binary_bytes(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

pub(crate) fn compute_hash(bytes: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    hex::encode(digest)
}

/// Query parameters for listing files.
#[derive(Debug, Clone, Default)]
pub struct FileQuery {
    pub glob: Option<String>,
    pub limit: Option<usize>,
    pub prefix: Option<WorkspacePath>,
}

impl FileQuery {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn glob(mut self, g: impl Into<String>) -> Self {
        self.glob = Some(g.into());
        self
    }
    pub fn limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
    pub fn prefix(mut self, p: WorkspacePath) -> Self {
        self.prefix = Some(p);
        self
    }
}

/// Query parameters for searching file contents.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub pattern: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub limit: Option<usize>,
    pub prefix: Option<WorkspacePath>,
}

impl SearchQuery {
    pub fn substring(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            regex: false,
            case_sensitive: true,
            limit: None,
            prefix: None,
        }
    }
}

/// One match returned by `Workspace::search`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchMatch {
    pub path: WorkspacePath,
    pub line: u32,
    pub column: u32,
    pub line_text: String,
}

/// What a [`Workspace::diff`] uses as a baseline.
#[derive(Debug, Clone)]
pub enum DiffBase {
    /// Pristine: diff against the original workspace contents.
    Initial,
    /// Custom snapshot: diff against an explicit set of base files.
    /// Useful for overlay implementations that store base contents
    /// out-of-band.
    Snapshot(std::collections::BTreeMap<WorkspacePath, Vec<u8>>),
}

/// Outcome of applying a patch to the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyPatchReport {
    pub files_changed: usize,
    pub files_skipped: usize,
    pub failures: Vec<WorkspaceError>,
}

/// The workspace trait. All paths are workspace-relative `WorkspacePath`
/// values; absolute or traversing paths are rejected before the trait is
/// called.
#[async_trait]
pub trait Workspace: Send + Sync {
    async fn read_file(&self, path: &WorkspacePath) -> Result<FileContent, WorkspaceError>;

    async fn write_file(&self, path: &WorkspacePath, content: &[u8]) -> Result<(), WorkspaceError>;

    async fn delete_file(&self, path: &WorkspacePath) -> Result<(), WorkspaceError>;

    async fn list_files(&self, query: FileQuery) -> Result<Vec<WorkspacePath>, WorkspaceError>;

    async fn search(&self, query: SearchQuery) -> Result<Vec<SearchMatch>, WorkspaceError>;

    /// Apply a patch and return a summary report. Implementations that do
    /// not support mutation should return an error or a `WorkspaceError`.
    async fn apply_patch(&self, patch: &PatchSet) -> Result<ApplyPatchReport, WorkspaceError>;

    /// Return a `PatchSet` describing changes relative to `base`.
    async fn diff(&self, base: DiffBase) -> Result<PatchSet, WorkspaceError>;

    fn capabilities(&self) -> WorkspaceCapabilities;

    /// Optional filesystem-root accessor for adapters that wrap an underlying
    /// directory. Used by the runtime to wire up filesystem-aware tools and
    /// validation commands. Workspaces with no filesystem root return `None`.
    fn filesystem_root(&self) -> Option<PathBuf> {
        None
    }
}

/// Source descriptor that the runtime materializes into a [`Workspace`].
pub enum WorkspaceSource {
    Filesystem(PathBuf),
    InMemory(InMemoryWorkspace),
    ReadOnly(Arc<dyn Workspace>),
    PatchOnly(Arc<dyn Workspace>),
    Overlay { base: Arc<dyn Workspace> },
    Custom(Arc<dyn Workspace>),
}

impl std::fmt::Debug for WorkspaceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Filesystem(p) => f.debug_tuple("Filesystem").field(p).finish(),
            Self::InMemory(_) => f.write_str("InMemory(..)"),
            Self::ReadOnly(_) => f.write_str("ReadOnly(..)"),
            Self::PatchOnly(_) => f.write_str("PatchOnly(..)"),
            Self::Overlay { .. } => f.write_str("Overlay { .. }"),
            Self::Custom(_) => f.write_str("Custom(..)"),
        }
    }
}

impl WorkspaceSource {
    pub fn filesystem(path: impl Into<PathBuf>) -> Self {
        Self::Filesystem(path.into())
    }

    pub fn in_memory(files: InMemoryWorkspace) -> Self {
        Self::InMemory(files)
    }

    pub fn read_only(inner: Arc<dyn Workspace>) -> Self {
        Self::ReadOnly(inner)
    }

    pub fn patch_only(inner: Arc<dyn Workspace>) -> Self {
        Self::PatchOnly(inner)
    }

    pub fn overlay(base: Arc<dyn Workspace>) -> Self {
        Self::Overlay { base }
    }

    pub fn custom(inner: Arc<dyn Workspace>) -> Self {
        Self::Custom(inner)
    }

    /// Materialize into a concrete `Arc<dyn Workspace>`. Filesystem sources
    /// expand to a [`FilesystemWorkspace`]. In-memory sources expand into
    /// the supplied store. Wrappers (read-only, patch-only, overlay) wrap
    /// the inner workspace.
    pub fn into_workspace(self) -> Result<Arc<dyn Workspace>, crate::error::AgentSdkError> {
        Ok(match self {
            #[cfg(feature = "filesystem-workspace")]
            Self::Filesystem(path) => Arc::new(FilesystemWorkspace::new(path)?),
            #[cfg(not(feature = "filesystem-workspace"))]
            Self::Filesystem(_) => {
                return Err(crate::error::AgentSdkError::invalid_request(
                    "workspace.feature_disabled",
                    "filesystem-workspace feature is disabled",
                ));
            }
            #[cfg(feature = "in-memory-workspace")]
            Self::InMemory(w) => Arc::new(w),
            #[cfg(not(feature = "in-memory-workspace"))]
            Self::InMemory(_) => {
                return Err(crate::error::AgentSdkError::invalid_request(
                    "workspace.feature_disabled",
                    "in-memory-workspace feature is disabled",
                ));
            }
            Self::ReadOnly(inner) => Arc::new(ReadOnlyWorkspace::new(inner)),
            Self::PatchOnly(inner) => Arc::new(PatchOnlyWorkspace::new(inner)),
            Self::Overlay { base } => Arc::new(OverlayWorkspace::new(base)),
            Self::Custom(w) => w,
        })
    }
}
