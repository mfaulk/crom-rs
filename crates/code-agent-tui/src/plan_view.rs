//! M11 TUI plan panel + approval/deviation modals.
//!
//! The renderers here are pure plain-text formatters returning a `Vec<String>`,
//! the same shape used by the rest of the TUI. The plain and fullscreen
//! renderers can show them as side panels, modals, or inline cards.

use code_agent_core::{PlanDeviation, PlanDocument, PlanStepStatus, PlanningOptions};

/// Render the plan panel: title, status, steps, validation. Compact enough
/// to fit alongside the transcript.
pub fn format_plan_panel(plan: &PlanDocument) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("Plan {}", plan.id));
    out.push(format!(
        "  version: {}   status: {}",
        plan.version,
        plan.status.label()
    ));
    out.push(format!("  title:   {}", plan.title));
    if !plan.goal.is_empty() {
        out.push(format!("  goal:    {}", plan.goal));
    }
    out.push(String::new());
    out.push("  Steps".into());
    for (i, step) in plan.steps.iter().enumerate() {
        let marker = step_marker(step.status);
        out.push(format!(
            "  {} {}. {} ({:?})",
            marker,
            i + 1,
            step.title,
            step.kind
        ));
        if !step.expected_files.is_empty() {
            let paths: Vec<String> = step
                .expected_files
                .iter()
                .map(|f| f.path.as_str().to_string())
                .collect();
            out.push(format!("       files: {}", paths.join(", ")));
        }
        if !step.expected_commands.is_empty() {
            let cmds: Vec<String> = step
                .expected_commands
                .iter()
                .map(|c| c.command.clone())
                .collect();
            out.push(format!("       run:   {}", cmds.join("; ")));
        }
    }
    if !plan.validations.is_empty() {
        out.push(String::new());
        out.push("  Validation".into());
        for v in &plan.validations {
            match &v.command {
                Some(cmd) => out.push(format!("    - {} ({})", cmd, v.description)),
                None => out.push(format!("    - {}", v.description)),
            }
        }
    }
    if !plan.risks.is_empty() {
        out.push(String::new());
        out.push("  Risks".into());
        for r in &plan.risks {
            out.push(format!("    - [{:?}] {}", r.level, r.description));
        }
    }
    if let Some(w) = &plan.unstructured_warning {
        out.push(String::new());
        out.push(format!("  ! {}", w));
    }
    out
}

fn step_marker(status: PlanStepStatus) -> &'static str {
    match status {
        PlanStepStatus::Pending => "[ ]",
        PlanStepStatus::InProgress => "[~]",
        PlanStepStatus::Completed => "[x]",
        PlanStepStatus::Failed => "[!]",
        PlanStepStatus::Skipped => "[-]",
        PlanStepStatus::Blocked => "[b]",
        PlanStepStatus::NeedsReplan => "[?]",
    }
}

/// Render the plan-approval modal: shows the headline summary, every
/// mutating tool the plan requests, validation commands, and the
/// keyboard action menu.
pub fn format_plan_approval_modal(plan: &PlanDocument) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!("Approve plan {} v{}?", plan.id, plan.version));
    out.push(format!("  title:    {}", plan.title));
    if !plan.summary.is_empty() {
        out.push(format!("  summary:  {}", plan.summary));
    }
    if !plan.goal.is_empty() {
        out.push(format!("  goal:     {}", plan.goal));
    }
    let mutating: Vec<&code_agent_core::PlanFileRef> = plan
        .files
        .iter()
        .chain(plan.steps.iter().flat_map(|s| s.expected_files.iter()))
        .filter(|f| f.access.is_mutating())
        .collect();
    if !mutating.is_empty() {
        out.push(String::new());
        out.push("  Mutating files".into());
        for f in mutating {
            out.push(format!("    - {} ({:?})", f.path.as_str(), f.access));
        }
    }
    let external: Vec<&code_agent_core::PlanToolUse> = plan
        .requested_tools
        .iter()
        .chain(plan.steps.iter().flat_map(|s| s.expected_tools.iter()))
        .filter(|t| t.tool_name.starts_with("mcp__"))
        .collect();
    if !external.is_empty() {
        out.push(String::new());
        out.push("  External (MCP) tools".into());
        for t in external {
            out.push(format!("    - {}", t.tool_name));
        }
    }
    // Surface every shell command the plan proposes to run. Without this,
    // an approver would see "Bash" in the requested tools list but have
    // no idea WHICH command(s) the model intends to execute, leaving an
    // arbitrary-shell-execution blind spot in the approval UI.
    let commands: Vec<&code_agent_core::PlanCommand> = plan
        .steps
        .iter()
        .flat_map(|s| s.expected_commands.iter())
        .collect();
    if !commands.is_empty() {
        out.push(String::new());
        out.push("  Shell commands".into());
        for c in commands {
            match &c.command_regex {
                Some(rx) => out.push(format!("    - {}  (regex: {})", c.command, rx)),
                None => out.push(format!("    - {}", c.command)),
            }
        }
    }
    let risky: Vec<&code_agent_core::PlanStep> = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(
                s.risk,
                code_agent_core::PlanRiskLevel::High | code_agent_core::PlanRiskLevel::External
            )
        })
        .collect();
    if !risky.is_empty() {
        out.push(String::new());
        out.push("  Risky steps".into());
        for s in risky {
            out.push(format!("    - [{:?}] {}", s.risk, s.title));
        }
    }
    if !plan.validations.is_empty() {
        out.push(String::new());
        out.push("  Validation".into());
        for v in &plan.validations {
            match &v.command {
                Some(cmd) => out.push(format!("    - {} ({})", cmd, v.description)),
                None => out.push(format!("    - {}", v.description)),
            }
        }
    }
    out.push(String::new());
    out.push("  [a] approve  [r] revise  [e] edit  [n] reject  [x] export  [?] help".into());
    out
}

