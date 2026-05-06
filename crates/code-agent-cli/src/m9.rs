//! M9 CLI surfaces: doctor, init, paths, completions, status, config,
//! sessions browser/recover, statusline, and changes views. Each helper
//! is a pure function (or returns a `DoctorReport`/etc) so it can be
//! exercised by tests without a real terminal.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use code_agent_core::{
    DoctorSeverity, DoctorStatus, RecoveryConfig, SettingSource, Settings, StatusLineContext,
    TerminalCapabilities, TerminalRenderer, UiConfig,
};
use code_agent_session::{detect_state, FsSessionStore, SessionRecoveryInfo, SessionStore};
use serde::{Deserialize, Serialize};

/// One row in the doctor report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: DoctorStatus,
    pub severity: DoctorSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

/// Aggregate doctor output. `text` is the human-readable form; `checks` is
/// the structured form serialized by `--json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorReport {
    pub checks: Vec<DoctorCheck>,
    #[serde(skip)]
    pub text: Vec<String>,
}

impl DoctorReport {
    pub fn worst_severity(&self) -> DoctorSeverity {
        let mut worst = DoctorSeverity::Info;
        for c in &self.checks {
            worst = match (worst, c.severity) {
                (DoctorSeverity::Error, _) | (_, DoctorSeverity::Error) => DoctorSeverity::Error,
                (DoctorSeverity::Warning, _) | (_, DoctorSeverity::Warning) => {
                    DoctorSeverity::Warning
                }
                _ => DoctorSeverity::Info,
            };
        }
        worst
    }

    pub fn has_failures(&self) -> bool {
        self.checks.iter().any(|c| {
            matches!(c.status, DoctorStatus::Fail) && matches!(c.severity, DoctorSeverity::Error)
        })
    }
}

