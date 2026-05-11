//! M9 terminal UI types: renderer selection, terminal capabilities, status
//! line and composer config, notification config, and recovery settings.
//!
//! Renderer selection and capability detection live here so that downstream
//! crates (CLI and the new `code-agent-tui` crate) can share the same model
//! without depending on each other. No I/O happens here — capability
//! detection is a pure function of the supplied environment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Choice of terminal renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalRenderer {
    /// Existing line-oriented output. Stable for CI, scripts, and dumb
    /// terminals.
    Plain,
    /// Interactive alternate-screen TUI built on `ratatui` + `crossterm`.
    Fullscreen,
}

impl TerminalRenderer {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "plain" | "line" | "stdout" => Some(Self::Plain),
            "fullscreen" | "tui" | "alt" | "alt-screen" => Some(Self::Fullscreen),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Fullscreen => "fullscreen",
        }
    }
}

/// Terminal color capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorCapability {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

/// Snapshot of terminal capabilities used by the renderer chooser, the doctor
/// command, and tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalCapabilities {
    pub is_tty: bool,
    pub width: u16,
    pub height: u16,
    pub color: ColorCapability,
    pub alternate_screen: bool,
    pub bracketed_paste: bool,
    pub keyboard_enhancement: bool,
    pub shift_enter_detectable: bool,
    pub unicode_width_safe: bool,
}

impl TerminalCapabilities {
    /// Returns true when the fullscreen renderer is safe to use.
    pub fn supports_fullscreen(&self) -> bool {
        self.is_tty && self.alternate_screen && self.width >= 40 && self.height >= 8
    }

    /// Conservative capabilities used in CI / tests.
    pub fn non_interactive() -> Self {
        Self {
            is_tty: false,
            width: 80,
            height: 24,
            color: ColorCapability::None,
            alternate_screen: false,
            bracketed_paste: false,
            keyboard_enhancement: false,
            shift_enter_detectable: false,
            unicode_width_safe: true,
        }
    }
}

/// Composer / input configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComposerConfig {
    pub multiline: bool,
    pub submit_key: SubmitKey,
    pub history_limit: usize,
    pub bracketed_paste: bool,
}

impl Default for ComposerConfig {
    fn default() -> Self {
        Self {
            multiline: true,
            submit_key: SubmitKey::Enter,
            history_limit: 200,
            bracketed_paste: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitKey {
    Enter,
    CtrlEnter,
}

/// Status line configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusLineConfig {
    pub enabled: bool,
    pub template: String,
    pub refresh_interval_ms: u64,
}

impl Default for StatusLineConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            template: DEFAULT_STATUSLINE_TEMPLATE.to_string(),
            refresh_interval_ms: 1000,
        }
    }
}

/// Default status line. Variables are substituted by [`render_status_line`].
pub const DEFAULT_STATUSLINE_TEMPLATE: &str =
    "{project} · {git_branch}{git_dirty} · {model} · ctx {context_percent}% · {estimated_cost}";

/// Notification configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NotificationConfig {
    pub enabled: bool,
    pub on_approval_required: bool,
    pub on_run_complete: bool,
    pub on_run_failed: bool,
    pub on_validation_failed: bool,
    pub on_long_command_complete: bool,
    pub on_subagent_complete: bool,
}

impl NotificationConfig {
    /// Defaults disabled, but with a sensible "if you turn me on, this is
    /// what fires" set already chosen so the user does not have to flip
    /// every leaf.
    pub fn defaults_when_enabled() -> Self {
        Self {
            enabled: true,
            on_approval_required: true,
            on_run_complete: true,
            on_run_failed: true,
            on_validation_failed: true,
            on_long_command_complete: true,
            on_subagent_complete: false,
        }
    }
}

