use std::path::PathBuf;
use thiserror::Error;

/// Errors raised by the slash-command parser, dispatcher, and expansion engine.
#[derive(Debug, Error)]
pub enum SlashCommandError {
    #[error("not a slash command")]
    NotASlashCommand,
    #[error("empty command")]
    EmptyCommand,
    #[error(
        "unsafe command name {name:?} (allowed: a-z, 0-9, '-', '_'; \
        no path separators or shell metacharacters)"
    )]
    UnsafeName { name: String },
    #[error("unknown slash command: {name}")]
    Unknown { name: String },
    #[error("slash commands are disabled by configuration")]
    Disabled,
    #[error("custom command file at {path}: {message}")]
    InvalidCustom { path: PathBuf, message: String },
    #[error("expansion failed: {0}")]
    ExpansionFailed(String),
    #[error("expansion of `{token}` is disabled by configuration")]
    ExpansionDisabled { token: String },
    #[error("expanded prompt exceeds {max} bytes")]
    PromptTooLarge { max: usize },
    #[error("missing positional argument ${index} in command {name:?}")]
    MissingPositional { name: String, index: usize },
    #[error("{0}")]
    Other(String),
}
