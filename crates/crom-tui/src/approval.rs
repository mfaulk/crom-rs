//! Approval card formatting. Produces deterministic plain-text blocks for
//! every supported approval type (Edit/Write, Bash, MCP, Agent, Skill,
//! plugin-sourced) so the same logic powers the TUI modal and the headless
//! CLI prompt.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalKind {
    Edit,
    Write,
    Bash,
    Mcp,
    Agent,
    Skill,
    Other,
}

impl ApprovalKind {
    pub fn for_tool(name: &str) -> Self {
        match name {
            "Edit" => Self::Edit,
            "Write" => Self::Write,
            "Bash" => Self::Bash,
            "Agent" => Self::Agent,
            "Skill" => Self::Skill,
            n if n.starts_with("mcp__") => Self::Mcp,
            _ => Self::Other,
        }
    }
}

/// Context passed to [`format_approval_card`]. Keeping this struct stable
/// makes it easy to render the same card in multiple places.
#[derive(Debug, Clone)]
pub struct ApprovalContext<'a> {
    pub tool_name: &'a str,
    pub input: &'a Value,
    pub source: Option<&'a str>,
    pub risk: Option<&'a str>,
    pub checkpoint_id: Option<&'a str>,
    pub checkpoint_note: Option<&'a str>,
    pub diff_preview: Option<&'a str>,
}

/// Render an approval card as a list of lines. Critical info is kept
/// near the top so even truncated rendering keeps the user in the loop.
pub fn format_approval_card(ctx: &ApprovalContext<'_>) -> Vec<String> {
    let kind = ApprovalKind::for_tool(ctx.tool_name);
    let mut out = Vec::new();
    out.push(format!("Approval requested: {}", ctx.tool_name));
    if let Some(s) = ctx.source {
        out.push(format!("  source:     {}", s));
    }
    if let Some(r) = ctx.risk {
        out.push(format!("  risk:       {}", r));
    }
    if let Some(c) = ctx.checkpoint_id {
        out.push(format!("  checkpoint: {}", c));
    }
    if let Some(n) = ctx.checkpoint_note {
        out.push(format!("  note:       {}", n));
    }
    match kind {
        ApprovalKind::Edit | ApprovalKind::Write => {
            if let Some(p) = ctx.input.get("path").and_then(Value::as_str) {
                out.push(format!("  file:       {}", p));
            }
            if let Some(d) = ctx.diff_preview {
                out.push("  diff:".to_string());
                for line in d.lines().take(20) {
                    out.push(format!("    {}", line));
                }
                if d.lines().count() > 20 {
                    out.push("    …[truncated; press d for full diff]".to_string());
                }
            }
        }
        ApprovalKind::Bash => {
            if let Some(c) = ctx.input.get("command").and_then(Value::as_str) {
                out.push(format!("  command:    {}", c));
            }
            if let Some(c) = ctx.input.get("cwd").and_then(Value::as_str) {
                out.push(format!("  cwd:        {}", c));
            }
            if let Some(t) = ctx.input.get("timeout_seconds").and_then(Value::as_u64) {
                out.push(format!("  timeout:    {}s", t));
            }
            out.push("  note:       Bash file changes are not tracked by checkpoints".to_string());
        }
        ApprovalKind::Mcp => {
            if let Some(s) = ctx.tool_name.strip_prefix("mcp__") {
                if let Some((server, tool)) = s.split_once("__") {
                    out.push(format!("  server:     {}", server));
                    out.push(format!("  tool:       {}", tool));
                }
            }
            out.push("  note:       External effect; may not be reversible".to_string());
        }
        ApprovalKind::Agent => {
            if let Some(n) = ctx.input.get("agent").and_then(Value::as_str) {
                out.push(format!("  agent:      {}", n));
            }
            if let Some(t) = ctx.input.get("max_turns").and_then(Value::as_u64) {
                out.push(format!("  max_turns:  {}", t));
            }
        }
        ApprovalKind::Skill => {
            if let Some(n) = ctx.input.get("skill").and_then(Value::as_str) {
                out.push(format!("  skill:      {}", n));
            }
        }
        ApprovalKind::Other => {}
    }
    out.push(String::new());
    out.push(
        "  Approve? [y] once  [s] session  [a] project  [n] deny  [d] full diff  [r] revise  [?] help"
            .to_string(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    // Edit approval surfaces the target file path.
    fn edit_card_includes_file() {
        let input = json!({"path": "src/parser.rs"});
        let ctx = ApprovalContext {
            tool_name: "Edit",
            input: &input,
            source: None,
            risk: None,
            checkpoint_id: Some("ckpt_1"),
            checkpoint_note: None,
            diff_preview: Some("- old\n+ new"),
        };
        let lines = format_approval_card(&ctx);
        assert!(lines.iter().any(|l| l.contains("src/parser.rs")));
        assert!(lines.iter().any(|l| l.contains("ckpt_1")));
        assert!(lines.iter().any(|l| l.contains("- old")));
    }

    #[test]
    // Bash approval surfaces command/cwd/timeout and the checkpoint warning.
    fn bash_card_warns_on_checkpoint_coverage() {
        let input = json!({"command": "cargo test", "cwd": "/repo", "timeout_seconds": 30});
        let ctx = ApprovalContext {
            tool_name: "Bash",
            input: &input,
            source: None,
            risk: Some("low"),
            checkpoint_id: None,
            checkpoint_note: None,
            diff_preview: None,
        };
        let lines = format_approval_card(&ctx);
        assert!(lines.iter().any(|l| l.contains("cargo test")));
        assert!(lines.iter().any(|l| l.contains("Bash file changes")));
        assert!(lines.iter().any(|l| l.contains("low")));
    }

    #[test]
    // MCP approval shows the external-effect warning and parsed server/tool.
    fn mcp_card_shows_server_tool() {
        let input = json!({});
        let ctx = ApprovalContext {
            tool_name: "mcp__github__create_issue",
            input: &input,
            source: Some("plugin rust-quality@1"),
            risk: Some("mutating"),
            checkpoint_id: None,
            checkpoint_note: None,
            diff_preview: None,
        };
        let lines = format_approval_card(&ctx);
        assert!(lines.iter().any(|l| l.contains("github")));
        assert!(lines.iter().any(|l| l.contains("create_issue")));
        assert!(lines.iter().any(|l| l.contains("rust-quality")));
        assert!(lines.iter().any(|l| l.contains("External effect")));
    }

    #[test]
    // Plugin-sourced action carries source metadata into the card.
    fn other_tool_with_source_visible() {
        let input = json!({});
        let ctx = ApprovalContext {
            tool_name: "RustQualityCheck",
            input: &input,
            source: Some("plugin rust-quality@1.0.0"),
            risk: None,
            checkpoint_id: None,
            checkpoint_note: None,
            diff_preview: None,
        };
        let lines = format_approval_card(&ctx);
        assert!(lines.iter().any(|l| l.contains("rust-quality@1.0.0")));
    }

    #[test]
    // Approval card always contains the action menu.
    fn always_contains_menu() {
        let input = json!({});
        let ctx = ApprovalContext {
            tool_name: "Read",
            input: &input,
            source: None,
            risk: None,
            checkpoint_id: None,
            checkpoint_note: None,
            diff_preview: None,
        };
        let lines = format_approval_card(&ctx);
        assert!(lines.last().unwrap().contains("Approve?"));
        assert!(lines.iter().any(|l| l.contains("[r] revise")));
    }
}