/// Run the doctor checks against a workspace + settings snapshot. Stays
/// fully synchronous and offline so it is safe to call from tests, CI,
/// and `code-agent doctor --json`.
pub fn run_doctor(cwd: &Path, settings: &Settings, redact_secrets: bool) -> DoctorReport {
    let mut checks = Vec::new();
    let mut text = Vec::new();

    text.push("code-agent doctor".to_string());
    text.push(String::new());

    text.push("Provider".to_string());
    let api_present = std::env::var_os("ANTHROPIC_API_KEY").is_some();
    if api_present {
        let masked = if redact_secrets {
            "****".to_string()
        } else {
            "set".to_string()
        };
        checks.push(DoctorCheck {
            id: "provider.api_key".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: format!("ANTHROPIC_API_KEY {}", masked),
            remediation: None,
        });
        text.push(format!("  ✓ ANTHROPIC_API_KEY {}", masked));
    } else {
        checks.push(DoctorCheck {
            id: "provider.api_key".into(),
            status: DoctorStatus::Fail,
            severity: DoctorSeverity::Error,
            message: "ANTHROPIC_API_KEY not set".into(),
            remediation: Some("export ANTHROPIC_API_KEY=...".into()),
        });
        text.push("  ✗ ANTHROPIC_API_KEY not set".into());
        text.push("    → export ANTHROPIC_API_KEY=...".into());
    }

    text.push(String::new());
    text.push("Workspace".to_string());

    let agent_dir = cwd.join(".agent");
    let writable = is_writable(&agent_dir);
    if writable {
        checks.push(DoctorCheck {
            id: "workspace.agent_dir".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: ".agent writable".into(),
            remediation: None,
        });
        text.push("  ✓ .agent writable".into());
    } else {
        checks.push(DoctorCheck {
            id: "workspace.agent_dir".into(),
            status: DoctorStatus::Warn,
            severity: DoctorSeverity::Warning,
            message: ".agent not writable (yet)".into(),
            remediation: Some("run `code-agent init`".into()),
        });
        text.push("  ! .agent not writable yet".into());
    }

    let sessions_dir = cwd.join(".agent").join("sessions");
    if is_writable(&sessions_dir) || sessions_dir.parent().map(is_writable).unwrap_or(false) {
        checks.push(DoctorCheck {
            id: "workspace.sessions".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: "session store writable".into(),
            remediation: None,
        });
        text.push("  ✓ session store writable".into());
    } else {
        checks.push(DoctorCheck {
            id: "workspace.sessions".into(),
            status: DoctorStatus::Fail,
            severity: DoctorSeverity::Error,
            message: "session store not writable".into(),
            remediation: Some(format!("ensure {} is writable", sessions_dir.display())),
        });
        text.push("  ✗ session store not writable".into());
    }

    let checkpoint_dir = cwd.join(".agent").join("checkpoints");
    if is_writable(&checkpoint_dir) || checkpoint_dir.parent().map(is_writable).unwrap_or(false) {
        checks.push(DoctorCheck {
            id: "workspace.checkpoints".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: "checkpoint store writable".into(),
            remediation: None,
        });
        text.push("  ✓ checkpoint store writable".into());
    } else {
        checks.push(DoctorCheck {
            id: "workspace.checkpoints".into(),
            status: DoctorStatus::Fail,
            severity: DoctorSeverity::Error,
            message: "checkpoint store not writable".into(),
            remediation: Some(format!("ensure {} is writable", checkpoint_dir.display())),
        });
        text.push("  ✗ checkpoint store not writable".into());
    }

    text.push(String::new());
    text.push("Configuration".to_string());
    if matches!(settings.source, SettingSource::None) {
        checks.push(DoctorCheck {
            id: "config.loaded".into(),
            status: DoctorStatus::Warn,
            severity: DoctorSeverity::Warning,
            message: "settings disabled (--settings none)".into(),
            remediation: None,
        });
        text.push("  ! settings disabled (--settings none)".into());
    } else {
        checks.push(DoctorCheck {
            id: "config.loaded".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: format!("settings loaded ({:?})", settings.source),
            remediation: None,
        });
        text.push(format!("  ✓ settings loaded ({:?})", settings.source));
    }

    text.push(String::new());
    text.push("Terminal".to_string());
    let caps = code_agent_tui::detect_capabilities_from_env();
    if caps.supports_fullscreen() {
        checks.push(DoctorCheck {
            id: "terminal.fullscreen".into(),
            status: DoctorStatus::Ok,
            severity: DoctorSeverity::Info,
            message: format!("fullscreen TUI supported ({}x{})", caps.width, caps.height),
            remediation: None,
        });
        text.push(format!(
            "  ✓ fullscreen TUI supported ({}x{})",
            caps.width, caps.height
        ));
    } else {
        checks.push(DoctorCheck {
            id: "terminal.fullscreen".into(),
            status: DoctorStatus::Warn,
            severity: DoctorSeverity::Warning,
            message: "fullscreen TUI unavailable; plain renderer will be used".into(),
            remediation: Some("run from an interactive terminal (TTY)".into()),
        });
        text.push("  ! fullscreen TUI unavailable; plain renderer will be used".into());
    }
    if !caps.shift_enter_detectable {
        checks.push(DoctorCheck {
            id: "terminal.shift_enter".into(),
            status: DoctorStatus::Warn,
            severity: DoctorSeverity::Info,
            message: "Shift+Enter not distinguishable; use Esc+Enter for newline".into(),
            remediation: None,
        });
        text.push("  ! Shift+Enter not distinguishable; use Esc+Enter for newline".into());
    }

    text.push(String::new());
    text.push("Recovery".to_string());
    if std::path::Path::new(&sessions_dir).exists() {
        match futures::executor::block_on(detect_state(
            cwd,
            settings.recovery.stale_lock_after_seconds,
        )) {
            Ok(infos) => {
                let stale: Vec<&SessionRecoveryInfo> = infos
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.state,
                            code_agent_core::SessionRuntimeState::NeedsRecovery
                                | code_agent_core::SessionRuntimeState::Crashed
                        )
                    })
                    .collect();
                if stale.is_empty() {
                    checks.push(DoctorCheck {
                        id: "recovery.stale_locks".into(),
                        status: DoctorStatus::Ok,
                        severity: DoctorSeverity::Info,
                        message: "no stale session locks".into(),
                        remediation: None,
                    });
                    text.push("  ✓ no stale session locks".into());
                } else {
                    checks.push(DoctorCheck {
                        id: "recovery.stale_locks".into(),
                        status: DoctorStatus::Warn,
                        severity: DoctorSeverity::Warning,
                        message: format!("{} session(s) need recovery", stale.len()),
                        remediation: Some(format!(
                            "run `code-agent sessions recover --cwd {}`",
                            cwd.display()
                        )),
                    });
                    text.push(format!("  ! {} session(s) need recovery", stale.len()));
                }
            }
            Err(e) => {
                checks.push(DoctorCheck {
                    id: "recovery.stale_locks".into(),
                    status: DoctorStatus::Skipped,
                    severity: DoctorSeverity::Info,
                    message: format!("could not inspect sessions: {}", e),
                    remediation: None,
                });
            }
        }
    }

    DoctorReport { checks, text }
}

