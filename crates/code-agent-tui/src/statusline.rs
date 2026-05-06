//! Status-line rendering. Thin wrapper over the core template engine that
//! also enforces a maximum width and degrades gracefully when variables
//! are missing.

use code_agent_core::{render_status_line, StatusLineContext};

/// Render the status line, capping the result to `max_width` characters
/// (counted by Unicode chars, which is sufficient for ASCII-heavy templates).
pub fn format_status_line(template: &str, ctx: &StatusLineContext, max_width: usize) -> String {
    let raw = render_status_line(template, ctx);
    let mut out = String::new();
    for (count, ch) in raw.chars().enumerate() {
        if count >= max_width.saturating_sub(1) {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Default template renders deterministically.
    fn renders_default_template() {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "demo");
        ctx.set("git_branch", "main");
        ctx.set("git_dirty", "");
        ctx.set("model", "Sonnet");
        ctx.set("context_percent", "10");
        ctx.set("estimated_cost", "$0.10");
        let s = format_status_line(code_agent_core::DEFAULT_STATUSLINE_TEMPLATE, &ctx, 80);
        assert!(s.contains("demo"));
        assert!(s.contains("Sonnet"));
    }

    #[test]
    // Empty cost variable degrades gracefully.
    fn degrades_when_cost_missing() {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "demo");
        let s = format_status_line("{project} - {estimated_cost}", &ctx, 40);
        assert_eq!(s, "demo - ");
    }

    #[test]
    // Width cap truncates with ellipsis.
    fn caps_to_width() {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "verylong-project-name");
        let s = format_status_line("{project}", &ctx, 6);
        assert_eq!(s, "veryl…");
    }
}
