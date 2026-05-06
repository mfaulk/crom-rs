//! Renderer abstraction and capability detection.

use std::io::IsTerminal;

use code_agent_core::{ColorCapability, TerminalCapabilities, TerminalRenderer};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("terminal does not support fullscreen rendering")]
    Unsupported,
}

/// Detect the host terminal's capabilities. Pure function on environment +
/// stdout: never opens a real TTY in tests.
pub fn detect_capabilities_from_env() -> TerminalCapabilities {
    let stdout = std::io::stdout();
    let is_tty = stdout.is_terminal();
    if !is_tty {
        return TerminalCapabilities::non_interactive();
    }
    let (width, height) = match crossterm::terminal::size() {
        Ok((w, h)) => (w, h),
        Err(_) => (80, 24),
    };
    let no_color = std::env::var_os("NO_COLOR").is_some();
    let term = std::env::var("TERM").unwrap_or_default();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default();
    let color = if no_color {
        ColorCapability::None
    } else if colorterm == "truecolor" || colorterm == "24bit" {
        ColorCapability::TrueColor
    } else if term.contains("256") {
        ColorCapability::Ansi256
    } else if term.is_empty() || term == "dumb" {
        ColorCapability::None
    } else {
        ColorCapability::Ansi16
    };
    let alternate_screen = !term.is_empty() && term != "dumb";
    let bracketed_paste = alternate_screen;
    let keyboard_enhancement = alternate_screen;
    let shift_enter_detectable = false;
    TerminalCapabilities {
        is_tty,
        width,
        height,
        color,
        alternate_screen,
        bracketed_paste,
        keyboard_enhancement,
        shift_enter_detectable,
        unicode_width_safe: true,
    }
}

/// Choose the right renderer given user preference and detected capabilities.
/// Always falls back to plain when fullscreen would be unsafe (no TTY,
/// dumb term, tiny size).
pub fn fallback_renderer_for(
    requested: TerminalRenderer,
    fallback: TerminalRenderer,
    caps: &TerminalCapabilities,
) -> TerminalRenderer {
    match requested {
        TerminalRenderer::Plain => TerminalRenderer::Plain,
        TerminalRenderer::Fullscreen => {
            if caps.supports_fullscreen() {
                TerminalRenderer::Fullscreen
            } else {
                fallback
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Plain renderer is preserved no matter the capabilities.
    fn plain_passthrough() {
        let caps = TerminalCapabilities::non_interactive();
        let chosen =
            fallback_renderer_for(TerminalRenderer::Plain, TerminalRenderer::Fullscreen, &caps);
        assert_eq!(chosen, TerminalRenderer::Plain);
    }

    #[test]
    // Fullscreen falls back to plain when there's no TTY.
    fn fullscreen_falls_back_when_no_tty() {
        let caps = TerminalCapabilities::non_interactive();
        let chosen =
            fallback_renderer_for(TerminalRenderer::Fullscreen, TerminalRenderer::Plain, &caps);
        assert_eq!(chosen, TerminalRenderer::Plain);
    }

    #[test]
    // Tiny terminals fall back regardless of TTY status.
    fn fullscreen_falls_back_when_tiny() {
        let mut caps = TerminalCapabilities::non_interactive();
        caps.is_tty = true;
        caps.alternate_screen = true;
        caps.width = 20;
        caps.height = 4;
        let chosen =
            fallback_renderer_for(TerminalRenderer::Fullscreen, TerminalRenderer::Plain, &caps);
        assert_eq!(chosen, TerminalRenderer::Plain);
    }
}