fn is_writable(path: &Path) -> bool {
    if !path.exists() {
        // If parent exists and is writable, treat as writable.
        if let Some(parent) = path.parent() {
            return parent.exists() && writable_metadata(parent);
        }
        return false;
    }
    writable_metadata(path)
}

fn writable_metadata(path: &Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => !m.permissions().readonly(),
        Err(_) => false,
    }
}

/// Where everything lives. Used by `code-agent paths` and the doctor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedPaths {
    pub project_settings: Option<PathBuf>,
    pub user_settings: Option<PathBuf>,
    pub project_local_settings: Option<PathBuf>,
    pub agent_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub checkpoints_dir: PathBuf,
    pub plugin_cache_dir: PathBuf,
    pub plugin_data_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub cache_dir: PathBuf,
}

pub fn resolve_paths(cwd: &Path) -> ResolvedPaths {
    let agent_dir = cwd.join(".agent");
    ResolvedPaths {
        project_settings: Some(agent_dir.join("settings.json")),
        user_settings: code_agent_core::settings::user_settings_path(),
        project_local_settings: Some(agent_dir.join("settings.local.json")),
        agent_dir: agent_dir.clone(),
        sessions_dir: agent_dir.join("sessions"),
        checkpoints_dir: agent_dir.join("checkpoints"),
        plugin_cache_dir: agent_dir.join("plugins").join("cache"),
        plugin_data_dir: agent_dir.join("plugins").join("data"),
        logs_dir: agent_dir.join("logs"),
        cache_dir: agent_dir.join("cache"),
    }
}

/// Result of `code-agent init`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOutcome {
    pub created_agent_dir: bool,
    pub created_settings: bool,
    pub created_local_settings_template: bool,
    pub created_agents_md: bool,
    pub api_key_present: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitMode {
    Minimal,
    Interactive,
    Ci,
}

impl InitMode {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "minimal" => Some(Self::Minimal),
            "interactive" => Some(Self::Interactive),
            "ci" => Some(Self::Ci),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Interactive => "interactive",
            Self::Ci => "ci",
        }
    }
}

/// Run `code-agent init`. Pure stdlib I/O so it can be unit-tested with
/// a TempDir. Never overwrites existing files unless `force` is set.
pub fn run_init(cwd: &Path, mode: InitMode, force: bool) -> Result<InitOutcome> {
    let agent_dir = cwd.join(".agent");
    let mut outcome = InitOutcome {
        created_agent_dir: false,
        created_settings: false,
        created_local_settings_template: false,
        created_agents_md: false,
        api_key_present: std::env::var_os("ANTHROPIC_API_KEY").is_some(),
    };
    if !agent_dir.exists() {
        std::fs::create_dir_all(&agent_dir)?;
        std::fs::create_dir_all(agent_dir.join("sessions"))?;
        std::fs::create_dir_all(agent_dir.join("checkpoints"))?;
        std::fs::create_dir_all(agent_dir.join("logs"))?;
        outcome.created_agent_dir = true;
    }

    let settings_path = agent_dir.join("settings.json");
    if !settings_path.exists() || force {
        std::fs::write(&settings_path, default_settings_json(mode))?;
        outcome.created_settings = true;
    }

    let local_settings_path = agent_dir.join("settings.local.json");
    if !local_settings_path.exists() && matches!(mode, InitMode::Interactive) {
        std::fs::write(
            &local_settings_path,
            "{\n  \"_comment\": \"Local-only overrides; ignored by git.\"\n}\n",
        )?;
        outcome.created_local_settings_template = true;
    }

    let agents_md = cwd.join("AGENTS.md");
    if !agents_md.exists() && matches!(mode, InitMode::Minimal | InitMode::Interactive) {
        std::fs::write(&agents_md, default_agents_md())?;
        outcome.created_agents_md = true;
    }

    Ok(outcome)
}

