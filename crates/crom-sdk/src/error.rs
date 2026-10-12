use serde::{Deserialize, Serialize};
use std::fmt;
use thiserror::Error;

use crate::workspace::WorkspacePath;
use crom_auth::ProviderId;
use crom_core::plan::PlanId;

/// Severity of a [`Diagnostic`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Info,
    Warning,
    Error,
}

/// Where the diagnostic originates from. Used to surface tooling/IDE
/// integrations; not all fields are present for every kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiagnosticLocation {
    File {
        path: WorkspacePath,
        #[serde(skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    Tool {
        name: String,
    },
    Provider {
        provider: ProviderId,
    },
    Plan {
        plan_id: PlanId,
        #[serde(skip_serializing_if = "Option::is_none")]
        step_id: Option<String>,
    },
}

/// Structured diagnostic emitted by the SDK. Diagnostics travel on the event
/// stream and on the final result; they are also embedded in
/// [`AgentSdkError`] for unrecoverable failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostic {
    pub code: String,
    pub severity: DiagnosticSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<DiagnosticLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl Diagnostic {
    pub fn new(
        code: impl Into<String>,
        severity: DiagnosticSeverity,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            severity,
            message: message.into(),
            location: None,
            suggestion: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Error, message)
    }

    pub fn warning(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Warning, message)
    }

    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, DiagnosticSeverity::Info, message)
    }

    pub fn with_location(mut self, location: DiagnosticLocation) -> Self {
        self.location = Some(location);
        self
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

/// Categorical kind of a public SDK error. Stable: applications match on
/// this to drive retry / surface logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentSdkErrorKind {
    InvalidRequest,
    Provider,
    Workspace,
    Tool,
    Permission,
    Approval,
    Storage,
    Planning,
    Patch,
    Validation,
    BudgetExceeded,
    Timeout,
    Cancelled,
    Internal,
}

/// The unified public SDK error. Carries a stable kind, a redacted message,
/// optional structured diagnostics, and a private source for debug logging.
#[derive(Debug, Error)]
pub struct AgentSdkError {
    pub kind: AgentSdkErrorKind,
    pub message: String,
    pub diagnostics: Vec<Diagnostic>,
    #[source]
    pub source: Option<anyhow::Error>,
}

impl AgentSdkError {
    pub fn new(kind: AgentSdkErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            diagnostics: Vec::new(),
            source: None,
        }
    }

    pub fn with_diagnostic(mut self, diagnostic: Diagnostic) -> Self {
        self.diagnostics.push(diagnostic);
        self
    }

    pub fn with_diagnostics(mut self, diagnostics: Vec<Diagnostic>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    pub fn with_source(mut self, source: anyhow::Error) -> Self {
        self.source = Some(source);
        self
    }

    // ---- builders for each kind, with diagnostic injection ----

    pub fn invalid_request(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::InvalidRequest, m).with_diagnostic(diag)
    }

    pub fn provider(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Provider, m).with_diagnostic(diag)
    }

    pub fn workspace(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Workspace, m).with_diagnostic(diag)
    }

    pub fn tool(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Tool, m).with_diagnostic(diag)
    }

    pub fn permission(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Permission, m).with_diagnostic(diag)
    }

    pub fn approval(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Approval, m).with_diagnostic(diag)
    }

    pub fn storage(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Storage, m).with_diagnostic(diag)
    }

    pub fn planning(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Planning, m).with_diagnostic(diag)
    }

    pub fn patch(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Patch, m).with_diagnostic(diag)
    }

    pub fn validation(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Validation, m).with_diagnostic(diag)
    }

    pub fn budget(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::BudgetExceeded, m).with_diagnostic(diag)
    }

    pub fn timeout(code: impl Into<String>, message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::error(code, m.clone());
        Self::new(AgentSdkErrorKind::Timeout, m).with_diagnostic(diag)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        let m = message.into();
        let diag = Diagnostic::info("run.cancelled", m.clone());
        Self::new(AgentSdkErrorKind::Cancelled, m).with_diagnostic(diag)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        let m = message.into();
        Self::new(AgentSdkErrorKind::Internal, m)
    }

    pub fn is_cancelled(&self) -> bool {
        self.kind == AgentSdkErrorKind::Cancelled
    }
}

impl fmt::Display for AgentSdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl From<crom_core::AgentError> for AgentSdkError {
    fn from(err: crom_core::AgentError) -> Self {
        use crom_core::AgentError as E;
        match err {
            E::Provider(p) => Self::provider("model.provider_error", p.to_string())
                .with_source(anyhow::anyhow!(p)),
            E::Tool(t) => {
                Self::tool("tool.execution_failed", t.to_string()).with_source(anyhow::anyhow!(t))
            }
            E::Session(s) => Self::storage("storage.session_error", s),
            E::InvalidConfig(s) => Self::invalid_request("request.invalid_config", s),
            E::Cancelled => Self::cancelled("run cancelled"),
            E::Other(s) => Self::internal(s),
        }
    }
}

impl From<std::io::Error> for AgentSdkError {
    fn from(err: std::io::Error) -> Self {
        Self::workspace("workspace.io_error", err.to_string()).with_source(anyhow::anyhow!(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AgentSdkError carries the diagnostic code and message for invalid_request.
    #[test]
    fn invalid_request_attaches_diagnostic() {
        let e = AgentSdkError::invalid_request("test.code", "boom");
        assert_eq!(e.kind, AgentSdkErrorKind::InvalidRequest);
        assert_eq!(e.diagnostics.len(), 1);
        assert_eq!(e.diagnostics[0].code, "test.code");
        assert_eq!(e.diagnostics[0].severity, DiagnosticSeverity::Error);
    }

    /// Diagnostic is JSON-serializable round-trip.
    #[test]
    fn diagnostic_roundtrips_json() {
        let d = Diagnostic::error("workspace.path_outside_root", "denied")
            .with_metadata(serde_json::json!({"path": "x"}));
        let s = serde_json::to_string(&d).unwrap();
        let back: Diagnostic = serde_json::from_str(&s).unwrap();
        assert_eq!(back.code, d.code);
        assert_eq!(back.severity, d.severity);
        assert_eq!(back.metadata, d.metadata);
    }

    /// AgentSdkError::cancelled is recognized via is_cancelled().
    #[test]
    fn cancelled_is_detected() {
        let e = AgentSdkError::cancelled("user cancelled");
        assert!(e.is_cancelled());
    }
}
