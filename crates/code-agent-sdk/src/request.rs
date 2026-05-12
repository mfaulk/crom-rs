//! AgentRequest and supporting policy types.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

use code_agent_core::plan::PlanId;

use crate::budget::RunBudgets;
use crate::policy::InteractivityMode;
use crate::provider::ProviderSelection;
use crate::workspace::WorkspaceSource;

/// What the agent should do for this request. The runtime validates each
/// mode against `PatchPolicy`/`WorkspaceCapabilities` before starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMode {
    /// Plan-only: produce a plan, no mutation.
    PlanOnly,
    /// Generate a patch without applying it. Default — safest.
    #[default]
    GeneratePatch,
    /// Apply a patch to the workspace.
    ApplyPatch,
    /// Mutate the workspace directly (Write/Edit/Bash subject to policy).
    ModifyWorkspace,
    /// Review-only: read & analyze, produce notes.
    ReviewOnly,
    /// Run validation only, no mutation.
    ValidateOnly,
    /// Plan, then act on the plan.
    PlanThenAct,
}

impl AgentMode {
    pub fn permits_mutation(self) -> bool {
        matches!(
            self,
            Self::ApplyPatch | Self::ModifyWorkspace | Self::PlanThenAct
        )
    }
}

/// How patches surface to the application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchPolicy {
    /// Return the patch only, never mutate the workspace.
    #[default]
    ReturnOnly,
    /// Apply the patch to the workspace.
    ApplyToWorkspace,
    /// Apply the patch AND return it.
    ApplyAndReturn,
    /// Refuse to even compute mutations.
    RejectMutations,
}

impl PatchPolicy {
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::ApplyToWorkspace | Self::ApplyAndReturn)
    }
}

/// Approval-callback policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    /// Never ask; deny anything mutating.
    NeverAskDeny,
    /// Never ask; allow safe (read-shaped) operations.
    NeverAskAllowSafe,
    /// Require the callback to decide. Default — host-aware.
    #[default]
    CallbackRequired,
    /// Auto-approve read-only side effects only.
    AutoApproveReadOnly,
    /// Auto-approve operations within the approved plan scope.
    AutoApprovePlanScope,
}

/// Session persistence policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPolicy {
    /// No session: stateless, no events stored.
    #[default]
    None,
    /// In-memory session: kept for the lifetime of the runtime.
    InMemory,
    /// Persist session to the configured storage backend.
    Persisted,
    /// Resume an existing session id.
    Existing(crate::SessionId),
    /// Host-managed: SDK does not own the session record.
    HostManaged,
}

/// Plan-mode policy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPolicy {
    /// No plan: act directly.
    #[default]
    None,
    /// Always plan, but plan output is the final result.
    Required,
    /// Plan if the runtime decides to.
    Optional,
    /// Plan first, then act.
    PlanThenAct,
    /// Reuse an existing plan id.
    UseExisting(PlanId),
}

/// Validation policy: which shell commands to run after the agent finishes
/// (when filesystem execution is enabled).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationPolicy {
    pub commands: Vec<String>,
    pub run_after_completion: bool,
    pub require_success: bool,
    #[serde(with = "duration_serde")]
    pub timeout: Duration,
    pub output_limit_bytes: usize,
}

impl Default for ValidationPolicy {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            run_after_completion: false,
            require_success: false,
            timeout: Duration::from_secs(120),
            output_limit_bytes: 32_768,
        }
    }
}

impl ValidationPolicy {
    pub fn with_command(mut self, command: impl Into<String>) -> Self {
        self.commands.push(command.into());
        self.run_after_completion = true;
        self
    }
}

/// Request metadata. Propagated to events, diagnostics, logs, and (where
/// appropriate) provider metadata. Tenant/user identifiers may be redacted
/// by the configured `SecretRedactor`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetadata {
    pub tenant_id: Option<String>,
    pub user_id: Option<String>,
    pub request_id: Option<String>,
    pub trace_id: Option<String>,
    pub labels: BTreeMap<String, String>,
}

impl RequestMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = Some(id.into());
        self
    }
}

/// Top-level request to [`crate::runtime::AgentRuntime`].
#[derive(Debug)]
pub struct AgentRequest {
    pub task: String,
    pub mode: AgentMode,
    pub workspace: Option<WorkspaceSource>,
    pub session_policy: SessionPolicy,
    pub plan_policy: PlanPolicy,
    pub patch_policy: PatchPolicy,
    pub validation: ValidationPolicy,
    pub approval_policy: ApprovalPolicy,
    pub interactivity: Option<InteractivityMode>,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub deadline: Option<Duration>,
    pub budgets: RunBudgets,
    pub metadata: RequestMetadata,
}