fn default_settings_json(mode: InitMode) -> String {
    let renderer = match mode {
        InitMode::Ci => "plain",
        _ => "fullscreen",
    };
    format!(
        "{{\n  \"permission_mode\": \"default\",\n  \"checkpointing\": {{\n    \"enabled\": true\n  }},\n  \"ui\": {{\n    \"renderer\": \"{renderer}\",\n    \"fallback_renderer\": \"plain\",\n    \"status_line\": {{\n      \"enabled\": true,\n      \"template\": \"{{project}} · {{git_branch}}{{git_dirty}} · {{model}} · ctx {{context_percent}}% · {{estimated_cost}}\"\n    }}\n  }},\n  \"recovery\": {{\n    \"enabled\": true,\n    \"stale_lock_after_seconds\": 3600\n  }}\n}}\n"
    )
}

fn default_agents_md() -> &'static str {
    "# AGENTS.md\n\nProject instructions for the coding agent.\n\n## Workflow\n- Run tests with `cargo test --workspace` before committing\n- Use Conventional Commits\n- Keep edits minimal and focused\n\n## Areas to be careful with\n- (add notes here as you discover sensitive paths)\n"
}

/// Stable lower-case label for a permission mode. Reused by both the
/// `code-agent config` summary and the status-line context so the same
/// mode renders identically everywhere.
pub fn permission_mode_label(mode: Option<code_agent_core::PermissionMode>) -> String {
    use code_agent_core::PermissionMode as P;
    match mode {
        None | Some(P::Default) => "default".to_string(),
        Some(P::AcceptEdits) => "accept_edits".to_string(),
        Some(P::Plan) => "plan".to_string(),
        Some(P::DontAsk) => "dont_ask".to_string(),
    }
}

/// Render a friendly summary of the effective configuration (for `/config`
/// and `code-agent config`). Secrets are not present in `Settings` itself
/// — provider keys live in env vars — so no redaction step is needed here.
pub fn format_effective_config(settings: &Settings) -> Vec<String> {
    let mut out = Vec::new();
    out.push("Effective configuration".to_string());
    out.push(format!("  source:          {:?}", settings.source));
    if let Some(p) = &settings.project_path {
        out.push(format!("  project file:    {}", p.display()));
    }
    if let Some(p) = &settings.user_path {
        out.push(format!("  user file:       {}", p.display()));
    }
    out.push(format!(
        "  model:           {}",
        settings.model.as_deref().unwrap_or("(default)")
    ));
    out.push(format!(
        "  permission_mode: {}",
        permission_mode_label(settings.permission_mode)
    ));
    out.push(format!(
        "  checkpointing:   {}",
        settings
            .checkpointing
            .enabled
            .map(|b| if b { "on" } else { "off" })
            .unwrap_or("default")
    ));
    out.push("  ui:".to_string());
    out.push(format!(
        "    renderer:        {}",
        settings.ui.renderer.as_str()
    ));
    out.push(format!(
        "    fallback:        {}",
        settings.ui.fallback_renderer.as_str()
    ));
    out.push(format!(
        "    status_line:     {}",
        if settings.ui.status_line.enabled {
            settings.ui.status_line.template.as_str()
        } else {
            "(disabled)"
        }
    ));
    out.push(format!(
        "    notifications:   {}",
        if settings.ui.notifications.enabled {
            "on"
        } else {
            "off"
        }
    ));
    out.push("  recovery:".to_string());
    out.push(format!(
        "    stale_after:     {}s",
        settings.recovery.stale_lock_after_seconds
    ));
    out
}

/// Format the runtime status line for `/status` and `code-agent status`.
pub fn format_session_status(
    cwd: &Path,
    settings: &Settings,
    session_id: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::new();
    out.push("Session status".to_string());
    out.push(format!("  cwd:           {}", cwd.display()));
    out.push(format!(
        "  model:         {}",
        settings.model.as_deref().unwrap_or("(default)")
    ));
    if let Some(s) = session_id {
        out.push(format!("  session:       {}", s));
    } else {
        out.push("  session:       (none)".to_string());
    }
    let store = FsSessionStore::new(cwd.to_path_buf());
    let listing = futures::executor::block_on(store.list(cwd)).unwrap_or_default();
    out.push(format!("  sessions on disk: {}", listing.len()));
    out
}

