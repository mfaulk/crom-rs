//! Internal tools that drive plan generation (`SubmitPlan`) and plan-step
//! progress tracking during act mode (`PlanUpdate`). These tools never
//! touch external systems — they update an in-memory plan handle the
//! runtime owns and persists.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::sync::Mutex;

use crate::errors::ToolError;
use crate::permission::PermissionKind;
use crate::tool::{Tool, ToolContent, ToolContext, ToolResult};

use super::artifact::{
    PlanCheckpointExpectation, PlanCommand, PlanContextItem, PlanCreatedBy, PlanDocument,
    PlanFileAccess, PlanFileRef, PlanRisk, PlanRiskLevel, PlanStatus, PlanStep, PlanStepKind,
    PlanStepStatus, PlanToolUse, PlanValidation, WorkspaceRelativePath,
};
use super::deviation::redact_secrets_in_plan;
use super::errors::PlanError;
use super::options::PlanningOptions;

pub const SUBMIT_PLAN_TOOL_NAME: &str = "SubmitPlan";
pub const PLAN_UPDATE_TOOL_NAME: &str = "PlanUpdate";

#[derive(Debug, Clone)]
pub struct SubmitPlanResult {
    pub plan: PlanDocument,
    pub warning: Option<String>,
}

/// Mutable handle the runtime hands to the SubmitPlan tool. The tool
/// produces a structured plan and stores it in the handle; the planning
/// driver then persists it.
#[derive(Debug, Default, Clone)]
pub struct SubmitPlanHandle {
    inner: Arc<Mutex<Option<SubmitPlanResult>>>,
}

impl SubmitPlanHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> Option<SubmitPlanResult> {
        self.inner.lock().await.clone()
    }

    pub async fn take(&self) -> Option<SubmitPlanResult> {
        self.inner.lock().await.take()
    }

    async fn store(&self, value: SubmitPlanResult) {
        *self.inner.lock().await = Some(value);
    }
}

/// Internal tool exposed to the model during planning mode. Validates the
/// structured plan input and stores it in the handle for the runtime to
/// pick up. Never modifies the workspace.
pub struct SubmitPlanTool {
    base_plan: Arc<Mutex<PlanDocument>>,
    handle: SubmitPlanHandle,
    options: PlanningOptions,
}

impl SubmitPlanTool {
    pub fn new(
        base_plan: PlanDocument,
        handle: SubmitPlanHandle,
        options: PlanningOptions,
    ) -> Self {
        Self {
            base_plan: Arc::new(Mutex::new(base_plan)),
            handle,
            options,
        }
    }

    pub fn handle(&self) -> SubmitPlanHandle {
        self.handle.clone()
    }
}

#[async_trait]
impl Tool for SubmitPlanTool {
    fn name(&self) -> &str {
        SUBMIT_PLAN_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Submit a structured plan describing how you propose to accomplish the user's request. \
         Used during planning mode. Each step must list the tools, files, commands, and \
         validations you expect to run. Do not request mutating tools during planning."
    }

    fn input_schema(&self) -> Value {
        submit_plan_tool_definition()
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &Value) -> String {
        let n = input
            .get("steps")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        format!("SubmitPlan ({n} steps)")
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let parsed: SubmitPlanInput = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("SubmitPlan input: {e}")))?;
        let mut plan = self.base_plan.lock().await.clone();
        let result = parsed
            .into_plan(&mut plan, &self.options)
            .map_err(|e| ToolError::InvalidInput(format!("SubmitPlan validation: {e}")))?;
        // Persist the submitted plan in the handle.
        self.handle.store(result.clone()).await;
        let summary = render_plan_pretty(&result.plan);
        Ok(ToolResult {
            content: vec![ToolContent::Text(summary)],
            is_error: false,
            metadata: json!({
                "plan_id": result.plan.id.0,
                "version": result.plan.version,
                "steps": result.plan.steps.len(),
                "warning": result.warning,
            }),
        })
    }
}

