//! Host-supplied callbacks.
//!
//! The SDK never prompts on stdin/stdout or pops a TUI. Anything that would
//! normally require a human goes through one of these callbacks. If a
//! callback is missing, the runtime applies the configured default
//! behavior — typically "deny" — and records a diagnostic.

mod redaction;

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::Arc;

use code_agent_auth::{AccountId, ProviderId, ResolvedCredential};
use code_agent_core::permission::PermissionRequest as CorePermissionRequest;
use code_agent_core::ToolResult;

use crate::error::AgentSdkError;
use crate::event::SdkEvent;
use crate::policy::SideEffectLevel;

pub use redaction::{DefaultSecretRedactor, NoopSecretRedactor};

/// Approval request passed to the host's [`ApprovalCallback`].
#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub permission_kind: code_agent_core::permission::PermissionKind,
    pub side_effect_level: SideEffectLevel,
    pub summary: String,
    pub input: serde_json::Value,
    pub run_id: crate::RunId,
    pub session_id: Option<crate::SessionId>,
    pub request_id: Option<String>,
    pub tenant_id: Option<String>,
    pub user_id: Option<String>,
}

impl ApprovalRequest {
    /// Build a wire-friendly view of the matching core `PermissionRequest`.
    /// Used by the runtime to call existing core callbacks where useful.
    pub fn to_core(&self) -> CorePermissionRequest {
        CorePermissionRequest {
            tool_name: self.tool_name.clone(),
            permission_kind: self.permission_kind,
            input: self.input.clone(),
            summary: self.summary.clone(),
            metadata: serde_json::Value::Null,
        }
    }
}

/// Decision returned by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    AllowForSession,
    Deny { reason: String },
}

impl ApprovalDecision {
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    pub fn to_core(self) -> code_agent_core::permission::PermissionDecision {
        match self {
            Self::Allow => code_agent_core::permission::PermissionDecision::AllowOnce,
            Self::AllowForSession => {
                code_agent_core::permission::PermissionDecision::AllowForSession
            }
            Self::Deny { reason } => {
                code_agent_core::permission::PermissionDecision::Deny { reason }
            }
        }
    }
}

/// Host approval callback. Called whenever the policy engine needs a
/// human-style decision. The SDK never falls back to a terminal.
#[async_trait]
pub trait ApprovalCallback: Send + Sync {
    async fn approve(&self, request: ApprovalRequest) -> Result<ApprovalDecision, AgentSdkError>;
}

/// Always-deny approval callback (default when none configured under
/// `HeadlessDeny`).
pub struct DenyAllApprovals;

#[async_trait]
impl ApprovalCallback for DenyAllApprovals {
    async fn approve(&self, _request: ApprovalRequest) -> Result<ApprovalDecision, AgentSdkError> {
        Ok(ApprovalDecision::deny("no approval callback configured"))
    }
}

/// Always-allow approval callback. Useful in tests.
pub struct AllowAllApprovals;

#[async_trait]
impl ApprovalCallback for AllowAllApprovals {
    async fn approve(&self, _request: ApprovalRequest) -> Result<ApprovalDecision, AgentSdkError> {
        Ok(ApprovalDecision::Allow)
    }
}

/// Request passed to the host's [`CredentialResolver`].
#[derive(Debug, Clone)]
pub struct CredentialRequest {
    pub provider: ProviderId,
    pub account: Option<AccountId>,
    pub request_id: Option<String>,
    pub tenant_id: Option<String>,
}

/// Host credential resolver. Called when the runtime needs a credential and
/// the application chose not to inject one up-front.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    async fn resolve(
        &self,
        request: CredentialRequest,
    ) -> Result<ResolvedCredential, AgentSdkError>;
}

/// Host event sink. Mirror of `EventSink` from core, but typed in terms of
/// public `SdkEvent`.
#[async_trait]
pub trait EventSink: Send + Sync {
    async fn emit(&self, event: SdkEvent);
}

/// Host secret redactor. Used in messages, diagnostics, and logs before
/// they're handed back to the application.
pub trait SecretRedactor: Send + Sync {
    fn redact(&self, input: &str) -> String;
}

/// Request the runtime makes to a host-supplied tool. Carries the tool name,
/// the arguments the model produced, and contextual identifiers.
#[derive(Debug, Clone)]
pub struct HostToolRequest {
    pub tool_name: String,
    pub input: serde_json::Value,
    pub run_id: crate::RunId,
    pub session_id: Option<crate::SessionId>,
    pub request_id: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

/// Host tool host. When set, the runtime forwards "host tool" invocations
/// here.
#[async_trait]
pub trait ToolHost: Send + Sync {
    async fn execute_host_tool(
        &self,
        request: HostToolRequest,
    ) -> Result<ToolResult, AgentSdkError>;
}

/// Bundle of host callbacks. Hosts can implement [`AgentHost`] once and pass
/// it to the runtime builder instead of registering callbacks one by one.
pub trait AgentHost: Send + Sync {
    fn approval_callback(&self) -> Option<Arc<dyn ApprovalCallback>> {
        None
    }
    fn credential_resolver(&self) -> Option<Arc<dyn CredentialResolver>> {
        None
    }
    fn event_sink(&self) -> Option<Arc<dyn EventSink>> {
        None
    }
    fn secret_redactor(&self) -> Option<Arc<dyn SecretRedactor>> {
        None
    }
    fn tool_host(&self) -> Option<Arc<dyn ToolHost>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DenyAllApprovals always returns Deny.
    #[tokio::test]
    async fn deny_all_denies() {
        let cb = DenyAllApprovals;
        let req = ApprovalRequest {
            tool_name: "Write".to_string(),
            permission_kind: code_agent_core::permission::PermissionKind::Mutating,
            side_effect_level: SideEffectLevel::WorkspaceWrite,
            summary: "write x".into(),
            input: serde_json::json!({}),
            run_id: crate::RunId::new(),
            session_id: None,
            request_id: None,
            tenant_id: None,
            user_id: None,
        };
        let decision = cb.approve(req).await.unwrap();
        assert!(matches!(decision, ApprovalDecision::Deny { .. }));
    }

    /// AllowAllApprovals always returns Allow.
    #[tokio::test]
    async fn allow_all_allows() {
        let cb = AllowAllApprovals;
        let req = ApprovalRequest {
            tool_name: "Read".to_string(),
            permission_kind: code_agent_core::permission::PermissionKind::ReadOnly,
            side_effect_level: SideEffectLevel::WorkspaceRead,
            summary: "read x".into(),
            input: serde_json::json!({}),
            run_id: crate::RunId::new(),
            session_id: None,
            request_id: None,
            tenant_id: None,
            user_id: None,
        };
        let decision = cb.approve(req).await.unwrap();
        assert_eq!(decision, ApprovalDecision::Allow);
    }
}
