use std::path::PathBuf;
use thiserror::Error;

/// Errors raised while validating, loading, or invoking Skill definitions.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("skill name is required")]
    MissingName,
    #[error(
        "skill name {name:?} is not identifier-safe (allowed: a-z, 0-9, '-', '_'; \
        must be 1..64 chars; no path separators or shell metacharacters)"
    )]
    UnsafeName { name: String },
    #[error("skill {name:?}: description is required")]
    MissingDescription { name: String },
    #[error("skill {name:?}: SKILL.md body is required")]
    MissingBody { name: String },
    #[error("duplicate skill name {name:?}")]
    DuplicateName { name: String },
    #[error("skill file at {path}: I/O error: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("skill file at {path}: missing or malformed `---` frontmatter delimiters")]
    MissingFrontmatter { path: PathBuf },
    #[error("skill file at {path}: invalid frontmatter: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },
    #[error("skill file at {path}: {message}")]
    InvalidField { path: PathBuf, message: String },
    #[error(
        "skill path {path} is not contained inside skills root {root} \
        (symlink-escape or path-traversal rejected)"
    )]
    UnsafePath { path: PathBuf, root: PathBuf },
    #[error("skill {name:?}: SKILL.md exceeds {max} bytes")]
    SkillMdTooLarge { name: String, max: usize },
    #[error("skill {name:?}: total resource bytes exceed {max}")]
    ResourcesTooLarge { name: String, max: usize },
    #[error("skill {name:?}: resource path {path} escapes the skill directory")]
    ResourceEscape { name: String, path: String },
    #[error("skill {name:?}: not found")]
    NotFound { name: String },
    #[error("skill {name:?}: blocked: {reason}")]
    Blocked { name: String, reason: String },
    #[error("skill {name:?}: maximum loaded skills per run ({max}) exceeded")]
    LoadedLimitExceeded { name: String, max: usize },
    #[error("skills are disabled by configuration")]
    Disabled,
    #[error("{0}")]
    Other(String),
}
