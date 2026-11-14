//! Glue between SDK callback traits and the underlying core traits.
//!
//! - `SdkPermissionCallback` adapts an [`ApprovalCallback`] into
//!   [`crom_core::PermissionCallback`].

use std::sync::Arc;

use async_trait::async_trait;
use crom_core::{PermissionCallback, PermissionDecision, PermissionRequest};

use crate::callback::{ApprovalCallback, ApprovalDecision, ApprovalRequest};
use crate::policy::{InteractivityMode, SideEffectLevel};
use crate::request::{ApprovalPolicy, RequestMetadata};
use crate::{RunId, SessionId};

pub(crate) struct SdkPermissionCallback {
    pub inner: Option<Arc<dyn ApprovalCallback>>,
    pub policy: ApprovalPolicy,
    pub interactivity: InteractivityMode,
    pub run_id: RunId,
    pub session_id: Option<SessionId>,
    pub metadata: RequestMetadata,
}

#[async_trait]
impl PermissionCallback for SdkPermissionCallback {
    async fn approve(&self, request: PermissionRequest) -> PermissionDecision {
        use crom_core::PermissionKind;
        // InteractivityMode is the outermost gate. `HeadlessDeny` always
        // denies before consulting the policy or callback; `HeadlessPlanOnly`
        // denies anything that would mutate or execute.
        match self.interactivity {
            InteractivityMode::HeadlessDeny => {
                return PermissionDecision::deny(
                    "interactivity mode HeadlessDeny refuses approvals",
                );
            }
            InteractivityMode::HeadlessPlanOnly => {
                if !matches!(request.permission_kind, PermissionKind::ReadOnly) {
                    return PermissionDecision::deny(
                        "interactivity mode HeadlessPlanOnly refuses non-read approvals",
                    );
                }
            }
            InteractivityMode::Headless | InteractivityMode::Interactive => {}
        }
        // ApprovalPolicy short-circuits.
        match self.policy {
            ApprovalPolicy::NeverAskDeny => {
                return PermissionDecision::deny("approval policy denies all");
            }
            ApprovalPolicy::NeverAskAllowSafe => {
                if matches!(request.permission_kind, PermissionKind::ReadOnly) {
                    return PermissionDecision::AllowOnce;
                }
                return PermissionDecision::deny("approval policy allows read-only only");
            }
            ApprovalPolicy::AutoApproveReadOnly => {
                if matches!(request.permission_kind, PermissionKind::ReadOnly) {
                    return PermissionDecision::AllowOnce;
                }
            }
            _ => {}
        }
        let side_effect_level = match request.permission_kind {
            PermissionKind::ReadOnly => SideEffectLevel::WorkspaceRead,
            PermissionKind::Mutating => SideEffectLevel::WorkspaceWrite,
            PermissionKind::Execution => SideEffectLevel::CommandExecution,
            PermissionKind::External => SideEffectLevel::ExternalNetwork,
            PermissionKind::Orchestration => SideEffectLevel::None,
        };
        let req = ApprovalRequest {
            tool_name: request.tool_name.clone(),
            permission_kind: request.permission_kind,
            side_effect_level,
            summary: request.summary.clone(),
            input: request.input.clone(),
            run_id: self.run_id.clone(),
            session_id: self.session_id.clone(),
            request_id: self.metadata.request_id.clone(),
            tenant_id: self.metadata.tenant_id.clone(),
            user_id: self.metadata.user_id.clone(),
        };
        match &self.inner {
            Some(cb) => match cb.approve(req).await {
                Ok(d) => d.to_core(),
                Err(e) => {
                    PermissionDecision::deny(format!("approval callback errored: {}", e.message))
                }
            },
            None => {
                // Headless-deny default.
                PermissionDecision::deny("no approval callback configured")
            }
        }
    }
}

/// Build a function that converts a core `PermissionDecision` into our
/// public `ApprovalDecision` (for surface events).
#[allow(dead_code)]
pub(crate) fn approval_decision_from_core(d: PermissionDecision) -> ApprovalDecision {
    match d {
        PermissionDecision::AllowOnce => ApprovalDecision::Allow,
        PermissionDecision::AllowForSession => ApprovalDecision::AllowForSession,
        PermissionDecision::AllowForProject => ApprovalDecision::AllowForSession,
        PermissionDecision::Deny { reason } => ApprovalDecision::Deny { reason },
        PermissionDecision::DenyForSession { reason } => ApprovalDecision::Deny { reason },
        PermissionDecision::RequestRevision { message } => {
            ApprovalDecision::Deny { reason: message }
        }
    }
}