/// Public schema of the SubmitPlan tool.
pub fn submit_plan_tool_definition() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string"},
            "summary": {"type": "string"},
            "goal": {"type": "string"},
            "assumptions": {"type": "array", "items": {"type": "string"}},
            "constraints": {"type": "array", "items": {"type": "string"}},
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "access": {"type": "string"},
                        "reason": {"type": "string"}
                    },
                    "required": ["path", "access"]
                }
            },
            "steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "string"},
                        "title": {"type": "string"},
                        "description": {"type": "string"},
                        "kind": {"type": "string"},
                        "expected_tools": {"type": "array"},
                        "expected_files": {"type": "array"},
                        "expected_commands": {"type": "array"},
                        "risk": {"type": "string"},
                        "requires_user_approval": {"type": "boolean"}
                    },
                    "required": ["id", "title", "kind"]
                }
            },
            "requested_tools": {"type": "array"},
            "validations": {"type": "array"},
            "checkpoints": {"type": "array"},
            "risks": {"type": "array"},
            "rollback_plan": {"type": "string"},
            "success_criteria": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["title", "steps"]
    })
}

/// Public schema of the PlanUpdate tool.
pub fn plan_update_tool_definition() -> Value {
    json!({
        "type": "object",
        "properties": {
            "step_id": {"type": "string"},
            "status": {"type": "string"},
            "message": {"type": "string"}
        },
        "required": ["step_id", "status"]
    })
}

#[derive(Debug, Deserialize)]
struct SubmitPlanInput {
    title: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    assumptions: Vec<String>,
    #[serde(default)]
    constraints: Vec<String>,
    #[serde(default)]
    files: Vec<RawFileRef>,
    steps: Vec<RawStep>,
    #[serde(default)]
    requested_tools: Vec<RawToolUse>,
    #[serde(default)]
    validations: Vec<RawValidation>,
    #[serde(default)]
    checkpoints: Vec<RawCheckpoint>,
    #[serde(default)]
    risks: Vec<RawRisk>,
    #[serde(default)]
    rollback_plan: Option<String>,
    #[serde(default)]
    success_criteria: Vec<String>,
    #[serde(default)]
    context_gathered: Vec<RawContextItem>,
}

#[derive(Debug, Deserialize)]
struct RawFileRef {
    path: String,
    access: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    kind: String,
    #[serde(default)]
    expected_tools: Vec<RawToolUse>,
    #[serde(default)]
    expected_files: Vec<RawFileRef>,
    #[serde(default)]
    expected_commands: Vec<RawCommand>,
    #[serde(default)]
    expected_outcome: Option<String>,
    #[serde(default = "default_risk")]
    risk: String,
    #[serde(default)]
    requires_user_approval: bool,
    #[serde(default)]
    validation: Option<RawValidation>,
}

fn default_risk() -> String {
    "low".into()
}

