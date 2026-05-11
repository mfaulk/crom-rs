//! Plan store / artifact errors.

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PlanError {
    #[error("plan not found: {0}")]
    NotFound(String),
    #[error("plan id is invalid: {0}")]
    InvalidId(String),
    #[error("plan store I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("plan JSON parse at {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("plan validation failed: {0}")]
    Invalid(String),
    #[error("plan exceeds size limit: {bytes} > {limit}")]
    TooLarge { bytes: usize, limit: usize },
    #[error("plan has too many steps: {got} > {limit}")]
    TooManySteps { got: usize, limit: usize },
    #[error("plan has too many revisions: {got} > {limit}")]
    TooManyRevisions { got: u32, limit: usize },
    #[error("plan version mismatch: expected {expected}, got {got}")]
    VersionMismatch { expected: u32, got: u32 },
    #[error("plan status forbids this operation: {0}")]
    BadStatus(String),
    #[error("plan store internal error: {0}")]
    Other(String),
}
