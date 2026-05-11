//! M11 plan artifact types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::options::SessionId;
use crate::permission::PermissionKind;
use crate::run_id::RunId;
use crate::usage::TokenUsage;

/// Stable, opaque plan identifier of the form `plan_<lowercase-hex>`.
#[derive(Debug, Clone, Hash, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanId(pub String);

impl PlanId {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True iff the id is safe to use as a filesystem name component.
    pub fn is_valid_format(&self) -> bool {
        if self.0.len() < 6 || self.0.len() > 96 {
            return false;
        }
        self.0.starts_with("plan_")
            && self.0[5..]
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    }
}

impl std::fmt::Display for PlanId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for PlanId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

/// Generate a new fresh plan id. Uses a UUIDv4 for the suffix.
pub fn new_plan_id() -> PlanId {
    let raw = uuid::Uuid::new_v4().simple().to_string();
    PlanId(format!("plan_{}", raw))
}

pub const MAX_PLAN_STEPS_DEFAULT: usize = 20;
pub const MAX_PLAN_REVISIONS_DEFAULT: usize = 10;
pub const MAX_PLAN_BYTES_DEFAULT: usize = 128 * 1024;

/// A workspace-relative path that has been validated to be free of `..`,
/// absolute prefixes, and Windows drive letters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRelativePath(String);

impl WorkspaceRelativePath {
    /// Validate and normalize a candidate path. Rejects absolute paths,
    /// `..` segments, drive prefixes, and embedded NULs.
    ///
    /// Normalization (`\` → `/`) happens **before** component validation so
    /// inputs like `..\\secret`, which Unix's `Path::components` would
    /// classify as a single `Normal` component, are properly broken into
    /// `..` + `secret` and rejected. Without this ordering, a
    /// backslash-encoded `..` would slip past the traversal guard on Unix
    /// and be silently rewritten to a real `../` path on disk.
    pub fn try_new(value: impl Into<String>) -> Result<Self, String> {
        let raw: String = value.into();
        if raw.is_empty() {
            return Err("path is empty".into());
        }
        if raw.contains('\0') {
            return Err("path contains NUL".into());
        }
        // Normalize FIRST so `..\\foo` is treated as `../foo` on every
        // platform. Then validate components.
        let normalized: String = raw.replace('\\', "/");
        let p = std::path::Path::new(&normalized);
        if p.is_absolute() {
            return Err(format!("path must be relative: {raw}"));
        }
        for comp in p.components() {
            use std::path::Component;
            match comp {
                Component::Normal(_) | Component::CurDir => {}
                _ => return Err(format!("path contains forbidden component: {raw}")),
            }
        }
        Ok(Self(normalized))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> PathBuf {
        PathBuf::from(&self.0)
    }
}

/// Lifecycle status of a plan document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    PendingApproval,
    Approved,
    Rejected,
    Executing,
    Completed,
    Failed,
    NeedsReplan,
    Superseded,
    Archived,
}

impl PlanStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::PendingApproval => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Executing => "executing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::NeedsReplan => "needs_replan",
            Self::Superseded => "superseded",
            Self::Archived => "archived",
        }
    }

    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "draft" => Self::Draft,
            "pending" | "pending_approval" | "pending-approval" => Self::PendingApproval,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "executing" | "running" => Self::Executing,
            "completed" | "complete" => Self::Completed,
            "failed" => Self::Failed,
            "needs_replan" | "needs-replan" | "replan" => Self::NeedsReplan,
            "superseded" => Self::Superseded,
            "archived" => Self::Archived,
            "all" | "" => return None,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCreatedBy {
    User,
    Agent,
    RevisedByUser,
    RevisedByAgent,
    Imported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Skipped,
    Blocked,
    NeedsReplan,
}

impl PlanStepStatus {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "pending" => Self::Pending,
            "in_progress" | "in-progress" => Self::InProgress,
            "completed" | "complete" | "done" => Self::Completed,
            "failed" => Self::Failed,
            "skipped" => Self::Skipped,
            "blocked" => Self::Blocked,
            "needs_replan" | "needs-replan" | "replan" => Self::NeedsReplan,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepKind {
    Inspect,
    Analyze,
    Edit,
    Execute,
    Validate,
    Delegate,
    External,
    Other,
}

impl PlanStepKind {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "inspect" | "read" | "look" => Self::Inspect,
            "analyze" | "analyse" | "think" => Self::Analyze,
            "edit" | "modify" | "change" => Self::Edit,
            "execute" | "run" | "command" => Self::Execute,
            "validate" | "verify" | "test" => Self::Validate,
            "delegate" | "subagent" => Self::Delegate,
            "external" | "mcp" => Self::External,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanRiskLevel {
    Low,
    Medium,
    High,
    External,
}

impl PlanRiskLevel {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "medium" | "med" => Self::Medium,
            "high" => Self::High,
            "external" => Self::External,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanFileAccess {
    Read,
    Write,
    Edit,
    Create,
    Delete,
}

impl PlanFileAccess {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "read" => Self::Read,
            "write" => Self::Write,
            "edit" | "modify" => Self::Edit,
            "create" => Self::Create,
            "delete" | "remove" => Self::Delete,
            _ => return None,
        })
    }

