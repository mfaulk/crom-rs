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
