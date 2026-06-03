//! Programmatic plan/act API.

use std::time::Duration;

use code_agent_core::plan::{PlanDocument, PlanningMode};
use code_agent_core::TokenUsage;

use crate::budget::RunBudgets;
use crate::error::Diagnostic;
use crate::provider::ProviderSelection;
use crate::request::{ApprovalPolicy, PatchPolicy, RequestMetadata, ValidationPolicy};
use crate::result::EventSummary;
use crate::workspace::WorkspaceSource;

/// Standalone plan request — produces a `PlanDocument` without acting.
#[derive(Debug)]
pub struct PlanRequest {
    pub task: String,
    pub workspace: Option<WorkspaceSource>,
    pub planning_mode: PlanningMode,
    pub max_steps: usize,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub metadata: RequestMetadata,
    pub budgets: RunBudgets,
}

impl PlanRequest {
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            workspace: None,
            planning_mode: PlanningMode::default(),
            max_steps: 16,
            provider: None,
            model: None,
            metadata: RequestMetadata::default(),
            budgets: RunBudgets::default(),
        }
    }

    pub fn workspace(mut self, ws: WorkspaceSource) -> Self {
        self.workspace = Some(ws);
        self
    }

    pub fn planning_mode(mut self, mode: PlanningMode) -> Self {
        self.planning_mode = mode;
        self
    }

    pub fn max_steps(mut self, n: usize) -> Self {
        self.max_steps = n;
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

    pub fn budgets(mut self, b: RunBudgets) -> Self {
        self.budgets = b;
        self
    }

    pub fn metadata(mut self, m: RequestMetadata) -> Self {
        self.metadata = m;
        self
    }

    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.metadata.request_id = Some(id.into());
        self
    }
}

/// Result of [`crate::AgentRuntime::plan`].
#[derive(Debug)]
pub struct PlanResult {
    pub plan: PlanDocument,
    pub diagnostics: Vec<Diagnostic>,
    pub usage: Option<TokenUsage>,
    pub events_summary: EventSummary,
}

/// Inputs to [`crate::AgentRuntime::act`].
#[derive(Debug)]
pub struct ActRequest {
    pub plan: PlanDocument,
    pub workspace: Option<WorkspaceSource>,
    pub patch_policy: PatchPolicy,
    pub approval_policy: ApprovalPolicy,
    pub validation: ValidationPolicy,
    pub provider: Option<ProviderSelection>,
    pub model: Option<String>,
    pub metadata: RequestMetadata,
    pub budgets: RunBudgets,
    /// Force-approve the plan internally. Intended for tests: allows
    /// `act()` to proceed even when the plan is in a non-approved status.
    pub test_force_approval: bool,
    pub deadline: Option<Duration>,
}

impl ActRequest {
    pub fn from_plan(plan: PlanDocument) -> Self {
        Self {
            plan,
            workspace: None,
            patch_policy: PatchPolicy::ReturnOnly,
            approval_policy: ApprovalPolicy::CallbackRequired,
            validation: ValidationPolicy::default(),
            provider: None,
            model: None,
            metadata: RequestMetadata::default(),
            budgets: RunBudgets::default(),
            test_force_approval: false,
            deadline: None,
        }
    }

    pub fn workspace(mut self, ws: WorkspaceSource) -> Self {
        self.workspace = Some(ws);
        self
    }

    pub fn patch_policy(mut self, p: PatchPolicy) -> Self {
        self.patch_policy = p;
        self
    }

    pub fn approval_policy(mut self, p: ApprovalPolicy) -> Self {
        self.approval_policy = p;
        self
    }

    pub fn validation(mut self, v: ValidationPolicy) -> Self {
        self.validation = v;
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

    pub fn budgets(mut self, b: RunBudgets) -> Self {
        self.budgets = b;
        self
    }

    pub fn metadata(mut self, m: RequestMetadata) -> Self {
        self.metadata = m;
        self
    }

    pub fn deadline(mut self, d: Duration) -> Self {
        self.deadline = Some(d);
        self
    }

    /// Internal/test escape hatch. In production, drive approval through the
    /// configured [`ApprovalPolicy`] (e.g. [`ApprovalPolicy::CallbackRequired`])
    /// so the plan goes through the approval pipeline.
    pub fn test_force_approval(mut self) -> Self {
        self.test_force_approval = true;
        self
    }
}
