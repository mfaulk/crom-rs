//! Loader for custom slash command files (`.agent/commands/*.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::agents::yaml::{parse_frontmatter, YamlValue};
use crate::tool_rule::ToolRule;

use super::parser::is_identifier_safe_command_name;

/// Maximum size of a single command file.
pub const MAX_COMMAND_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum CustomSlashCommandError {
    #[error("command file at {path}: I/O error: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command file at {path}: missing or malformed `---` frontmatter delimiters")]
    MissingFrontmatter { path: PathBuf },
    #[error("command file at {path}: invalid frontmatter: {message}")]
    InvalidFrontmatter { path: PathBuf, message: String },
    #[error("command file at {path}: body is required")]
    MissingBody { path: PathBuf },
    #[error("command file at {path}: file exceeds {max} bytes")]
    TooLarge { path: PathBuf, max: usize },
    #[error(
        "command path {path} is not contained inside commands root {root} \
        (symlink-escape rejected)"
    )]
    UnsafePath { path: PathBuf, root: PathBuf },
    #[error("command file at {path}: derived name {name:?} is not identifier-safe")]
    UnsafeName { path: PathBuf, name: String },
    #[error("command file at {path}: {message}")]
    InvalidField { path: PathBuf, message: String },
}

/// A typed custom slash-command definition.
#[derive(Debug, Clone)]
pub struct CustomSlashCommandDefinition {
    pub name: String,
    pub namespace: Option<String>,
    pub description: String,
    pub argument_hint: Option<String>,
    pub allowed_tools: Vec<ToolRule>,
    pub disallowed_tools: Vec<ToolRule>,
    pub model: Option<String>,
    pub body: String,
    pub source_path: PathBuf,
}

impl CustomSlashCommandDefinition {
    pub fn full_name(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{}:{}", ns, self.name),
            None => self.name.clone(),
        }
    }
}

/// Convenience: the canonical project-local commands directory.
pub fn custom_commands_dir(cwd: &Path) -> PathBuf {
    cwd.join(".agent").join("commands")
}

/// Load every `.md` file under `dir`, including files in immediate
/// subdirectories (which produce namespaced commands like `team:review`).
pub fn load_custom_commands(
    dir: &Path,
) -> (
    Vec<CustomSlashCommandDefinition>,
    Vec<CustomSlashCommandError>,
) {
    let mut defs = Vec::new();
    let mut errors = Vec::new();
    if !dir.exists() {
        return (defs, errors);
    }
    let canonical_root = match dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            errors.push(CustomSlashCommandError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
            return (defs, errors);
        }
    };
    walk(
        &canonical_root,
        &canonical_root,
        None,
        &mut defs,
        &mut errors,
    );
    defs.sort_by_key(|a| a.full_name());
    (defs, errors)
}

fn walk(
    root: &Path,
    dir: &Path,
    namespace: Option<&str>,
    defs: &mut Vec<CustomSlashCommandDefinition>,
    errors: &mut Vec<CustomSlashCommandError>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            errors.push(CustomSlashCommandError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
            return;
        }
    };
    let mut sub_paths: Vec<PathBuf> = Vec::new();
    let mut sub_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            sub_dirs.push(p);
        } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
            sub_paths.push(p);
        }
    }
    sub_paths.sort();
    sub_dirs.sort();
    for p in sub_paths {
        match load_one(root, &p, namespace) {
            Ok(d) => defs.push(d),
            Err(e) => errors.push(e),
        }
    }
    for sub_dir in sub_dirs {
        let dir_name = match sub_dir.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !is_identifier_safe_command_name(&dir_name) {
            errors.push(CustomSlashCommandError::UnsafeName {
                path: sub_dir.clone(),
                name: dir_name,
            });
            continue;
        }
        // Symlink-escape and symlink-loop protection: canonicalize each
        // subdir before recursing and refuse to follow anything that
        // resolves outside the commands root. Without this, a symlinked
        // directory could either escape the commands tree or loop forever
        // during discovery.
        let canonical_sub = match sub_dir.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                errors.push(CustomSlashCommandError::UnsafePath {
                    path: sub_dir.clone(),
                    root: root.to_path_buf(),
                });
                continue;
            }
        };
        if !canonical_sub.starts_with(root) {
            errors.push(CustomSlashCommandError::UnsafePath {
                path: sub_dir.clone(),
                root: root.to_path_buf(),
            });
            continue;
        }
        let nested_ns = match namespace {
            Some(parent) => format!("{}:{}", parent, dir_name),
            None => dir_name,
        };
        walk(root, &canonical_sub, Some(&nested_ns), defs, errors);
    }
}