#[derive(Debug, Deserialize)]
struct RawToolUse {
    tool_name: String,
    #[serde(default)]
    permission_kind: Option<String>,
    #[serde(default)]
    input_constraints: Option<Value>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawCommand {
    command: String,
    #[serde(default)]
    command_regex: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawValidation {
    #[serde(default)]
    command: Option<String>,
    description: String,
    #[serde(default = "default_required")]
    required: bool,
}

fn default_required() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawCheckpoint {
    description: String,
    #[serde(default)]
    checkpoint_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawRisk {
    level: String,
    description: String,
    #[serde(default)]
    mitigation: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawContextItem {
    source: String,
    summary: String,
    #[serde(default)]
    bytes_read: Option<u64>,
}

impl SubmitPlanInput {
    fn into_plan(
        self,
        base: &mut PlanDocument,
        options: &PlanningOptions,
    ) -> Result<SubmitPlanResult, PlanError> {
        if self.title.trim().is_empty() {
            return Err(PlanError::Invalid("plan title cannot be empty".into()));
        }
        if self.steps.is_empty() {
            return Err(PlanError::Invalid(
                "plan must have at least one step".into(),
            ));
        }
        if self.steps.len() > options.max_plan_steps {
            return Err(PlanError::TooManySteps {
                got: self.steps.len(),
                limit: options.max_plan_steps,
            });
        }

        base.title = self.title;
        base.summary = self.summary;
        base.goal = self.goal;
        base.assumptions = self.assumptions;
        base.constraints = self.constraints;
        base.context_gathered = self
            .context_gathered
            .into_iter()
            .map(|c| PlanContextItem {
                source: c.source,
                summary: c.summary,
                bytes_read: c.bytes_read,
            })
            .collect();
        base.files = parse_files(self.files)?;
        base.steps = parse_steps(self.steps, options)?;
        base.requested_tools = parse_tools(self.requested_tools)?;
        base.validations = self
            .validations
            .into_iter()
            .map(|v| PlanValidation {
                command: v.command,
                description: v.description,
                required: v.required,
            })
            .collect();
        base.checkpoints = self
            .checkpoints
            .into_iter()
            .map(|c| PlanCheckpointExpectation {
                description: c.description,
                checkpoint_id: c.checkpoint_id,
            })
            .collect();
        base.risks = self
            .risks
            .into_iter()
            .map(|r| {
                let level =
                    PlanRiskLevel::from_str_lenient(&r.level).unwrap_or(PlanRiskLevel::Medium);
                PlanRisk {
                    level,
                    description: r.description,
                    mitigation: r.mitigation,
                }
            })
            .collect();
        base.rollback_plan = self.rollback_plan;
        base.success_criteria = self.success_criteria;
        base.status = PlanStatus::PendingApproval;
        base.updated_at = OffsetDateTime::now_utc();
        if !matches!(
            base.created_by,
            PlanCreatedBy::User | PlanCreatedBy::Imported
        ) {
            base.created_by = PlanCreatedBy::Agent;
        }

        redact_secrets_in_plan(base);
        Ok(SubmitPlanResult {
            plan: base.clone(),
            warning: None,
        })
    }
}

fn parse_files(raws: Vec<RawFileRef>) -> Result<Vec<PlanFileRef>, PlanError> {
    let mut out = Vec::with_capacity(raws.len());
    for r in raws {
        let access = PlanFileAccess::from_str_lenient(&r.access)
            .ok_or_else(|| PlanError::Invalid(format!("invalid file access: {}", r.access)))?;
        let path = WorkspaceRelativePath::try_new(r.path).map_err(PlanError::Invalid)?;
        out.push(PlanFileRef {
            path,
            access,
            reason: r.reason,
        });
    }
    Ok(out)
}

fn parse_steps(raws: Vec<RawStep>, options: &PlanningOptions) -> Result<Vec<PlanStep>, PlanError> {
    let mut out = Vec::with_capacity(raws.len());
    for r in raws {
        if r.id.trim().is_empty() {
            return Err(PlanError::Invalid("step id is empty".into()));
        }
        if r.title.trim().is_empty() {
            return Err(PlanError::Invalid(format!(
                "step `{}` is missing a title",
                r.id
            )));
        }
        let kind = PlanStepKind::from_str_lenient(&r.kind)
            .ok_or_else(|| PlanError::Invalid(format!("invalid step kind: {}", r.kind)))?;
        let risk = PlanRiskLevel::from_str_lenient(&r.risk).unwrap_or(PlanRiskLevel::Low);
        let expected_files = parse_files(r.expected_files)?;
        let expected_commands = parse_commands(r.expected_commands)?;
        let validation = r.validation.map(|v| PlanValidation {
            command: v.command,
            description: v.description,
            required: v.required,
        });
        out.push(PlanStep {
            id: r.id,
            title: r.title,
            description: r.description,
            status: PlanStepStatus::Pending,
            kind,
            expected_tools: parse_tools(r.expected_tools)?,
            expected_files,
            expected_commands,
            expected_outcome: r.expected_outcome,
            risk,
            requires_user_approval: r.requires_user_approval,
            validation,
        });
    }
    if out.len() > options.max_plan_steps {
        return Err(PlanError::TooManySteps {
            got: out.len(),
            limit: options.max_plan_steps,
        });
    }
    Ok(out)
}

fn parse_commands(raws: Vec<RawCommand>) -> Result<Vec<PlanCommand>, PlanError> {
    let mut out = Vec::with_capacity(raws.len());
    for r in raws {
        if let Some(re) = &r.command_regex {
            // Reject inline flag groups (e.g. `(?m)`, `(?s)`, `(?x)`).
            // Defense against newline-injection bypass: a planned
            // `(?m)^cargo test$` paired with the classifier's anchoring
            // becomes `^(?m)^cargo test$$`, which matches
            // `cargo test\nrm -rf ~/` because `(?m)` re-interprets the
            // anchors as line anchors. Plan authors don't need flag
            // groups; reject any occurrence of `(?` to keep the rule
            // simple and broad.
            if re.contains("(?") {
                return Err(PlanError::Invalid(format!(
                    "command_regex must not contain inline flag groups (`(?`): {re}"
                )));
            }
            // Validate regex eagerly so invalid expressions are surfaced now,
            // not at deviation-classification time.
            regex::Regex::new(re)
                .map_err(|e| PlanError::Invalid(format!("invalid command_regex: {e}")))?;
        }
        let cwd = match r.cwd {
            Some(c) => Some(WorkspaceRelativePath::try_new(c).map_err(PlanError::Invalid)?),
            None => None,
        };
        out.push(PlanCommand {
            command: r.command,
            command_regex: r.command_regex,
            cwd,
            reason: r.reason,
        });
    }
    Ok(out)
}

fn parse_tools(raws: Vec<RawToolUse>) -> Result<Vec<PlanToolUse>, PlanError> {
    let mut out = Vec::with_capacity(raws.len());
    for r in raws {
        validate_tool_name_pattern(&r.tool_name)?;
        out.push(PlanToolUse {
            tool_name: r.tool_name,
            permission_kind: parse_permission_kind(r.permission_kind.as_deref()),
            input_constraints: r.input_constraints.unwrap_or(Value::Null),
            reason: r.reason,
        });
    }
    Ok(out)
}

/// Reject bare wildcard tool names. The plan-scope classifier compiles
/// `tool_name` as a glob; `"*"` and `"**"` match every tool name and
/// would silently authorize tools that the plan never enumerated. The
/// remaining classifier paths fall through to `Ambiguous` for tools
/// without a structural constraint, so a blanket wildcard combined with
/// a non-strict deviation policy effectively turns plan-scope into a
/// no-op. Force authors to use either an exact name or a non-empty
/// prefix like `mcp__github__*`.
fn validate_tool_name_pattern(name: &str) -> Result<(), PlanError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(PlanError::Invalid("tool_name is empty".into()));
    }
    if trimmed == "*" || trimmed == "**" {
        return Err(PlanError::Invalid(format!(
            "tool_name `{trimmed}` is a bare wildcard; use an exact name or a non-empty prefix \
             like `mcp__github__*`"
        )));
    }
    // Reject a leading `*` (`*foo`, `*` was already caught) — a leading
    // wildcard would still match every tool starting with anything.
    if trimmed.starts_with('*') {
        return Err(PlanError::Invalid(format!(
            "tool_name `{trimmed}` starts with a wildcard; require a non-empty literal prefix"
        )));
    }
    Ok(())
}

fn parse_permission_kind(s: Option<&str>) -> PermissionKind {
    let s = match s {
        Some(s) => s.to_ascii_lowercase(),
        None => return PermissionKind::ReadOnly,
    };
    match s.as_str() {
        "read_only" | "read-only" | "readonly" => PermissionKind::ReadOnly,
        "mutating" | "write" | "edit" => PermissionKind::Mutating,
        "execution" | "exec" | "bash" => PermissionKind::Execution,
        "external" | "mcp" => PermissionKind::External,
        "orchestration" | "subagent" | "agent" => PermissionKind::Orchestration,
        _ => PermissionKind::ReadOnly,
    }
}

/// Best-effort attempt to convert a textual plan into a structured one.
/// Used as a fallback when the model returns prose instead of calling
/// SubmitPlan. Always sets `unstructured_warning` so the caller can flag
/// the plan in the UI.
/// Maximum number of *characters* (not bytes) retained in the fallback
/// plan summary. UTF-8 codepoints are variable width, so a byte-based cap
/// would panic when the cut falls in the middle of a multi-byte codepoint
/// (e.g. emoji at byte offset 1023).
const UNSTRUCTURED_SUMMARY_CHAR_CAP: usize = 1024;

pub fn parse_submit_plan_input(text: &str, base: PlanDocument) -> SubmitPlanResult {
    let mut plan = base;
    let trimmed = text.trim();
    plan.summary = truncate_with_ellipsis(trimmed, UNSTRUCTURED_SUMMARY_CHAR_CAP);
    if plan.title.is_empty() {
        plan.title = trimmed
            .lines()
            .next()
            .unwrap_or("Untitled plan")
            .to_string();
    }
    if plan.steps.is_empty() {
        plan.steps.push(PlanStep {
            id: "1".into(),
            title: "Execute the plan as described in the summary".into(),
            description: trimmed.to_string(),
            status: PlanStepStatus::Pending,
            kind: PlanStepKind::Other,
            expected_tools: Vec::new(),
            expected_files: Vec::new(),
            expected_commands: Vec::new(),
            expected_outcome: None,
            risk: PlanRiskLevel::Medium,
            requires_user_approval: true,
            validation: None,
        });
    }
    plan.status = PlanStatus::PendingApproval;
    plan.updated_at = OffsetDateTime::now_utc();
    plan.unstructured_warning =
        Some("plan is unstructured: model returned prose instead of calling SubmitPlan".into());
    SubmitPlanResult {
        plan,
        warning: plan_unstructured_warning_text(),
    }
}

fn plan_unstructured_warning_text() -> Option<String> {
    Some("plan is unstructured: review carefully before approving".into())
}

/// Truncate `s` to at most `max_chars` Unicode scalar values, appending
/// an ellipsis when truncation occurs. Operates on `char_indices` so the
/// cut never falls inside a multi-byte UTF-8 codepoint.
fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let mut indices = s.char_indices();
    if let Some((cut, _)) = indices.nth(max_chars) {
        let mut out = String::with_capacity(cut + 3);
        out.push_str(&s[..cut]);
        out.push('…');
        out
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlanUpdateHandle {
    inner: Arc<Mutex<Vec<PlanStepUpdateRecord>>>,
}

#[derive(Debug, Clone)]
pub struct PlanStepUpdateRecord {
    pub step_id: String,
    pub status: PlanStepStatus,
    pub message: Option<String>,
    pub at: OffsetDateTime,
}

impl PlanUpdateHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> Vec<PlanStepUpdateRecord> {
        self.inner.lock().await.clone()
    }

    async fn append(&self, record: PlanStepUpdateRecord) {
        self.inner.lock().await.push(record);
    }
}

/// Internal tool exposed to the model during act mode. Records progress in
/// the handle; the act driver consumes the records to update the plan
/// document.
pub struct PlanUpdateTool {
    handle: PlanUpdateHandle,
}

impl PlanUpdateTool {
    pub fn new(handle: PlanUpdateHandle) -> Self {
        Self { handle }
    }

    pub fn handle(&self) -> PlanUpdateHandle {
        self.handle.clone()
    }
}

#[async_trait]
impl Tool for PlanUpdateTool {
    fn name(&self) -> &str {
        PLAN_UPDATE_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Update progress on the currently-executing plan. Provide the step_id, a new status \
         (pending|in_progress|completed|failed|blocked|needs_replan), and an optional message. \
         Use this between tool calls so the user can follow along."
    }

    fn input_schema(&self) -> Value {
        plan_update_tool_definition()
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &Value) -> String {
        let id = input.get("step_id").and_then(Value::as_str).unwrap_or("?");
        let st = input.get("status").and_then(Value::as_str).unwrap_or("?");
        format!("PlanUpdate(step={id}, status={st})")
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        #[derive(Deserialize)]
        struct In {
            step_id: String,
            status: String,
            #[serde(default)]
            message: Option<String>,
        }
        let parsed: In = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("PlanUpdate input: {e}")))?;
        let status = PlanStepStatus::from_str_lenient(&parsed.status).ok_or_else(|| {
            ToolError::InvalidInput(format!("unknown step status: {}", parsed.status))
        })?;
        let record = PlanStepUpdateRecord {
            step_id: parsed.step_id.clone(),
            status,
            message: parsed.message.clone(),
            at: OffsetDateTime::now_utc(),
        };
        self.handle.append(record).await;
        let body = format!(
            "[plan-step] {} -> {:?}{}",
            parsed.step_id,
            status,
            parsed
                .message
                .as_deref()
                .map(|m| format!(": {m}"))
                .unwrap_or_default()
        );
        Ok(ToolResult {
            content: vec![ToolContent::Text(body)],
            is_error: false,
            metadata: json!({
                "step_id": parsed.step_id,
                "status": parsed.status,
                "message": parsed.message,
            }),
        })
    }
}

