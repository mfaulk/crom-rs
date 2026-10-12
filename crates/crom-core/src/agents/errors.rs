use std::path::PathBuf;
use thiserror::Error;

/// Errors raised while validating or loading agent definitions.
#[derive(Debug, Error)]
pub enum AgentDefinitionError {
    #[error("agent name is required")]
    MissingName,
    #[error(
        "agent name {name:?} is not identifier-safe (allowed: a-z, 0-9, '-', '_'; \
        must be 1..64 chars; no path separators or shell metacharacters)"
    )]
    UnsafeName { name: String },
    #[error("agent {name:?}: description is required")]
    MissingDescription { name: String },
    #[error("agent {name:?}: prompt is required")]
    MissingPrompt { name: String },
    #[error("agent {name:?}: max_turns must be 1..={max}, got {actual}")]
    MaxTurnsOutOfRange { name: String, actual: u32, max: u32 },
    #[error("duplicate agent name {name:?}")]
    DuplicateName { name: String },
    #[error(
        "agent {name:?} references the built-in `Agent` tool, but recursive subagents are not \
        supported in M6. Remove `Agent` from `tools` or add it to `disallowed_tools`."
    )]
    RecursiveAgentTool { name: String },
    #[error("agent {name:?} references unknown tool(s): {tools}")]
    UnknownTools { name: String, tools: String },
    #[error("agent definition file at {path}: I/O error: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("agent definition file at {path}: missing or malformed `---` frontmatter delimiters")]
    MissingFrontmatter { path: PathBuf },
    #[error("agent definition file at {path}: invalid frontmatter: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },
    #[error(
        "agent definition file at {path}: prompt body is empty (the markdown body after \
        `---` is the agent prompt and must not be blank)"
    )]
    EmptyPrompt { path: PathBuf },
    #[error("agent definition file at {path}: {message}")]
    InvalidField { path: PathBuf, message: String },
    #[error(
        "agent definition path {path} is not contained inside agents directory {root} \
        (symlink-escape or path-traversal rejected)"
    )]
    UnsafePath { path: PathBuf, root: PathBuf },
    #[error("agent definition file at {path}: prompt exceeds {max} bytes")]
    PromptTooLarge { path: PathBuf, max: usize },
    #[error("{0}")]
    Other(String),
}
