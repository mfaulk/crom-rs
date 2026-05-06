use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    DontAsk,
}

impl PermissionMode {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "acceptedits" | "accept_edits" | "accept-edits" => Some(Self::AcceptEdits),
            "plan" => Some(Self::Plan),
            "dontask" | "dont_ask" | "dont-ask" => Some(Self::DontAsk),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionKind {
    ReadOnly,
    Mutating,
    Execution,
    External,
    /// Subagent orchestration — invoking the built-in `Agent` tool. Treated
    /// like `Execution` by the policy pipeline (no auto-approval under
    /// `Default` or `AcceptEdits`), but classified separately so callbacks
    /// and audit records can show "this allows the parent to delegate work
    /// to a child agent" with appropriate context.
    Orchestration,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub permission_kind: PermissionKind,
    pub input: serde_json::Value,
    pub summary: String,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

/// Decision returned by a permission callback or memory.
///
/// `AllowOnce` allows just this call.
/// `AllowForSession` allows this and any later call that matches the same
/// approval key for the rest of the session.
/// `AllowForProject` is like `AllowForSession` but persists across sessions
/// (when the runtime opts into project-level memory).
/// `Deny` denies just this call.
/// `DenyForSession` denies this and any later matching call for the rest of
/// the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PermissionDecision {
    AllowOnce,
    AllowForSession,
    AllowForProject,
    Deny { reason: String },
    DenyForSession { reason: String },
}

impl PermissionDecision {
    /// Backwards-compatible alias for `AllowOnce`.
    pub fn allow() -> Self {
        Self::AllowOnce
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(
            self,
            Self::AllowOnce | Self::AllowForSession | Self::AllowForProject
        )
    }

    pub fn is_deny(&self) -> bool {
        matches!(self, Self::Deny { .. } | Self::DenyForSession { .. })
    }
}

#[async_trait]
pub trait PermissionCallback: Send + Sync {
    async fn approve(&self, request: PermissionRequest) -> PermissionDecision;
}

/// A callback that always denies. Used as a safe default when a tool requires
/// approval but no callback was supplied.
pub struct AlwaysDeny;

#[async_trait]
impl PermissionCallback for AlwaysDeny {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny {
            reason: "no permission callback configured".to_string(),
        }
    }
}

/// A callback that always allows once. Useful in tests.
pub struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::AllowOnce
    }
}

/// Approval-memory mode: how an approval applies to subsequent calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMemoryMode {
    /// No memory: every approval prompt is shown again next time.
    None,
    /// Memory persists for the duration of the live agent session.
    #[default]
    Session,
    /// Memory persists across sessions for the same cwd (project).
    Project,
}
