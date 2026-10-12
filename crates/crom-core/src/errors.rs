use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("path outside workspace: {0}")]
    PathEscape(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("permission denied: {0}")]
    PermissionDenied(String),

    #[error("limit exceeded: {0}")]
    LimitExceeded(String),

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("{0}")]
    Other(String),
}

impl ToolError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("HTTP error: {0}")]
    Http(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("stream error: {0}")]
    Stream(String),

    #[error("rate limited")]
    RateLimited,

    #[error("timeout")]
    Timeout,

    #[error("cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("tool error: {0}")]
    Tool(#[from] ToolError),

    #[error("session error: {0}")]
    Session(String),

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl AgentError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }
}