impl AgentRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            mode: AgentMode::default(),
            workspace: None,
            session_policy: SessionPolicy::default(),
            plan_policy: PlanPolicy::default(),
            patch_policy: PatchPolicy::default(),
            validation: ValidationPolicy::default(),
            approval_policy: ApprovalPolicy::default(),
            interactivity: None,
            provider: None,
            model: None,
            max_turns: None,
            deadline: None,
            budgets: RunBudgets::default(),
            metadata: RequestMetadata::default(),
        }
    }

    pub fn mode(mut self, mode: AgentMode) -> Self {
        self.mode = mode;
        self
    }
    pub fn workspace(mut self, w: WorkspaceSource) -> Self {
        self.workspace = Some(w);
        self
    }
    pub fn session_policy(mut self, p: SessionPolicy) -> Self {
        self.session_policy = p;
        self
    }
    pub fn plan_policy(mut self, p: PlanPolicy) -> Self {
        self.plan_policy = p;
        self
    }
    pub fn patch_policy(mut self, p: PatchPolicy) -> Self {
        self.patch_policy = p;
        self
    }
    pub fn validation(mut self, v: ValidationPolicy) -> Self {
        self.validation = v;
        self
    }
    pub fn validation_command(mut self, cmd: impl Into<String>) -> Self {
        self.validation = self.validation.with_command(cmd);
        self
    }
    pub fn approval_policy(mut self, p: ApprovalPolicy) -> Self {
        self.approval_policy = p;
        self
    }
    pub fn interactivity(mut self, m: InteractivityMode) -> Self {
        self.interactivity = Some(m);
        self
    }
    pub fn provider(mut self, p: ProviderSelection) -> Self {
        self.provider = Some(p);
        self
    }
    pub fn model(mut self, m: impl Into<String>) -> Self {
        self.model = Some(m.into());
        self
    }
    pub fn max_turns(mut self, n: u32) -> Self {
        self.max_turns = Some(n);
        self
    }
    pub fn deadline(mut self, d: Duration) -> Self {
        self.deadline = Some(d);
        self
    }
    pub fn budgets(mut self, b: RunBudgets) -> Self {
        self.budgets = b;
        self
    }
    pub fn tenant_id(mut self, id: impl Into<String>) -> Self {
        self.metadata.tenant_id = Some(id.into());
        self
    }
    pub fn user_id(mut self, id: impl Into<String>) -> Self {
        self.metadata.user_id = Some(id.into());
        self
    }
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.metadata.request_id = Some(id.into());
        self
    }
    pub fn trace_id(mut self, id: impl Into<String>) -> Self {
        self.metadata.trace_id = Some(id.into());
        self
    }
    pub fn label(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.metadata.labels.insert(k.into(), v.into());
        self
    }

    /// Validate the request consistency. Called by the runtime at the
    /// start of every run.
    pub fn validate(&self) -> Result<(), crate::error::AgentSdkError> {
        use crate::error::AgentSdkError;
        if self.task.trim().is_empty() {
            return Err(AgentSdkError::invalid_request(
                "request.empty_task",
                "agent task is empty",
            ));
        }
        if self.mode == AgentMode::PlanOnly && self.patch_policy.allows_mutation() {
            return Err(AgentSdkError::invalid_request(
                "request.invalid_mode_combo",
                "plan-only mode is incompatible with applying patches",
            ));
        }
        if matches!(self.patch_policy, PatchPolicy::RejectMutations) && self.mode.permits_mutation()
        {
            return Err(AgentSdkError::invalid_request(
                "request.invalid_mode_combo",
                "patch_policy=RejectMutations conflicts with mutating mode",
            ));
        }
        Ok(())
    }
}

mod duration_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let ms = u64::deserialize(d)?;
        Ok(Duration::from_millis(ms))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AgentRequest::new sets defaults: GeneratePatch, ReturnOnly, no metadata.
    #[test]
    fn new_sets_safe_defaults() {
        let r = AgentRequest::new("fix bug");
        assert_eq!(r.mode, AgentMode::GeneratePatch);
        assert_eq!(r.patch_policy, PatchPolicy::ReturnOnly);
        assert_eq!(r.metadata.request_id, None);
    }

    /// AgentRequest builder methods compose.
    #[test]
    fn builder_methods_compose() {
        let r = AgentRequest::new("x")
            .max_turns(3)
            .tenant_id("t1")
            .user_id("u1")
            .request_id("r1")
            .mode(AgentMode::PlanOnly);
        assert_eq!(r.max_turns, Some(3));
        assert_eq!(r.metadata.tenant_id.as_deref(), Some("t1"));
        assert_eq!(r.metadata.user_id.as_deref(), Some("u1"));
        assert_eq!(r.metadata.request_id.as_deref(), Some("r1"));
        assert_eq!(r.mode, AgentMode::PlanOnly);
    }

    /// AgentRequest::validate rejects PlanOnly + ApplyToWorkspace.
    #[test]
    fn rejects_plan_only_with_apply() {
        let r = AgentRequest::new("x")
            .mode(AgentMode::PlanOnly)
            .patch_policy(PatchPolicy::ApplyToWorkspace);
        assert!(r.validate().is_err());
    }

    /// AgentRequest::validate rejects RejectMutations + ApplyPatch.
    #[test]
    fn rejects_reject_with_mutate() {
        let r = AgentRequest::new("x")
            .mode(AgentMode::ApplyPatch)
            .patch_policy(PatchPolicy::RejectMutations);
        assert!(r.validate().is_err());
    }

    /// AgentRequest::validate rejects empty task.
    #[test]
    fn rejects_empty_task() {
        let r = AgentRequest::new("   ");
        assert!(r.validate().is_err());
    }
}