/// Aggregated UI settings — the on-disk shape AND the in-memory effective
/// config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiConfig {
    pub renderer: TerminalRenderer,
    pub fallback_renderer: TerminalRenderer,
    pub confirm_exit_while_running: bool,
    pub show_subagent_events: bool,
    pub show_tool_json: bool,
    pub status_line: StatusLineConfig,
    pub composer: ComposerConfig,
    pub notifications: NotificationConfig,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            renderer: TerminalRenderer::Fullscreen,
            fallback_renderer: TerminalRenderer::Plain,
            confirm_exit_while_running: true,
            show_subagent_events: false,
            show_tool_json: false,
            status_line: StatusLineConfig::default(),
            composer: ComposerConfig::default(),
            notifications: NotificationConfig::default(),
        }
    }
}

/// On-disk schema for `settings.ui`. Every field optional so JSON deltas
/// merge cleanly. See [`merge_ui_into`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSettingsConfig {
    #[serde(default)]
    pub renderer: Option<String>,
    #[serde(default)]
    pub fallback_renderer: Option<String>,
    #[serde(default)]
    pub confirm_exit_while_running: Option<bool>,
    #[serde(default)]
    pub show_subagent_events: Option<bool>,
    #[serde(default)]
    pub show_tool_json: Option<bool>,
    #[serde(default)]
    pub status_line: Option<StatusLineSettingsConfig>,
    #[serde(default)]
    pub composer: Option<ComposerSettingsConfig>,
    #[serde(default)]
    pub notifications: Option<NotificationSettingsConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusLineSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub refresh_interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerSettingsConfig {
    #[serde(default)]
    pub multiline: Option<bool>,
    #[serde(default)]
    pub submit_key: Option<String>,
    #[serde(default)]
    pub history_limit: Option<usize>,
    #[serde(default)]
    pub bracketed_paste: Option<bool>,
}

#[derive(Debug, Clone, Default, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub on_approval_required: Option<bool>,
    #[serde(default)]
    pub on_run_complete: Option<bool>,
    #[serde(default)]
    pub on_run_failed: Option<bool>,
    #[serde(default)]
    pub on_validation_failed: Option<bool>,
    #[serde(default)]
    pub on_long_command_complete: Option<bool>,
    #[serde(default)]
    pub on_subagent_complete: Option<bool>,
}

/// Errors that can happen while validating the UI settings block.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UiSettingsError {
    #[error("invalid ui.renderer: {0}")]
    BadRenderer(String),
    #[error("invalid ui.fallback_renderer: {0}")]
    BadFallbackRenderer(String),
    #[error("invalid ui.composer.submit_key: {0}")]
    BadSubmitKey(String),
    #[error("ui.status_line.template references unknown variables: {0:?}")]
    UnknownStatusLineVariables(Vec<String>),
}

/// Recovery / lock settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryConfig {
    pub enabled: bool,
    pub stale_lock_after_seconds: u64,
    pub auto_mark_interrupted: bool,
}

impl Default for RecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stale_lock_after_seconds: 3600,
            auto_mark_interrupted: true,
        }
    }
}

#[derive(Debug, Clone, Default, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoverySettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub stale_lock_after_seconds: Option<u64>,
    #[serde(default)]
    pub auto_mark_interrupted: Option<bool>,
}