/// Render a plan as a multi-line plain-text summary suitable for the CLI
/// or the TUI plan panel.
pub fn render_plan_pretty(plan: &PlanDocument) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Plan {} v{} ({})\n",
        plan.id,
        plan.version,
        plan.status.label()
    ));
    s.push_str(&format!("Title: {}\n", plan.title));
    if !plan.goal.is_empty() {
        s.push_str(&format!("Goal:  {}\n", plan.goal));
    }
    if !plan.summary.is_empty() {
        s.push_str(&format!("Summary: {}\n", plan.summary));
    }
    s.push_str(&format!("Steps ({}):\n", plan.steps.len()));
    for (i, step) in plan.steps.iter().enumerate() {
        s.push_str(&format!(
            "  {}. [{:?}/{:?}] {}\n",
            i + 1,
            step.kind,
            step.risk,
            step.title
        ));
        if !step.description.is_empty() {
            s.push_str(&format!("     {}\n", step.description));
        }
        for f in &step.expected_files {
            s.push_str(&format!(
                "     - file:{} ({:?})\n",
                f.path.as_str(),
                f.access
            ));
        }
        for c in &step.expected_commands {
            s.push_str(&format!("     - run: {}\n", c.command));
        }
    }
    if !plan.requested_tools.is_empty() {
        s.push_str("Tools requested:\n");
        for t in &plan.requested_tools {
            s.push_str(&format!("  - {}\n", t.tool_name));
        }
    }
    if !plan.validations.is_empty() {
        s.push_str("Validation:\n");
        for v in &plan.validations {
            match v.command.as_deref() {
                Some(cmd) => s.push_str(&format!("  - {} ({})\n", cmd, v.description)),
                None => s.push_str(&format!("  - {}\n", v.description)),
            }
        }
    }
    if !plan.risks.is_empty() {
        s.push_str("Risks:\n");
        for r in &plan.risks {
            s.push_str(&format!("  - [{:?}] {}\n", r.level, r.description));
        }
    }
    if let Some(rb) = &plan.rollback_plan {
        s.push_str(&format!("Rollback: {}\n", rb));
    }
    if let Some(w) = &plan.unstructured_warning {
        s.push_str(&format!("WARNING: {}\n", w));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SessionId;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::new_minimal(PathBuf::from("."), SessionId::from("s".to_string()))
    }

    #[tokio::test]
    // SubmitPlan accepts a structured plan and stores it in the handle.
    async fn submit_plan_happy_path() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "fix tests");
        let handle = SubmitPlanHandle::new();
        let tool = SubmitPlanTool::new(base, handle.clone(), PlanningOptions::default());
        let r = tool
            .execute(
                json!({
                    "title": "fix the failing tests",
                    "summary": "make parser tests green",
                    "goal": "all tests pass",
                    "files": [{"path": "src/parser.rs", "access": "edit"}],
                    "steps": [{
                        "id": "1",
                        "title": "look at parser",
                        "kind": "inspect",
                        "expected_files": [{"path": "src/parser.rs", "access": "read"}],
                        "risk": "low"
                    }, {
                        "id": "2",
                        "title": "edit parser",
                        "kind": "edit",
                        "expected_files": [{"path": "src/parser.rs", "access": "edit"}],
                        "risk": "medium"
                    }],
                    "validations": [{"command": "cargo test", "description": "all green"}]
                }),
                ctx(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let snap = handle.snapshot().await.unwrap();
        assert_eq!(snap.plan.steps.len(), 2);
        assert_eq!(snap.plan.status, PlanStatus::PendingApproval);
    }

    #[tokio::test]
    // SubmitPlan rejects a command_regex containing an inline flag group
    // (`(?m)`, `(?s)`, etc.) at parse time. Defends against the
    // newline-injection bypass where `(?m)` re-interprets the auto-
    // anchored classifier wrapping as line anchors.
    async fn submit_plan_rejects_inline_regex_flags() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "x");
        let handle = SubmitPlanHandle::new();
        let tool = SubmitPlanTool::new(base, handle, PlanningOptions::default());
        let err = tool
            .execute(
                json!({
                    "title": "t",
                    "steps": [{
                        "id": "1", "title": "x", "kind": "execute",
                        "expected_commands": [{
                            "command": "cargo test",
                            "command_regex": "(?m)^cargo test$"
                        }]
                    }]
                }),
                ctx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(msg) => assert!(
                msg.contains("inline flag groups") || msg.contains("(?"),
                "expected flag-group rejection, got: {msg}"
            ),
            other => panic!("expected InvalidInput, got: {other:?}"),
        }
    }

    #[tokio::test]
    // SubmitPlan rejects a bare-wildcard `tool_name: "*"` in
    // `requested_tools`. A bare wildcard combined with the classifier's
    // Ambiguous fallthrough would let any tool slip past plan scope.
    async fn submit_plan_rejects_bare_wildcard_tool_name() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "x");
        let handle = SubmitPlanHandle::new();
        let tool = SubmitPlanTool::new(base, handle, PlanningOptions::default());
        let err = tool
            .execute(
                json!({
                    "title": "t",
                    "requested_tools": [{"tool_name": "*"}],
                    "steps": [{"id": "1", "title": "x", "kind": "execute"}]
                }),
                ctx(),
            )
            .await
            .unwrap_err();
        match err {
            ToolError::InvalidInput(msg) => assert!(
                msg.contains("wildcard"),
                "expected wildcard rejection, got: {msg}"
            ),
            other => panic!("expected InvalidInput, got: {other:?}"),
        }
    }

    #[tokio::test]
    // SubmitPlan accepts a prefix-glob `tool_name: "ok_prefix*"`. Plans
    // legitimately need to wildcard MCP tool families like
    // `mcp__github__*`, so the validation rejects only bare/leading
    // wildcards.
    async fn submit_plan_accepts_prefix_wildcard_tool_name() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "x");
        let handle = SubmitPlanHandle::new();
        let tool = SubmitPlanTool::new(base, handle.clone(), PlanningOptions::default());
        let r = tool
            .execute(
                json!({
                    "title": "t",
                    "requested_tools": [{"tool_name": "ok_prefix*"}],
                    "steps": [{"id": "1", "title": "x", "kind": "execute"}]
                }),
                ctx(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        let snap = handle.snapshot().await.unwrap();
        assert_eq!(snap.plan.requested_tools.len(), 1);
        assert_eq!(snap.plan.requested_tools[0].tool_name, "ok_prefix*");
    }

    #[tokio::test]
    // SubmitPlan rejects path traversal in expected files.
    async fn submit_plan_rejects_traversal() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "x");
        let handle = SubmitPlanHandle::new();
        let tool = SubmitPlanTool::new(base, handle, PlanningOptions::default());
        let err = tool
            .execute(
                json!({
                    "title": "t",
                    "steps": [{"id": "1", "title": "x", "kind": "edit",
                        "expected_files": [{"path": "../escape", "access": "edit"}]}]
                }),
                ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    // PlanUpdate records the step status update with timestamp.
    async fn plan_update_records() {
        let handle = PlanUpdateHandle::new();
        let tool = PlanUpdateTool::new(handle.clone());
        tool.execute(
            json!({"step_id": "1", "status": "in_progress", "message": "starting"}),
            ctx(),
        )
        .await
        .unwrap();
        let snap = handle.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].status, PlanStepStatus::InProgress);
    }

    #[test]
    // Textual fallback parses prose into a single-step pending plan.
    fn unstructured_fallback() {
        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "u");
        let r = parse_submit_plan_input("first read parser, then edit it", base);
        assert!(r.warning.is_some());
        assert_eq!(r.plan.steps.len(), 1);
        assert_eq!(r.plan.status, PlanStatus::PendingApproval);
        assert!(r.plan.unstructured_warning.is_some());
    }

    #[test]
    // Textual fallback summary truncation must not panic when the cap
    // falls on a multi-byte UTF-8 codepoint boundary (e.g. an emoji).
    // Pre-fix this panicked with "byte index 1024 is not a char boundary".
    fn unstructured_fallback_multibyte_boundary_no_panic() {
        // Build a string where a 4-byte emoji straddles the 1024-byte cap.
        // 1023 ASCII bytes + one 4-byte emoji ("🦀") + trailing text means
        // byte 1024 lands inside the emoji.
        let mut text = String::with_capacity(2048);
        for _ in 0..1023 {
            text.push('a');
        }
        text.push('🦀');
        text.push_str(" trailing tail content");

        let base = PlanDocument::new_draft(crate::plan::artifact::new_plan_id(), "", "u");
        let r = parse_submit_plan_input(&text, base);
        // Must produce a valid UTF-8 String ending in the ellipsis sentinel.
        assert!(r.plan.summary.ends_with('…'));
        // And the summary must be valid UTF-8 (implicit since it's a String,
        // but assert chars().count() works without panic on the result).
        let _ = r.plan.summary.chars().count();
    }
}