/// Render the deviation modal when an act-mode tool call is out of scope.
pub fn format_deviation_modal(
    deviation: &PlanDeviation,
    plan: &PlanDocument,
    options: &PlanningOptions,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Plan deviation: {} requested {} (severity={:?})",
        plan.id, deviation.tool_name, deviation.severity
    ));
    out.push(format!("  reason: {}", deviation.reason));
    if let Some(step) = &deviation.step_id {
        out.push(format!("  step:   {}", step));
    }
    out.push(String::new());
    out.push("  Approved scope".into());
    let mut tools: Vec<String> = plan
        .requested_tools
        .iter()
        .map(|t| t.tool_name.clone())
        .collect();
    tools.dedup();
    if !tools.is_empty() {
        out.push(format!("    tools: {}", tools.join(", ")));
    }
    let files: Vec<String> = plan
        .files
        .iter()
        .map(|f| format!("{} ({:?})", f.path.as_str(), f.access))
        .collect();
    if !files.is_empty() {
        out.push(format!("    files: {}", files.join(", ")));
    }
    out.push(String::new());
    out.push(format!(
        "  Policy: {} (override with --strict-plan or settings)",
        match options.deviation_policy {
            code_agent_core::PlanDeviationPolicy::Deny => "deny",
            code_agent_core::PlanDeviationPolicy::Ask => "ask",
            code_agent_core::PlanDeviationPolicy::RequireReplan => "require-replan",
            code_agent_core::PlanDeviationPolicy::AllowWithWarning => "allow-with-warning",
        }
    ));
    out.push(String::new());
    out.push("  [a] approve once  [u] update plan  [r] re-plan  [n] deny".into());
    out
}

/// Render a textual diff between two plan revisions for the TUI.
pub fn format_plan_diff(from: &PlanDocument, to: &PlanDocument) -> Vec<String> {
    let mut out = Vec::new();
    out.push(format!(
        "Plan {}  v{}  →  v{}",
        to.id, from.version, to.version
    ));
    if from.title != to.title {
        out.push(format!("  title: {} → {}", from.title, to.title));
    }
    if from.goal != to.goal {
        out.push(format!("  goal:  {} → {}", from.goal, to.goal));
    }
    if from.steps.len() != to.steps.len() {
        out.push(format!(
            "  steps: {} → {}",
            from.steps.len(),
            to.steps.len()
        ));
    }
    // Per-step diff: walk in lock-step.
    let len = std::cmp::max(from.steps.len(), to.steps.len());
    for i in 0..len {
        match (from.steps.get(i), to.steps.get(i)) {
            (Some(a), Some(b)) if a.title != b.title || a.description != b.description => {
                out.push(format!("  [{}] {} → {}", i + 1, a.title, b.title));
            }
            (None, Some(b)) => out.push(format!("  + [{}] {}", i + 1, b.title)),
            (Some(a), None) => out.push(format!("  - [{}] {}", i + 1, a.title)),
            _ => {}
        }
    }
    if from.validations != to.validations {
        out.push("  validation differs".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use code_agent_core::{
        new_plan_id, PlanFileAccess, PlanFileRef, PlanRiskLevel, PlanStep, PlanStepKind,
        WorkspaceRelativePath,
    };

    fn small_plan() -> PlanDocument {
        let mut p = PlanDocument::new_draft(new_plan_id(), "T", "u");
        p.steps.push(PlanStep {
            id: "1".into(),
            title: "inspect".into(),
            description: String::new(),
            status: PlanStepStatus::Pending,
            kind: PlanStepKind::Inspect,
            expected_tools: Vec::new(),
            expected_files: vec![PlanFileRef {
                path: WorkspaceRelativePath::try_new("src/lib.rs").unwrap(),
                access: PlanFileAccess::Read,
                reason: None,
            }],
            expected_commands: Vec::new(),
            expected_outcome: None,
            risk: PlanRiskLevel::Low,
            requires_user_approval: false,
            validation: None,
        });
        p
    }

    #[test]
    // Plan panel includes plan id and step titles.
    fn panel_snapshot() {
        let p = small_plan();
        let lines = format_plan_panel(&p);
        assert!(lines.iter().any(|l| l.contains("Plan ")));
        assert!(lines.iter().any(|l| l.contains("inspect")));
    }

    #[test]
    // Approval modal renders the menu.
    fn approval_modal_has_menu() {
        let p = small_plan();
        let lines = format_plan_approval_modal(&p);
        assert!(lines.iter().any(|l| l.contains("[a] approve")));
    }

    #[test]
    // Deviation modal mentions the requested tool name.
    fn deviation_modal_mentions_tool() {
        let p = small_plan();
        let dev = PlanDeviation {
            plan_id: p.id.clone(),
            step_id: Some("1".into()),
            tool_name: "Edit".into(),
            requested_input: serde_json::json!({"file_path": "src/lexer.rs"}),
            reason: "not in plan".into(),
            severity: code_agent_core::PlanDeviationSeverity::High,
            occurred_at: time::OffsetDateTime::now_utc(),
        };
        let lines = format_deviation_modal(&dev, &p, &PlanningOptions::default());
        assert!(lines.iter().any(|l| l.contains("Edit")));
        assert!(lines.iter().any(|l| l.contains("[a] approve once")));
    }
}
