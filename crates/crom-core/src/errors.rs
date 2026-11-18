use std::time::Duration;

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

/// Provider failures, classified so callers (agent loop, eval) can tell an
/// upstream outage or quota problem from an agent bug. Retryability is
/// defined by [`ProviderError::is_retryable`] and consumed by
/// [`crate::provider_retry::retry_provider_call`].
#[derive(Debug, Error)]
pub enum ProviderError {
    /// Unclassified HTTP-layer failure (unexpected status, body read error).
    #[error("HTTP error: {0}")]
    Http(String),

    /// 401/403 or a missing/unsupported credential.
    #[error("auth error: {0}")]
    Auth(String),

    /// 402, or a 429 whose body says the quota is exhausted
    /// (`insufficient_quota`): the account needs credits, so retrying is futile.
    #[error("billing error: {0}")]
    Billing(String),

    /// 4xx other than auth/402/408/429: malformed request, context too long,
    /// unsupported parameter, unknown model.
    #[error("invalid request (status {status}): {message}")]
    InvalidRequest { status: u16, message: String },

    #[error("invalid response: {0}")]
    InvalidResponse(String),

    #[error("stream error: {0}")]
    Stream(String),

    /// 429. `retry_after` carries the server's `Retry-After` hint when sent.
    #[error("rate limited: {message}")]
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },

    /// 408 or a client-side connect/read/total timeout.
    #[error("timeout: {0}")]
    Timeout(String),

    /// The connection could not be established, or was reset or dropped
    /// before the response completed.
    #[error("connection error: {0}")]
    Connect(String),

    /// 5xx.
    #[error("server error (status {status}): {message}")]
    ServerError { status: u16, message: String },

    #[error("cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl ProviderError {
    pub fn other(msg: impl Into<String>) -> Self {
        Self::Other(msg.into())
    }

    /// Map a non-2xx HTTP status and its body to a typed variant. `context`
    /// prefixes the message (e.g. `"openai"`).
    pub fn from_status(
        context: &str,
        status: u16,
        body: &str,
        retry_after: Option<Duration>,
    ) -> Self {
        let message = format!("{context}: status {status}: {body}");
        match status {
            401 | 403 => Self::Auth(message),
            402 => Self::Billing(message),
            408 => Self::Timeout(message),
            429 if body.contains("insufficient_quota") => Self::Billing(message),
            429 => Self::RateLimited {
                retry_after,
                message,
            },
            500..=599 => Self::ServerError { status, message },
            400..=499 => Self::InvalidRequest { status, message },
            _ => Self::Http(message),
        }
    }

    /// Transient failures worth retrying: rate limits, timeouts, connection
    /// failures, and 5xx.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::RateLimited { .. }
                | Self::Timeout(_)
                | Self::Connect(_)
                | Self::ServerError { .. }
        )
    }

    /// Server-provided `Retry-After` hint, if any.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    /// Stable snake_case label for events, metrics, and eval reports.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Http(_) => "http",
            Self::Auth(_) => "auth",
            Self::Billing(_) => "billing",
            Self::InvalidRequest { .. } => "invalid_request",
            Self::InvalidResponse(_) => "invalid_response",
            Self::Stream(_) => "stream",
            Self::RateLimited { .. } => "rate_limited",
            Self::Timeout(_) => "timeout",
            Self::Connect(_) => "connect",
            Self::ServerError { .. } => "server_error",
            Self::Cancelled => "cancelled",
            Self::Other(_) => "other",
        }
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
