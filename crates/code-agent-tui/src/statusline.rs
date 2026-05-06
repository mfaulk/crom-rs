//! Status-line rendering. Thin wrapper over the core template engine that
//! also enforces a maximum width and degrades gracefully when variables
//! are missing.

use code_agent_core::{render_status_line, StatusLineContext};
use unicode_width::UnicodeWidthChar;

/// Render the status line, capping the result to `max_width` *display*
/// columns. Wide CJK and emoji characters count as 2 columns, so a
/// status line full of `你好世界` doesn't silently overflow into the next
/// row of the TUI.
pub fn format_status_line(template: &str, ctx: &StatusLineContext, max_width: usize) -> String {
    let raw = render_status_line(template, ctx);
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        // Not enough room for content + ellipsis. Honor the cap exactly.
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    let ellipsis_w = 1usize; // `…` is a single column
    let cap = max_width;
    for ch in raw.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + ellipsis_w > cap {
            // Reserving room for the ellipsis means we stop before
            // overflowing even when the next char is wide.
            // If we already saw at least one char, emit the ellipsis.
            // Otherwise we fall through and append nothing (handled below).
            out.push('…');
            return out;
        }
        out.push(ch);
        used += w;
        if used >= cap {
            return out;
        }
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

    #[test]
    // Zero-width cap returns an empty string instead of an ellipsis-only
    // overflow. Edge case flagged by external review.
    fn zero_width_returns_empty() {
        let ctx = StatusLineContext::default();
        let s = format_status_line("hi", &ctx, 0);
        assert_eq!(s, "");
    }

    #[test]
    // Wide CJK characters count as two display columns each, so the cap
    // applies to columns rather than `chars()`. Regression test for the
    // unicode-width review finding.
    fn wide_chars_use_display_width() {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "你好世界");
        // Each Han char = 2 cols. With max_width=5 we should fit two of
        // them plus an ellipsis.
        let s = format_status_line("{project}", &ctx, 5);
        // Two wide chars (4 cols) + `…` (1 col) = 5 cols total.
        assert_eq!(s, "你好…");
    }
}