/// Build the default status-line context for a CLI session. Variables
/// missing here render as empty strings, which is what we want — partial
/// info is better than fabricated info.
pub fn build_default_status_ctx(
    cwd: &Path,
    settings: &Settings,
    session_id: Option<&str>,
    git_branch: Option<&str>,
    git_dirty: bool,
) -> StatusLineContext {
    let project_name = cwd
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project")
        .to_string();
    let mut ctx = StatusLineContext::default();
    ctx.set("project", project_name);
    ctx.set("cwd", cwd.display().to_string());
    if let Some(b) = git_branch {
        ctx.set("git_branch", b);
    }
    ctx.set("git_dirty", if git_dirty { "*" } else { "" });
    ctx.set(
        "model",
        settings
            .model
            .clone()
            .unwrap_or_else(|| "Sonnet".to_string()),
    );
    ctx.set(
        "permission_mode",
        permission_mode_label(settings.permission_mode),
    );
    if let Some(s) = session_id {
        let short: String = s.chars().take(8).collect();
        ctx.set("session_id", short);
    }
    ctx.set("context_percent", "0");
    ctx.set("estimated_cost", "$0.00");
    ctx
}

/// Choose a renderer using the configured preferences and detected caps.
pub fn choose_renderer(
    ui: &UiConfig,
    caps: &TerminalCapabilities,
    cli_override: Option<TerminalRenderer>,
) -> TerminalRenderer {
    let requested = cli_override.unwrap_or(ui.renderer);
    code_agent_tui::fallback_renderer_for(requested, ui.fallback_renderer, caps)
}

/// Helper for completions: emit a clap-generated completion script. The
/// caller passes the live `clap::Command` so we don't have to duplicate it.
pub fn write_completions<C: clap::CommandFactory>(
    shell: clap_complete::Shell,
    bin_name: &str,
    out: &mut dyn std::io::Write,
) -> Result<()> {
    let mut cmd = C::command();
    clap_complete::generate(shell, &mut cmd, bin_name, out);
    Ok(())
}

/// Parse the various `--older-than` strings used by `sessions clean`.
pub fn parse_age_spec(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix('d') {
        let n: u64 = rest.parse().map_err(|_| anyhow!("invalid age: {s}"))?;
        return Ok(std::time::Duration::from_secs(n * 86_400));
    }
    if let Some(rest) = s.strip_suffix('h') {
        let n: u64 = rest.parse().map_err(|_| anyhow!("invalid age: {s}"))?;
        return Ok(std::time::Duration::from_secs(n * 3_600));
    }
    if let Some(rest) = s.strip_suffix('m') {
        let n: u64 = rest.parse().map_err(|_| anyhow!("invalid age: {s}"))?;
        return Ok(std::time::Duration::from_secs(n * 60));
    }
    let n: u64 = s.parse().map_err(|_| anyhow!("invalid age: {s}"))?;
    Ok(std::time::Duration::from_secs(n))
}

/// Identify session JSONL files older than `age`. Returns the candidate
/// paths and their last-modified time. `dry_run=true` callers will print
/// without deleting; `dry_run=false` removes after that.
pub fn collect_clean_candidates(
    cwd: &Path,
    age: std::time::Duration,
) -> Vec<(PathBuf, std::time::SystemTime)> {
    let dir = FsSessionStore::sessions_dir_for(cwd);
    let mut out = Vec::new();
    let now = std::time::SystemTime::now();
    let cutoff = now.checked_sub(age).unwrap_or(std::time::UNIX_EPOCH);
    let read = match std::fs::read_dir(&dir) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for ent in read.flatten() {
        let p = ent.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let modified = ent
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        if modified <= cutoff {
            out.push((p, modified));
        }
    }
    out
}

/// Stub for renderer-aware notifications.
pub fn send_terminal_bell_for(reason: &str) {
    // Conservative default — print a BEL on stderr. The CLI flips this
    // behind a setting toggle.
    let _ = reason;
    use std::io::Write;
    let mut err = std::io::stderr();
    let _ = err.write_all(b"\x07");
    let _ = err.flush();
}

