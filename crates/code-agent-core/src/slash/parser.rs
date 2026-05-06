//! Slash-command tokenizer.
//!
//! Recognizes:
//! - `/name args...`
//! - `/namespace:name args...`
//!
//! The parser preserves quoted strings (`"a b"`, `'c d'`). Names must be
//! identifier-safe to keep the dispatch path injection-free.

use serde::{Deserialize, Serialize};

use super::errors::SlashCommandError;

pub const MAX_COMMAND_NAME_LEN: usize = 64;

/// A parsed slash invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedSlashCommand {
    pub namespace: Option<String>,
    pub name: String,
    pub arguments: Vec<String>,
    /// Raw argument text (everything after the command name, with leading
    /// whitespace trimmed). Useful for `$ARGUMENTS` substitution.
    pub raw_arguments: String,
}

impl ParsedSlashCommand {
    /// Fully-qualified command identifier suitable for logging and event
    /// payloads (e.g. `release-pr` or `team:review`).
    pub fn full_name(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}:{}", ns, self.name),
            None => self.name.clone(),
        }
    }
}

/// Parse user input as a slash command. Returns `NotASlashCommand` for
/// inputs that don't begin with `/`, so callers can fall back to a normal
/// model turn.
pub fn parse_slash_command(input: &str) -> Result<ParsedSlashCommand, SlashCommandError> {
    let trimmed = input.trim_start();
    let rest = trimmed
        .strip_prefix('/')
        .ok_or(SlashCommandError::NotASlashCommand)?;
    if rest.is_empty() {
        return Err(SlashCommandError::EmptyCommand);
    }
    let mut split = rest.splitn(2, char::is_whitespace);
    let head = split.next().unwrap_or("");
    let raw_args = split.next().unwrap_or("").trim();

    let (namespace, name) = match head.split_once(':') {
        Some((ns, n)) => (Some(ns.to_string()), n.to_string()),
        None => (None, head.to_string()),
    };
    if let Some(ns) = &namespace {
        if !is_identifier_safe_command_name(ns) {
            return Err(SlashCommandError::UnsafeName { name: ns.clone() });
        }
    }
    if !is_identifier_safe_command_name(&name) {
        return Err(SlashCommandError::UnsafeName { name });
    }
    let arguments = tokenize_arguments(raw_args);
    Ok(ParsedSlashCommand {
        namespace,
        name,
        arguments,
        raw_arguments: raw_args.to_string(),
    })
}

/// Identifier-safe command name. Same allowlist as Skill names.
pub fn is_identifier_safe_command_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_COMMAND_NAME_LEN {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_'))
}

/// Tokenize an argument string with double- and single-quote handling.
fn tokenize_arguments(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_dq = false;
    let mut in_sq = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_sq => in_dq = !in_dq,
            '\'' if !in_dq => in_sq = !in_sq,
            '\\' if in_dq => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_dq && !in_sq => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Bare /help parses with no arguments.
    fn parses_bare_command() {
        let p = parse_slash_command("/help").unwrap();
        assert_eq!(p.name, "help");
        assert!(p.namespace.is_none());
        assert!(p.arguments.is_empty());
    }

    #[test]
    // /release-pr v0.4.0 parses with one positional arg.
    fn parses_command_with_args() {
        let p = parse_slash_command("/release-pr v0.4.0").unwrap();
        assert_eq!(p.name, "release-pr");
        assert_eq!(p.arguments, vec!["v0.4.0"]);
        assert_eq!(p.raw_arguments, "v0.4.0");
    }

    #[test]
    // Namespaced commands parse the namespace separately.
    fn parses_namespaced_command() {
        let p = parse_slash_command("/team:review src/lib.rs").unwrap();
        assert_eq!(p.namespace.as_deref(), Some("team"));
        assert_eq!(p.name, "review");
        assert_eq!(p.arguments, vec!["src/lib.rs"]);
    }

    #[test]
    // Quoted arguments preserve embedded whitespace.
    fn quoted_args_preserved() {
        let p = parse_slash_command("/note \"hello world\" tag").unwrap();
        assert_eq!(p.arguments, vec!["hello world", "tag"]);
    }

    #[test]
    // Inputs that don't start with / are not slash commands.
    fn non_slash_input_returns_not_a_command() {
        let err = parse_slash_command("hi there").unwrap_err();
        assert!(matches!(err, SlashCommandError::NotASlashCommand));
    }

    #[test]
    // Just `/` is empty.
    fn bare_slash_is_empty() {
        let err = parse_slash_command("/").unwrap_err();
        assert!(matches!(err, SlashCommandError::EmptyCommand));
    }

    #[test]
    // Unsafe names are rejected.
    fn unsafe_command_rejected() {
        let err = parse_slash_command("/../etc/passwd arg").unwrap_err();
        assert!(matches!(err, SlashCommandError::UnsafeName { .. }));
        // Names containing path separators or shell metacharacters are
        // rejected outright.
        let err = parse_slash_command("/foo;rm").unwrap_err();
        assert!(matches!(err, SlashCommandError::UnsafeName { .. }));
        let err = parse_slash_command("/path/to").unwrap_err();
        assert!(matches!(err, SlashCommandError::UnsafeName { .. }));
    }

    #[test]
    // Leading whitespace is tolerated.
    fn leading_whitespace_tolerated() {
        let p = parse_slash_command("   /help").unwrap();
        assert_eq!(p.name, "help");
    }
}
