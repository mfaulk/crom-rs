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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_name: String,
    pub permission_kind: PermissionKind,
    pub input: serde_json::Value,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny { reason: String },
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

/// A callback that always allows. Useful in tests.
pub struct AlwaysAllow;

#[async_trait]
impl PermissionCallback for AlwaysAllow {
    async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
}