/// Recovery summary used by `code-agent sessions recover`.
pub fn recoverable_sessions(cwd: &Path, recovery: &RecoveryConfig) -> Vec<SessionRecoveryInfo> {
    futures::executor::block_on(detect_state(cwd, recovery.stale_lock_after_seconds))
        .unwrap_or_default()
        .into_iter()
        .filter(|i| {
            matches!(
                i.state,
                code_agent_core::SessionRuntimeState::Interrupted
                    | code_agent_core::SessionRuntimeState::Crashed
                    | code_agent_core::SessionRuntimeState::NeedsRecovery
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // Doctor reports a fail when ANTHROPIC_API_KEY is missing.
    fn doctor_fails_without_api_key() {
        let dir = TempDir::new().unwrap();
        let _g = EnvGuard::new("ANTHROPIC_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let report = run_doctor(dir.path(), &Settings::default(), true);
        let api = report
            .checks
            .iter()
            .find(|c| c.id == "provider.api_key")
            .unwrap();
        assert_eq!(api.status, DoctorStatus::Fail);
    }

    #[test]
    // Doctor JSON output is valid and doesn't expose the raw key value.
    fn doctor_json_output_redacts_secret() {
        let dir = TempDir::new().unwrap();
        let _g = EnvGuard::new("ANTHROPIC_API_KEY");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-XXXXXXXXXXXXXXXXXXXX");
        let report = run_doctor(dir.path(), &Settings::default(), true);
        let s = serde_json::to_string(&report.checks).unwrap();
        assert!(!s.contains("sk-XXXXXXXX"));
        assert!(s.contains("provider.api_key"));
    }

    #[test]
    // init --minimal writes settings.json and AGENTS.md if missing.
    fn init_minimal_creates_files() {
        let dir = TempDir::new().unwrap();
        let outcome = run_init(dir.path(), InitMode::Minimal, false).unwrap();
        assert!(outcome.created_agent_dir);
        assert!(outcome.created_settings);
        assert!(outcome.created_agents_md);
        assert!(dir.path().join(".agent/settings.json").exists());
        assert!(dir.path().join("AGENTS.md").exists());
    }

    #[test]
    // init does not overwrite existing files unless force=true.
    fn init_does_not_overwrite() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".agent")).unwrap();
        std::fs::write(dir.path().join(".agent/settings.json"), "preexisting").unwrap();
        let outcome = run_init(dir.path(), InitMode::Minimal, false).unwrap();
        assert!(!outcome.created_settings);
        let body = std::fs::read_to_string(dir.path().join(".agent/settings.json")).unwrap();
        assert_eq!(body, "preexisting");
    }

    #[test]
    // init --ci uses plain renderer in the generated settings.
    fn init_ci_uses_plain() {
        let dir = TempDir::new().unwrap();
        run_init(dir.path(), InitMode::Ci, false).unwrap();
        let body = std::fs::read_to_string(dir.path().join(".agent/settings.json")).unwrap();
        assert!(body.contains("\"plain\""));
    }

    #[test]
    // Generated settings parse back successfully.
    fn generated_settings_parse() {
        let dir = TempDir::new().unwrap();
        run_init(dir.path(), InitMode::Minimal, false).unwrap();
        let s =
            code_agent_core::settings::load(dir.path(), code_agent_core::SettingSource::Project)
                .unwrap();
        assert_eq!(s.ui.renderer.as_str(), "fullscreen");
    }

    #[test]
    // resolve_paths returns plausible defaults rooted at cwd/.agent.
    fn resolve_paths_uses_agent_dir() {
        let dir = TempDir::new().unwrap();
        let p = resolve_paths(dir.path());
        assert_eq!(p.agent_dir, dir.path().join(".agent"));
        assert_eq!(p.sessions_dir, dir.path().join(".agent/sessions"));
    }

    #[test]
    // Age spec parses common shorthand.
    fn parse_age_spec_handles_shorthand() {
        assert_eq!(parse_age_spec("30d").unwrap().as_secs(), 30 * 86_400);
        assert_eq!(parse_age_spec("2h").unwrap().as_secs(), 7_200);
        assert_eq!(parse_age_spec("60").unwrap().as_secs(), 60);
    }

    #[test]
    // Effective config formatter does not panic with default settings.
    fn config_formatter_runs() {
        let lines = format_effective_config(&Settings::default());
        assert!(lines.iter().any(|l| l.contains("Effective configuration")));
    }

    /// Restores an env var on drop so tests don't leak global state.
    struct EnvGuard {
        key: String,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn new(key: &str) -> Self {
            Self {
                key: key.to_string(),
                prior: std::env::var(key).ok(),
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prior {
                Some(v) => std::env::set_var(&self.key, v),
                None => std::env::remove_var(&self.key),
            }
        }
    }
}
