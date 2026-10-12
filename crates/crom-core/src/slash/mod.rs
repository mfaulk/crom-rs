//! Slash commands (M7).
//!
//! A *slash command* is an explicit invocation of a workflow that the user
//! types at the start of their prompt. The dispatcher recognizes:
//! - Built-in commands (`/help`, `/context`, `/usage`, `/tools`, `/skills`,
//!   `/agents`, `/checkpoints`, `/compact`).
//! - Skill-backed commands (`/release-pr v0.4.0` calls the `release-pr`
//!   Skill).
//! - Custom command files at `.agent/commands/*.md` with frontmatter.
//!
//! Custom command bodies support safe placeholder, `@file`, and `!`bash``
//! expansion. All expansion routes through the existing tool / permission
//! pipeline — no shortcut around hooks or approvals.

pub mod commands;
pub mod custom;
pub mod errors;
pub mod expansion;
pub mod parser;
pub mod registry;

pub use commands::{
    builtin_command_summary, BuiltInSlashCommand, SlashBuiltinResult, SlashCommandHandler,
    SlashCommandSource, SlashCommandSummary,
};
pub use custom::{
    custom_commands_dir, load_custom_commands, parse_custom_command, CustomSlashCommandDefinition,
    CustomSlashCommandError, MAX_COMMAND_FILE_BYTES,
};
pub use errors::SlashCommandError;
pub use expansion::{
    expand_custom_command, render_synthetic_skill_prompt, ExpandedToolCallSummary,
    SlashExpansionOptions, SlashExpansionResult,
};
pub use parser::{
    is_identifier_safe_command_name, parse_slash_command, ParsedSlashCommand, MAX_COMMAND_NAME_LEN,
};
pub use registry::{SlashCommandDispatch, SlashCommandRegistry};