/// Apply a [`UiSettingsConfig`] over `dst`.
pub fn merge_ui_into(dst: &mut UiConfig, src: UiSettingsConfig) -> Result<(), UiSettingsError> {
    if let Some(r) = src.renderer {
        dst.renderer = TerminalRenderer::from_str_lenient(&r)
            .ok_or_else(|| UiSettingsError::BadRenderer(r.clone()))?;
    }
    if let Some(r) = src.fallback_renderer {
        dst.fallback_renderer = TerminalRenderer::from_str_lenient(&r)
            .ok_or_else(|| UiSettingsError::BadFallbackRenderer(r.clone()))?;
    }
    if let Some(b) = src.confirm_exit_while_running {
        dst.confirm_exit_while_running = b;
    }
    if let Some(b) = src.show_subagent_events {
        dst.show_subagent_events = b;
    }
    if let Some(b) = src.show_tool_json {
        dst.show_tool_json = b;
    }
    if let Some(sl) = src.status_line {
        if let Some(b) = sl.enabled {
            dst.status_line.enabled = b;
        }
        if let Some(t) = sl.template {
            // Validate the template eagerly so an unknown variable surfaces
            // as a settings error, not a silent rendering failure.
            let unknown = unknown_status_line_variables(&t);
            if !unknown.is_empty() {
                return Err(UiSettingsError::UnknownStatusLineVariables(unknown));
            }
            dst.status_line.template = t;
        }
        if let Some(ms) = sl.refresh_interval_ms {
            dst.status_line.refresh_interval_ms = ms;
        }
    }
    if let Some(c) = src.composer {
        if let Some(b) = c.multiline {
            dst.composer.multiline = b;
        }
        if let Some(sk) = c.submit_key {
            dst.composer.submit_key = match sk.to_ascii_lowercase().as_str() {
                "enter" => SubmitKey::Enter,
                "ctrl_enter" | "ctrl-enter" | "ctrlenter" => SubmitKey::CtrlEnter,
                other => return Err(UiSettingsError::BadSubmitKey(other.to_string())),
            };
        }
        if let Some(n) = c.history_limit {
            dst.composer.history_limit = n;
        }
        if let Some(b) = c.bracketed_paste {
            dst.composer.bracketed_paste = b;
        }
    }
    if let Some(n) = src.notifications {
        if let Some(b) = n.enabled {
            dst.notifications.enabled = b;
        }
        if let Some(b) = n.on_approval_required {
            dst.notifications.on_approval_required = b;
        }
        if let Some(b) = n.on_run_complete {
            dst.notifications.on_run_complete = b;
        }
        if let Some(b) = n.on_run_failed {
            dst.notifications.on_run_failed = b;
        }
        if let Some(b) = n.on_validation_failed {
            dst.notifications.on_validation_failed = b;
        }
        if let Some(b) = n.on_long_command_complete {
            dst.notifications.on_long_command_complete = b;
        }
        if let Some(b) = n.on_subagent_complete {
            dst.notifications.on_subagent_complete = b;
        }
    }
    Ok(())
}

pub fn merge_recovery_into(dst: &mut RecoveryConfig, src: RecoverySettingsConfig) {
    if let Some(b) = src.enabled {
        dst.enabled = b;
    }
    if let Some(n) = src.stale_lock_after_seconds {
        dst.stale_lock_after_seconds = n;
    }
    if let Some(b) = src.auto_mark_interrupted {
        dst.auto_mark_interrupted = b;
    }
}

/// Set of supported status line variables. Used for validation and rendering.
pub fn supported_status_line_variables() -> &'static [&'static str] {
    &[
        "project",
        "cwd",
        "git_branch",
        "git_dirty",
        "model",
        "permission_mode",
        "session_id",
        "run_id",
        "context_percent",
        "input_tokens",
        "output_tokens",
        "estimated_cost",
        "checkpoint_id",
        "mcp_servers",
        "plugins",
        "loaded_tools",
        "deferred_tools",
        // M10 auth/provider variables.
        "provider",
        "account",
        "auth_method",
        "plan",
        "quota",
    ]
}

/// Return any `{var}` placeholders in `template` that are not in
/// [`supported_status_line_variables`].
pub fn unknown_status_line_variables(template: &str) -> Vec<String> {
    let supported = supported_status_line_variables();
    let mut bad = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '}' {
                closed = true;
                break;
            }
            name.push(c);
        }
        if closed && !name.is_empty() && !supported.contains(&name.as_str()) && !bad.contains(&name)
        {
            bad.push(name);
        }
    }
    bad
}

/// Variable bag used by [`render_status_line`].
#[derive(Debug, Clone, Default)]
pub struct StatusLineContext {
    pub vars: BTreeMap<String, String>,
}

impl StatusLineContext {
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.vars.insert(key.to_string(), value.into());
    }
}