fn load_one(
    root: &Path,
    path: &Path,
    namespace: Option<&str>,
) -> Result<CustomSlashCommandDefinition, CustomSlashCommandError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|e| CustomSlashCommandError::Io {
            path: root.to_path_buf(),
            source: e,
        })?;
    let canonical_path = path
        .canonicalize()
        .map_err(|_| CustomSlashCommandError::UnsafePath {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(CustomSlashCommandError::UnsafePath {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        });
    }
    let bytes = std::fs::read(&canonical_path).map_err(|source| CustomSlashCommandError::Io {
        path: canonical_path.clone(),
        source,
    })?;
    if bytes.len() > MAX_COMMAND_FILE_BYTES {
        return Err(CustomSlashCommandError::TooLarge {
            path: canonical_path,
            max: MAX_COMMAND_FILE_BYTES,
        });
    }
    let text = String::from_utf8(bytes).map_err(|e| CustomSlashCommandError::InvalidField {
        path: canonical_path.clone(),
        message: format!("file is not valid UTF-8: {e}"),
    })?;
    parse_custom_command(&text, &canonical_path, namespace)
}

/// Parse the text of a custom command file. Public so SDK callers and
/// tests can avoid disk I/O.
pub fn parse_custom_command(
    text: &str,
    path: &Path,
    namespace: Option<&str>,
) -> Result<CustomSlashCommandDefinition, CustomSlashCommandError> {
    let (frontmatter_body, body) = split_frontmatter(text, path)?;
    let map = parse_frontmatter(frontmatter_body).map_err(|e| {
        CustomSlashCommandError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    })?;

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();
    if !is_identifier_safe_command_name(&stem) {
        return Err(CustomSlashCommandError::UnsafeName {
            path: path.to_path_buf(),
            name: stem,
        });
    }

    let mut def = CustomSlashCommandDefinition {
        name: stem,
        namespace: namespace.map(|s| s.to_string()),
        description: String::new(),
        argument_hint: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        model: None,
        body: body.trim().to_string(),
        source_path: path.to_path_buf(),
    };
    if def.body.is_empty() {
        return Err(CustomSlashCommandError::MissingBody {
            path: path.to_path_buf(),
        });
    }
    apply_frontmatter(&mut def, &map, path)?;
    Ok(def)
}

fn split_frontmatter<'a>(
    text: &'a str,
    path: &Path,
) -> Result<(&'a str, &'a str), CustomSlashCommandError> {
    let trimmed_start = text.trim_start_matches('\u{FEFF}');
    if !trimmed_start.starts_with("---") {
        return Err(CustomSlashCommandError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }
    let after_open = trimmed_start.strip_prefix("---").unwrap_or(trimmed_start);
    let after_open = match after_open.strip_prefix('\n') {
        Some(s) => s,
        None => match after_open.strip_prefix("\r\n") {
            Some(s) => s,
            None => {
                return Err(CustomSlashCommandError::MissingFrontmatter {
                    path: path.to_path_buf(),
                });
            }
        },
    };
    let mut search_from = 0usize;
    let close_offset = loop {
        let needle = "\n---";
        let Some(rel) = after_open[search_from..].find(needle) else {
            return Err(CustomSlashCommandError::MissingFrontmatter {
                path: path.to_path_buf(),
            });
        };
        let abs = search_from + rel;
        let after = abs + needle.len();
        let tail = &after_open[after..];
        if tail.is_empty()
            || tail.starts_with('\n')
            || tail.starts_with("\r\n")
            || tail.chars().next().map(|c| c == '\r').unwrap_or(false)
        {
            break abs;
        }
        search_from = after;
    };
    let frontmatter = &after_open[..close_offset];
    let after_close = &after_open[close_offset + 4..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);
    let body = body.strip_prefix("\r\n").unwrap_or(body);
    Ok((frontmatter, body))
}

fn apply_frontmatter(
    def: &mut CustomSlashCommandDefinition,
    map: &BTreeMap<String, YamlValue>,
    path: &Path,
) -> Result<(), CustomSlashCommandError> {
    let invalid = |message: &str| CustomSlashCommandError::InvalidField {
        path: path.to_path_buf(),
        message: message.to_string(),
    };
    for (k, v) in map {
        match k.as_str() {
            "description" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`description` must be a string"))?;
                def.description = s.to_string();
            }
            "argument-hint" | "argument_hint" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`argument-hint` must be a string"))?;
                if !s.is_empty() {
                    def.argument_hint = Some(s.to_string());
                }
            }
            "allowed-tools" | "allowed_tools" => {
                def.allowed_tools = parse_tool_rule_list(v).map_err(|e| invalid(&e))?;
            }
            "disallowed-tools" | "disallowed_tools" => {
                def.disallowed_tools = parse_tool_rule_list(v).map_err(|e| invalid(&e))?;
            }
            "model" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`model` must be a string"))?;
                if !s.is_empty() {
                    def.model = Some(s.to_string());
                }
            }
            "name" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`name` must be a string"))?;
                if !s.is_empty() {
                    if !is_identifier_safe_command_name(s) {
                        return Err(CustomSlashCommandError::UnsafeName {
                            path: path.to_path_buf(),
                            name: s.to_string(),
                        });
                    }
                    def.name = s.to_string();
                }
            }
            other => {
                tracing::debug!(unknown = other, "ignoring unknown command frontmatter key");
            }
        }
    }
    Ok(())
}

