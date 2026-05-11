//! Slash-command summary types and built-in command enum.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Where a slash command came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SlashCommandSource {
    BuiltIn,
    Skill { skill_name: String },
    CustomFile { path: PathBuf },
}

impl SlashCommandSource {
    pub fn label(&self) -> &'static str {
        match self {
            Self::BuiltIn => "builtin",
            Self::Skill { .. } => "skill",
            Self::CustomFile { .. } => "custom",
        }
    }
}

/// Handler glue used at dispatch time. Public so SDK tests can inspect.
#[derive(Debug, Clone)]
pub enum SlashCommandHandler {
    BuiltIn(BuiltInSlashCommand),
    Skill(String),
    CustomFile(PathBuf),
}

/// Set of built-in commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum BuiltInSlashCommand {
    Help,
    Context,
    Usage,
    Tools,
    Skills,
    Agents,
    Checkpoints,
    Compact,
    /// M9: show effective configuration.
    Config,
    /// M9: show current run/session status.
    Status,
    /// M9: configure the status line template.
    StatusLine,
    /// M9: run diagnostics.
    Doctor,
    /// M9: list session changes.
    Changes,
    /// M9: show patch diff for the session/checkpoint.
    Diff,
    /// M9: revert to a checkpoint.
    Revert,
    /// M9: list known sessions.
    Sessions,
    /// M9: detect and recover an interrupted session.
    Recover,
    /// M10: open the provider login flow.
    Login,
    /// M10: log out of the active provider.
    Logout,
    /// M10: show stored auth credentials.
    Auth,
    /// M10: list or set the active model provider.
    Provider,
    /// M10: list or pick a model from the registry.
    Model,
}

impl BuiltInSlashCommand {
    pub fn name(self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::Context => "context",
            Self::Usage => "usage",
            Self::Tools => "tools",
            Self::Skills => "skills",
            Self::Agents => "agents",
            Self::Checkpoints => "checkpoints",
            Self::Compact => "compact",
            Self::Config => "config",
            Self::Status => "status",
            Self::StatusLine => "statusline",
            Self::Doctor => "doctor",
            Self::Changes => "changes",
            Self::Diff => "diff",
            Self::Revert => "revert",
            Self::Sessions => "sessions",
            Self::Recover => "recover",
            Self::Login => "login",
            Self::Logout => "logout",
            Self::Auth => "auth",
            Self::Provider => "provider",
            Self::Model => "model",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Help => "List available slash commands.",
            Self::Context => "Show conversation/turn/budget context.",
            Self::Usage => "Show token and cost usage for the session.",
            Self::Tools => "Show loaded and deferred tools.",
            Self::Skills => "List available Skills.",
            Self::Agents => "List available subagents.",
            Self::Checkpoints => "List recent workspace checkpoints.",
            Self::Compact => "Manually compact older conversation history.",
            Self::Config => "Show effective configuration and source locations.",
            Self::Status => "Show current run/session status.",
            Self::StatusLine => "View or change the status line template.",
            Self::Doctor => "Run diagnostic checks.",
            Self::Changes => "List file changes made during this session.",
            Self::Diff => "Show patch diff for current changes or a checkpoint.",
            Self::Revert => "Revert workspace files to a checkpoint.",
            Self::Sessions => "List known sessions for this workspace.",
            Self::Recover => "Detect and recover an interrupted session.",
            Self::Login => "Login to a model-access provider.",
            Self::Logout => "Logout (delete stored credentials).",
            Self::Auth => "Show or manage stored auth credentials.",
            Self::Provider => "List or set the active model provider.",
            Self::Model => "List or pick a model from the registry.",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "help" => Self::Help,
            "context" => Self::Context,
            "usage" => Self::Usage,
            "tools" => Self::Tools,
            "skills" => Self::Skills,
            "agents" => Self::Agents,
            "checkpoints" => Self::Checkpoints,
            "compact" => Self::Compact,
            "config" => Self::Config,
            "status" => Self::Status,
            "statusline" => Self::StatusLine,
            "doctor" => Self::Doctor,
            "changes" => Self::Changes,
            "diff" => Self::Diff,
            "revert" => Self::Revert,
            "sessions" => Self::Sessions,
            "recover" => Self::Recover,
            "login" => Self::Login,
            "logout" => Self::Logout,
            "auth" => Self::Auth,
            "provider" => Self::Provider,
            "model" => Self::Model,
            _ => return None,
        })
    }

    pub fn all() -> &'static [BuiltInSlashCommand] {
        &[
            Self::Help,
            Self::Context,
            Self::Usage,
            Self::Tools,
            Self::Skills,
            Self::Agents,
            Self::Checkpoints,
            Self::Compact,
            Self::Config,
            Self::Status,
            Self::StatusLine,
            Self::Doctor,
            Self::Changes,
            Self::Diff,
            Self::Revert,
            Self::Sessions,
            Self::Recover,
            Self::Login,
            Self::Logout,
            Self::Auth,
            Self::Provider,
            Self::Model,
        ]
    }
}

/// Compact summary of a slash command for `/help`, `commands list`, and
/// `SystemInit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlashCommandSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub description: String,
    pub source: SlashCommandSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// Build a SlashCommandSummary for a built-in.
pub fn builtin_command_summary(b: BuiltInSlashCommand) -> SlashCommandSummary {
    SlashCommandSummary {
        name: b.name().to_string(),
        namespace: None,
        description: b.description().to_string(),
        source: SlashCommandSource::BuiltIn,
        argument_hint: None,
    }
}

/// Result of dispatching a built-in command synchronously (i.e. without
/// going through the model).
#[derive(Debug, Clone)]
pub struct SlashBuiltinResult {
    /// Plain-text body to display to the user.
    pub display: String,
}