/// Render a status line. Unknown variables are reported (expected to have
/// been caught at settings load); supported variables fall back to an empty
/// string when not provided.
pub fn render_status_line(template: &str, ctx: &StatusLineContext) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for c in chars.by_ref() {
            if c == '}' {
                closed = true;
                break;
            }
            name.push(c);
        }
        if !closed {
            out.push('{');
            out.push_str(&name);
            continue;
        }
        out.push_str(ctx.vars.get(&name).map(|s| s.as_str()).unwrap_or(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Default fallback renderer is Plain regardless of primary choice.
    fn default_ui_falls_back_to_plain() {
        let cfg = UiConfig::default();
        assert_eq!(cfg.fallback_renderer, TerminalRenderer::Plain);
    }

    #[test]
    // Unknown renderer values produce a clear settings error.
    fn invalid_renderer_errors() {
        let mut cfg = UiConfig::default();
        let res = merge_ui_into(
            &mut cfg,
            UiSettingsConfig {
                renderer: Some("ascii-art".into()),
                ..Default::default()
            },
        );
        assert_eq!(res, Err(UiSettingsError::BadRenderer("ascii-art".into())));
    }

    #[test]
    // Status line template validation rejects unknown variables.
    fn unknown_status_variables_rejected() {
        let mut cfg = UiConfig::default();
        let res = merge_ui_into(
            &mut cfg,
            UiSettingsConfig {
                status_line: Some(StatusLineSettingsConfig {
                    template: Some("{project} {bogus}".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert!(matches!(
            res,
            Err(UiSettingsError::UnknownStatusLineVariables(ref v)) if v == &vec!["bogus".to_string()]
        ));
    }

    #[test]
    // Default template renders deterministically with provided variables.
    fn default_template_renders() {
        let mut ctx = StatusLineContext::default();
        ctx.set("project", "demo");
        ctx.set("git_branch", "main");
        ctx.set("git_dirty", "*");
        ctx.set("model", "Sonnet");
        ctx.set("context_percent", "42");
        ctx.set("estimated_cost", "$0.37");
        let s = render_status_line(DEFAULT_STATUSLINE_TEMPLATE, &ctx);
        assert_eq!(s, "demo · main* · Sonnet · ctx 42% · $0.37");
    }

    #[test]
    // Missing variables degrade to empty strings without panicking.
    fn missing_variable_renders_empty() {
        let ctx = StatusLineContext::default();
        let s = render_status_line("{project}-{git_branch}", &ctx);
        assert_eq!(s, "-");
    }

    #[test]
    // Recovery settings merge respects each leaf.
    fn recovery_merge_overrides() {
        let mut cfg = RecoveryConfig::default();
        merge_recovery_into(
            &mut cfg,
            RecoverySettingsConfig {
                enabled: Some(false),
                stale_lock_after_seconds: Some(60),
                auto_mark_interrupted: None,
            },
        );
        assert!(!cfg.enabled);
        assert_eq!(cfg.stale_lock_after_seconds, 60);
        assert!(cfg.auto_mark_interrupted);
    }

    #[test]
    // Submit key parsing recognizes both Enter and Ctrl+Enter.
    fn submit_key_parses() {
        let mut cfg = UiConfig::default();
        merge_ui_into(
            &mut cfg,
            UiSettingsConfig {
                composer: Some(ComposerSettingsConfig {
                    submit_key: Some("ctrl_enter".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(cfg.composer.submit_key, SubmitKey::CtrlEnter);
    }

    #[test]
    // Non-interactive caps reflect a TTY-less environment with sensible
    // defaults.
    fn non_interactive_caps_disable_fullscreen() {
        let caps = TerminalCapabilities::non_interactive();
        assert!(!caps.supports_fullscreen());
    }

    #[test]
    // Notifications default to disabled.
    fn notifications_disabled_by_default() {
        let cfg = NotificationConfig::default();
        assert!(!cfg.enabled);
    }
}