fn parse_tool_rule_list(v: &YamlValue) -> Result<Vec<ToolRule>, String> {
    let list = v
        .as_list()
        .ok_or_else(|| "expected a list of tool rules".to_string())?;
    let mut rules = Vec::with_capacity(list.len());
    for item in list {
        let s = item
            .as_string()
            .ok_or_else(|| "tool rule entries must be strings".to_string())?;
        rules.push(parse_tool_rule_string(s)?);
    }
    Ok(rules)
}

/// Parse a single tool rule string. Recognizes:
/// - `Tool` -> ExactTool
/// - `Tool*`/glob -> ToolGlob
/// - `Bash(pattern)` -> BashCommandRegex (with `*` -> `.*`)
/// - `Edit(path-glob)` etc -> FilePathGlob
pub(crate) fn parse_tool_rule_string(s: &str) -> Result<ToolRule, String> {
    if let Some(open) = s.find('(') {
        if !s.ends_with(')') {
            return Err(format!("tool rule {s:?} missing closing `)`"));
        }
        let tool = s[..open].trim();
        let inner = &s[open + 1..s.len() - 1];
        if tool.is_empty() {
            return Err(format!("tool rule {s:?} missing tool name"));
        }
        if tool == "Bash" {
            return Ok(ToolRule::BashCommandRegex(bash_pattern_to_regex(inner)));
        }
        return Ok(ToolRule::FilePathGlob {
            tool: tool.to_string(),
            pattern: inner.to_string(),
        });
    }
    if s.contains('*') || s.contains('?') {
        return ToolRule::try_tool_glob(s.to_string());
    }
    Ok(ToolRule::ExactTool(s.to_string()))
}

fn bash_pattern_to_regex(pattern: &str) -> String {
    let mut out = String::from("^");
    for c in pattern.chars() {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '\\' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    // A basic command file parses cleanly.
    fn parses_basic_command() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("review-changes.md");
        std::fs::write(
            &path,
            "---\ndescription: Review changes since base\nargument-hint: <base-ref>\nallowed-tools:\n  - Read\n  - Bash(git diff *)\n---\nReview the changes since $1.\n!`git diff $1`\n",
        )
        .unwrap();
        let def = load_custom_commands(dir.path())
            .0
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(def.name, "review-changes");
        assert_eq!(def.argument_hint.as_deref(), Some("<base-ref>"));
        assert!(def.body.contains("Review the changes since $1."));
        assert_eq!(def.allowed_tools.len(), 2);
    }

    #[test]
    // Subdirectory commands acquire a namespace.
    fn subdirectory_command_namespaced() {
        let dir = TempDir::new().unwrap();
        let group = dir.path().join("team");
        std::fs::create_dir_all(&group).unwrap();
        std::fs::write(
            group.join("review.md"),
            "---\ndescription: team review\n---\nbody\n",
        )
        .unwrap();
        let (defs, errs) = load_custom_commands(dir.path());
        assert!(errs.is_empty());
        assert_eq!(defs[0].full_name(), "team:review");
    }

    #[test]
    // Missing body is rejected.
    fn missing_body_rejected() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("x.md"), "---\ndescription: d\n---\n").unwrap();
        let (_, errs) = load_custom_commands(dir.path());
        assert!(matches!(
            errs[0],
            CustomSlashCommandError::MissingBody { .. }
        ));
    }

    #[test]
    // Files larger than the cap are rejected.
    fn oversize_rejected() {
        let dir = TempDir::new().unwrap();
        let big = format!(
            "---\ndescription: d\n---\n{}\n",
            "x".repeat(MAX_COMMAND_FILE_BYTES + 100)
        );
        std::fs::write(dir.path().join("big.md"), big).unwrap();
        let (_, errs) = load_custom_commands(dir.path());
        assert!(matches!(errs[0], CustomSlashCommandError::TooLarge { .. }));
    }
}