    pub fn is_mutating(self) -> bool {
        !matches!(self, Self::Read)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanFileRef {
    pub path: WorkspaceRelativePath,
    pub access: PlanFileAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanToolUse {
    pub tool_name: String,
    pub permission_kind: PermissionKind,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub input_constraints: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCommand {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command_regex: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<WorkspaceRelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanValidation {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub description: String,
    #[serde(default = "default_true")]
    pub required: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRisk {
    pub level: PlanRiskLevel,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mitigation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanContextItem {
    pub source: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanCheckpointExpectation {
    /// Free-form description of the checkpoint the executor should produce
    /// (or the existing checkpoint id it should rely on).
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: PlanStepStatus,
    pub kind: PlanStepKind,
    #[serde(default)]
    pub expected_tools: Vec<PlanToolUse>,
    #[serde(default)]
    pub expected_files: Vec<PlanFileRef>,
    #[serde(default)]
    pub expected_commands: Vec<PlanCommand>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_outcome: Option<String>,
    pub risk: PlanRiskLevel,
    #[serde(default)]
    pub requires_user_approval: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<PlanValidation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanApprover {
    LocalUser,
    CliFlag,
    TestHarness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlanApprovalScope {
    /// Approves the entire plan version.
    FullPlan,
    /// Approves only the listed step ids; other steps remain pending.
    Steps { step_ids: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanApproval {
    #[serde(with = "time::serde::rfc3339")]
    pub approved_at: OffsetDateTime,
    pub approved_by: PlanApprover,
    pub approved_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    pub scope: PlanApprovalScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
    NeedsReplan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanExecutionSummary {
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "rfc3339_opt")]
    pub completed_at: Option<OffsetDateTime>,
    pub status: PlanExecutionStatus,
    #[serde(default)]
    pub completed_steps: Vec<String>,
    #[serde(default)]
    pub failed_steps: Vec<String>,
    #[serde(default)]
    pub deviations: Vec<crate::plan::deviation::PlanDeviation>,
    #[serde(default)]
    pub validations: Vec<ValidationReport>,
    #[serde(default)]
    pub checkpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<TokenUsage>,
}

mod rfc3339_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use time::OffsetDateTime;

    pub fn serialize<S>(v: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match v {
            Some(dt) => time::serde::rfc3339::serialize(dt, s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(d: D) -> Result<Option<OffsetDateTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let opt: Option<String> = Option::deserialize(d)?;
        match opt {
            None => Ok(None),
            Some(s) => OffsetDateTime::parse(&s, &time::format_description::well_known::Rfc3339)
                .map(Some)
                .map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The full structured plan document persisted in the plan store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanDocument {
    pub id: PlanId,
    pub version: u32,
    pub status: PlanStatus,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_plan_id: Option<PlanId>,

    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub created_by: PlanCreatedBy,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    pub title: String,
    #[serde(default)]
    pub user_request: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub goal: String,

    #[serde(default)]
    pub assumptions: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub context_gathered: Vec<PlanContextItem>,
    #[serde(default)]
    pub files: Vec<PlanFileRef>,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub requested_tools: Vec<PlanToolUse>,
    #[serde(default)]
    pub validations: Vec<PlanValidation>,
    #[serde(default)]
    pub checkpoints: Vec<PlanCheckpointExpectation>,
    #[serde(default)]
    pub risks: Vec<PlanRisk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_plan: Option<String>,
    #[serde(default)]
    pub success_criteria: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<PlanApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<PlanExecutionSummary>,

    /// Free-form metadata. Always passes through redaction before persistence.
    #[serde(default)]
    pub metadata: serde_json::Value,

    /// Set by the runtime when the model returns a textual plan instead of
    /// calling SubmitPlan. Tests use this to assert the unstructured fallback.
    #[serde(default)]
    pub unstructured_warning: Option<String>,
}

impl PlanDocument {
    /// Brand-new plan document with an empty plan body. Callers populate
    /// fields and persist via the plan store.
    pub fn new_draft(
        id: PlanId,
        title: impl Into<String>,
        user_request: impl Into<String>,
    ) -> Self {
        let now = OffsetDateTime::now_utc();
        Self {
            id,
            version: 1,
            status: PlanStatus::Draft,
            session_id: None,
            run_id: None,
            parent_plan_id: None,
            created_at: now,
            updated_at: now,
            created_by: PlanCreatedBy::Agent,
            provider: None,
            model: None,
            title: title.into(),
            user_request: user_request.into(),
            summary: String::new(),
            goal: String::new(),
            assumptions: Vec::new(),
            constraints: Vec::new(),
            context_gathered: Vec::new(),
            files: Vec::new(),
            steps: Vec::new(),
            requested_tools: Vec::new(),
            validations: Vec::new(),
            checkpoints: Vec::new(),
            risks: Vec::new(),
            rollback_plan: None,
            success_criteria: Vec::new(),
            approval: None,
            execution: None,
            metadata: serde_json::Value::Null,
            unstructured_warning: None,
        }
    }

    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    pub fn approved(&self) -> bool {
        matches!(
            self.status,
            PlanStatus::Approved | PlanStatus::Executing | PlanStatus::Completed
        )
    }

    pub fn summary(&self) -> PlanSummary {
        PlanSummary {
            id: self.id.clone(),
            version: self.version,
            status: self.status,
            title: self.title.clone(),
            session_id: self.session_id.clone(),
            created_at: self.created_at,
            updated_at: self.updated_at,
            step_count: self.steps.len(),
            approved: self.approved(),
        }
    }
}

/// Compact summary returned by listings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSummary {
    pub id: PlanId,
    pub version: u32,
    pub status: PlanStatus,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub step_count: usize,
    pub approved: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Plan id format check accepts uuid-style suffixes and rejects path traversal.
    fn plan_id_format() {
        assert!(new_plan_id().is_valid_format());
        assert!(PlanId("plan_abc-123".into()).is_valid_format());
        assert!(!PlanId("plan_..".into()).is_valid_format());
        assert!(!PlanId("foo".into()).is_valid_format());
        assert!(!PlanId("plan_/x".into()).is_valid_format());
    }

    #[test]
    // Workspace-relative path strips backslashes and rejects absolute paths.
    fn workspace_path_validation() {
        assert!(WorkspaceRelativePath::try_new("src/lib.rs").is_ok());
        assert_eq!(
            WorkspaceRelativePath::try_new("src\\lib.rs")
                .unwrap()
                .as_str(),
            "src/lib.rs"
        );
        assert!(WorkspaceRelativePath::try_new("/etc/passwd").is_err());
        assert!(WorkspaceRelativePath::try_new("../escape").is_err());
        assert!(WorkspaceRelativePath::try_new("").is_err());
    }

    #[test]
    // Backslash-encoded `..` is rejected: validation runs AFTER `\` → `/`
    // normalization, so `..\\secret` (which Unix's `Path::components`
    // would otherwise treat as a single Normal segment) is properly
    // broken into `..` + `secret` and refused.
    fn backslash_encoded_traversal_rejected() {
        assert!(WorkspaceRelativePath::try_new("..\\secret").is_err());
        assert!(WorkspaceRelativePath::try_new("a\\..\\b").is_err());
        assert!(WorkspaceRelativePath::try_new("\\etc\\passwd").is_err());
        // Plain backslash still normalizes to forward-slash for safe paths.
        assert_eq!(
            WorkspaceRelativePath::try_new("a\\b\\c").unwrap().as_str(),
            "a/b/c"
        );
    }

    #[test]
    // PlanDocument serializes/deserializes without losing fields.
    fn plan_round_trip() {
        let mut p = PlanDocument::new_draft(new_plan_id(), "Title", "Original ask");
        p.steps.push(PlanStep {
            id: "1".into(),
            title: "Inspect".into(),
            description: "look at parser".into(),
            status: PlanStepStatus::Pending,
            kind: PlanStepKind::Inspect,
            expected_tools: Vec::new(),
            expected_files: Vec::new(),
            expected_commands: Vec::new(),
            expected_outcome: None,
            risk: PlanRiskLevel::Low,
            requires_user_approval: false,
            validation: None,
        });
        let s = serde_json::to_string(&p).unwrap();
        let back: PlanDocument = serde_json::from_str(&s).unwrap();
        assert_eq!(back.id.as_str(), p.id.as_str());
        assert_eq!(back.steps.len(), 1);
        assert_eq!(back.steps[0].kind, PlanStepKind::Inspect);
    }
}
