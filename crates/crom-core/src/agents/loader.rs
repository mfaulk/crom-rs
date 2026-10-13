//! Loader for `.agent/agents/<name>.md` definitions.
//!
//! Each `.md` file is split into:
//! - YAML frontmatter delimited by `---` lines (must be the first content of
//!   the file).
//! - A markdown body that becomes the agent's prompt.
//!
//! The parser does not execute or evaluate anything in the file. It also
//! refuses to follow symlinks that escape the agents directory and rejects
//! filenames containing path separators.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::permission::PermissionMode;
use crate::tool_catalog::ToolSearchOptions;
use crate::tool_rule::ToolRule;

use super::definition::{
    is_identifier_safe_agent_name, AgentConcurrencyHint, AgentDefinition, AgentOutputMode,
    DEFAULT_MAX_TURNS,
};
use super::errors::AgentDefinitionError;
use super::runtime::SubagentCheckpointPolicy;
use super::yaml::{parse_frontmatter, YamlValue};

/// Maximum size of a single agent definition file. Generous, but bounded so a
/// pathological file cannot exhaust memory.
pub const MAX_AGENT_FILE_BYTES: usize = 256 * 1024;

/// Convenience: the canonical project-local agents directory.
pub fn filesystem_agents_dir(cwd: &Path) -> PathBuf {
    cwd.join(".agent").join("agents")
}

/// Load every `*.md` definition under `dir`. Skips non-`.md` files and files
/// that are *not* contained inside `dir` (symlink-escape protection). Errors
/// from individual files are collected and returned as the second element of
/// the tuple so the CLI can show partial successes.
///
/// The "strict" variant short-circuits on the first error.
pub fn load_filesystem_agents(dir: &Path) -> (Vec<AgentDefinition>, Vec<AgentDefinitionError>) {
    let mut defs = Vec::new();
    let mut errors = Vec::new();
    let canonical_dir = match canonicalize_or_keep(dir) {
        Some(p) => p,
        None => {
            // Directory does not exist — that's fine; there are no agents.
            return (defs, errors);
        }
    };
    let entries = match std::fs::read_dir(&canonical_dir) {
        Ok(rd) => rd,
        Err(e) => {
            errors.push(AgentDefinitionError::Io {
                path: canonical_dir,
                source: e,
            });
            return (defs, errors);
        }
    };
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        paths.push(p);
    }
    paths.sort();
    for p in paths {
        match load_one(&canonical_dir, &p) {
            Ok(def) => defs.push(def),
            Err(e) => errors.push(e),
        }
    }
    (defs, errors)
}

/// Strict variant of [`load_filesystem_agents`]. Returns `Err` on the first
/// failure and discards any successful loads.
pub fn load_filesystem_agents_strict(
    dir: &Path,
) -> Result<Vec<AgentDefinition>, AgentDefinitionError> {
    let (defs, errors) = load_filesystem_agents(dir);
    if let Some(first) = errors.into_iter().next() {
        return Err(first);
    }
    Ok(defs)
}

fn canonicalize_or_keep(dir: &Path) -> Option<PathBuf> {
    if !dir.exists() {
        return None;
    }
    Some(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()))
}

fn load_one(root: &Path, path: &Path) -> Result<AgentDefinition, AgentDefinitionError> {
    // Symlink-escape protection: canonicalize BOTH the root and the file
    // path and require the file to live under the root. If `canonicalize`
    // fails on either side we fail closed — the lexical `starts_with`
    // check is not a substitute for canonicalization (e.g. a dangling
    // symlink could pass the lexical check and then resolve to something
    // outside `root` on the subsequent `read`). Better to refuse than
    // open a TOCTOU window.
    let canonical_root = root.canonicalize().map_err(|e| AgentDefinitionError::Io {
        path: root.to_path_buf(),
        source: e,
    })?;
    let canonical_path = path
        .canonicalize()
        .map_err(|_| AgentDefinitionError::UnsafePath {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        })?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(AgentDefinitionError::UnsafePath {
            path: path.to_path_buf(),
            root: root.to_path_buf(),
        });
    }
    let bytes = std::fs::read(path).map_err(|source| AgentDefinitionError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() > MAX_AGENT_FILE_BYTES {
        return Err(AgentDefinitionError::PromptTooLarge {
            path: path.to_path_buf(),
            max: MAX_AGENT_FILE_BYTES,
        });
    }
    let text = String::from_utf8(bytes).map_err(|e| AgentDefinitionError::InvalidField {
        path: path.to_path_buf(),
        message: format!("file is not valid UTF-8: {e}"),
    })?;
    parse_definition_file(&text, path)
}

/// Parse an agent-definition file's text. Public so unit tests and callers
/// who already hold the raw bytes can avoid re-reading from disk.
pub fn parse_definition_file(
    text: &str,
    path: &Path,
) -> Result<AgentDefinition, AgentDefinitionError> {
    let (frontmatter_body, prompt_body) = split_frontmatter(text, path)?;
    let map = parse_frontmatter(frontmatter_body).map_err(|e| {
        AgentDefinitionError::InvalidFrontmatter {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    })?;

    let mut def = AgentDefinition {
        prompt: prompt_body.trim().to_string(),
        ..AgentDefinition::default()
    };
    if def.prompt.is_empty() {
        return Err(AgentDefinitionError::EmptyPrompt {
            path: path.to_path_buf(),
        });
    }

    apply_frontmatter(&mut def, &map, path)?;

    // If the frontmatter omitted `name`, fall back to the file stem.
    if def.name.is_empty() {
        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
            def.name = stem.to_string();
        }
    }
    if !is_identifier_safe_agent_name(&def.name) {
        return Err(AgentDefinitionError::UnsafeName { name: def.name });
    }

    def.validate_structural()?;
    Ok(def)
}

fn split_frontmatter<'a>(
    text: &'a str,
    path: &Path,
) -> Result<(&'a str, &'a str), AgentDefinitionError> {
    let trimmed_start = text.trim_start_matches('\u{FEFF}');
    if !trimmed_start.starts_with("---") {
        return Err(AgentDefinitionError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }
    let after_open = trimmed_start.strip_prefix("---").unwrap_or(trimmed_start);
    // Require a newline immediately after the opening `---`.
    let after_open = match after_open.strip_prefix('\n') {
        Some(s) => s,
        None => match after_open.strip_prefix("\r\n") {
            Some(s) => s,
            None => {
                return Err(AgentDefinitionError::MissingFrontmatter {
                    path: path.to_path_buf(),
                });
            }
        },
    };
    // Find the closing `---` on its own line.
    let mut search_from = 0usize;
    let close_offset = loop {
        let needle = "\n---";
        let Some(rel) = after_open[search_from..].find(needle) else {
            return Err(AgentDefinitionError::MissingFrontmatter {
                path: path.to_path_buf(),
            });
        };
        let abs = search_from + rel;
        let after = abs + needle.len();
        // The closing fence must be either end-of-input or followed by a
        // newline; nothing else on the same line.
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
    // The body starts right after the closing fence + its newline if any.
    let after_close = &after_open[close_offset + 4..];
    let body = after_close.strip_prefix('\n').unwrap_or(after_close);
    let body = body.strip_prefix("\r\n").unwrap_or(body);
    Ok((frontmatter, body))
}

#[allow(clippy::too_many_lines)]
fn apply_frontmatter(
    def: &mut AgentDefinition,
    map: &BTreeMap<String, YamlValue>,
    path: &Path,
) -> Result<(), AgentDefinitionError> {
    let invalid = |message: &str| AgentDefinitionError::InvalidField {
        path: path.to_path_buf(),
        message: message.to_string(),
    };
    for (k, v) in map {
        match k.as_str() {
            "name" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`name` must be a string"))?;
                def.name = s.to_string();
            }
            "description" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`description` must be a string"))?;
                def.description = s.to_string();
            }
            // Body wins over a frontmatter-supplied `prompt`.
            "prompt" => {
                if def.prompt.is_empty() {
                    let s = v
                        .as_string()
                        .ok_or_else(|| invalid("`prompt` must be a string"))?;
                    def.prompt = s.to_string();
                }
            }
            "model" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`model` must be a string"))?;
                if !s.is_empty() {
                    def.model = Some(s.to_string());
                }
            }
            "max_turns" => {
                let n = v
                    .as_int()
                    .ok_or_else(|| invalid("`max_turns` must be an integer"))?;
                if n < 0 {
                    return Err(invalid("`max_turns` must be >= 0"));
                }
                def.max_turns =
                    Some(u32::try_from(n).map_err(|_| invalid("`max_turns` overflow"))?);
            }
            "permission_mode" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`permission_mode` must be a string"))?;
                let m = PermissionMode::from_str_lenient(s)
                    .ok_or_else(|| invalid(&format!("unknown permission_mode {s:?}")))?;
                def.permission_mode = Some(m);
            }
            "tools" => {
                def.tools = Some(parse_tool_rule_list(v).map_err(|e| invalid(&e))?);
            }
            "disallowed_tools" => {
                def.disallowed_tools = parse_tool_rule_list(v).map_err(|e| invalid(&e))?;
            }
            "mcp_servers" => {
                def.mcp_servers = Some(parse_string_list(v).map_err(|e| invalid(&e))?);
            }
            "tool_search" => {
                let map = v
                    .as_map()
                    .ok_or_else(|| invalid("`tool_search` must be a mapping"))?;
                def.tool_search = Some(parse_tool_search(map).map_err(|e| invalid(&e))?);
            }
            "checkpointing" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`checkpointing` must be a string"))?;
                def.checkpointing = Some(
                    SubagentCheckpointPolicy::from_str_lenient(s)
                        .ok_or_else(|| invalid(&format!("unknown checkpoint policy {s:?}")))?,
                );
            }
            "concurrency" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`concurrency` must be a string"))?;
                def.concurrency = match s.to_ascii_lowercase().as_str() {
                    "sequential_only" | "sequential-only" | "sequential" => {
                        AgentConcurrencyHint::SequentialOnly
                    }
                    "parallel_safe" | "parallel-safe" | "parallel" => {
                        AgentConcurrencyHint::ParallelSafe
                    }
                    other => return Err(invalid(&format!("unknown concurrency hint {other:?}"))),
                };
            }
            "output_mode" => {
                let s = v
                    .as_string()
                    .ok_or_else(|| invalid("`output_mode` must be a string"))?;
                def.output_mode = match s.to_ascii_lowercase().as_str() {
                    "text" => AgentOutputMode::Text,
                    "markdown" | "md" => AgentOutputMode::Markdown,
                    other => return Err(invalid(&format!("unknown output_mode {other:?}"))),
                };
            }
            // Tolerated metadata: ignore but keep loading deterministic.
            "version" | "tags" | "category" | "owner" => {}
            other => {
                tracing::debug!(unknown = other, "ignoring unknown agent frontmatter key");
            }
        }
    }
    if def.max_turns.is_none() {
        def.max_turns = Some(DEFAULT_MAX_TURNS);
    }
    Ok(())
}

fn parse_string_list(v: &YamlValue) -> Result<Vec<String>, String> {
    let list = v
        .as_list()
        .ok_or_else(|| "expected a list of strings".to_string())?;
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        match item.as_string() {
            Some(s) => out.push(s.to_string()),
            None => return Err("list items must be strings".to_string()),
        }
    }
    Ok(out)
}

fn parse_tool_rule_list(v: &YamlValue) -> Result<Vec<ToolRule>, String> {
    let list = v
        .as_list()
        .ok_or_else(|| "expected a list of tool names or rules".to_string())?;
    let mut rules = Vec::with_capacity(list.len());
    for item in list {
        match item {
            YamlValue::String(s) => {
                if s.contains('*') || s.contains('?') {
                    rules.push(ToolRule::try_tool_glob(s.clone())?);
                } else {
                    rules.push(ToolRule::ExactTool(s.clone()));
                }
            }
            _ => return Err("tool rule entries must be strings in M6".to_string()),
        }
    }
    Ok(rules)
}

fn parse_tool_search(map: &BTreeMap<String, YamlValue>) -> Result<ToolSearchOptions, String> {
    let mut opts = ToolSearchOptions::default();
    if let Some(v) = map.get("mode") {
        let s = v
            .as_string()
            .ok_or_else(|| "tool_search.mode must be a string".to_string())?;
        opts.mode = crate::tool_catalog::ToolLoadingMode::from_str_lenient(s, 10, 10)
            .ok_or_else(|| format!("unknown tool_search.mode {s:?}"))?;
    }
    if let Some(v) = map.get("strategy") {
        let s = v
            .as_string()
            .ok_or_else(|| "tool_search.strategy must be a string".to_string())?;
        opts.strategy = crate::tool_catalog::ToolSearchStrategy::from_str_lenient(s)
            .ok_or_else(|| format!("unknown tool_search.strategy {s:?}"))?;
    }
    if let Some(v) = map.get("max_results") {
        let n = v
            .as_int()
            .ok_or_else(|| "tool_search.max_results must be an integer".to_string())?;
        opts.max_results =
            usize::try_from(n).map_err(|_| "tool_search.max_results out of range".to_string())?;
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    // A well-formed `.md` definition loads with all fields populated.
    fn loads_well_formed_definition() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join(".agent").join("agents");
        std::fs::create_dir_all(&agents).unwrap();
        let content = "---\n\
name: security-reviewer\n\
description: Reviews Rust code for security issues\n\
tools:\n  - Read\n  - Grep\n\
disallowed_tools:\n  - Edit\n  - Write\n  - Bash\n\
model: claude-sonnet-4-6\n\
max_turns: 8\n\
permission_mode: dontAsk\n\
---\n\
You are a security-focused Rust reviewer.\n";
        std::fs::write(agents.join("security-reviewer.md"), content).unwrap();
        let (defs, errors) = load_filesystem_agents(&agents);
        assert!(errors.is_empty(), "got errors: {errors:?}");
        assert_eq!(defs.len(), 1);
        let d = &defs[0];
        assert_eq!(d.name, "security-reviewer");
        assert_eq!(d.tools.as_ref().unwrap().len(), 2);
        assert_eq!(d.disallowed_tools.len(), 3);
        assert_eq!(d.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(d.max_turns, Some(8));
        assert!(d.prompt.contains("security-focused"));
    }

    #[test]
    // Missing `description` produces a clear error.
    fn missing_description_errors() {
        let dir = TempDir::new().unwrap();
        write_file(&dir, "x.md", "---\nname: x\n---\nhello\n");
        let p = dir.path().join("x.md");
        let err = parse_definition_file(&std::fs::read_to_string(&p).unwrap(), &p)
            .expect_err("must reject missing description");
        assert!(matches!(
            err,
            AgentDefinitionError::MissingDescription { .. }
        ));
    }

    #[test]
    // A file without frontmatter delimiters is rejected.
    fn missing_frontmatter_errors() {
        let dir = TempDir::new().unwrap();
        let p = write_file(&dir, "x.md", "no frontmatter\n");
        let text = std::fs::read_to_string(&p).unwrap();
        let err = parse_definition_file(&text, &p).expect_err("must reject");
        assert!(matches!(
            err,
            AgentDefinitionError::MissingFrontmatter { .. }
        ));
    }

    #[test]
    // Empty body (no prompt after frontmatter) is rejected.
    fn empty_prompt_errors() {
        let dir = TempDir::new().unwrap();
        let p = write_file(&dir, "x.md", "---\nname: x\ndescription: d\n---\n");
        let text = std::fs::read_to_string(&p).unwrap();
        let err = parse_definition_file(&text, &p).expect_err("must reject");
        assert!(matches!(err, AgentDefinitionError::EmptyPrompt { .. }));
    }

    #[test]
    // A definition that lists `Agent` in tools is rejected (no recursion).
    fn recursive_agent_in_file_rejected() {
        let dir = TempDir::new().unwrap();
        let p = write_file(
            &dir,
            "x.md",
            "---\nname: x\ndescription: d\ntools: [Agent, Read]\n---\nbody\n",
        );
        let text = std::fs::read_to_string(&p).unwrap();
        let err = parse_definition_file(&text, &p).expect_err("must reject");
        assert!(matches!(
            err,
            AgentDefinitionError::RecursiveAgentTool { .. }
        ));
    }

    #[test]
    // Unsafe agent names (uppercase, special chars) fail.
    fn unsafe_name_rejected_in_file() {
        let dir = TempDir::new().unwrap();
        let p = write_file(
            &dir,
            "x.md",
            "---\nname: BAD/NAME\ndescription: d\n---\nbody\n",
        );
        let text = std::fs::read_to_string(&p).unwrap();
        let err = parse_definition_file(&text, &p).expect_err("must reject");
        assert!(matches!(err, AgentDefinitionError::UnsafeName { .. }));
    }

    #[test]
    // Files that are not `.md` are skipped silently.
    fn non_md_files_skipped() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().to_path_buf();
        std::fs::write(agents.join("notes.txt"), "ignored").unwrap();
        std::fs::write(
            agents.join("a.md"),
            "---\nname: a\ndescription: d\n---\nb\n",
        )
        .unwrap();
        let (defs, errors) = load_filesystem_agents(&agents);
        assert!(errors.is_empty());
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "a");
    }

    #[test]
    // Missing directory returns an empty result rather than erroring.
    fn missing_dir_is_empty_result() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().join("does-not-exist");
        let (defs, errors) = load_filesystem_agents(&agents);
        assert!(defs.is_empty());
        assert!(errors.is_empty());
    }

    #[test]
    // The strict variant returns the first error.
    fn strict_short_circuits_on_first_error() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().to_path_buf();
        std::fs::write(agents.join("a.md"), "no frontmatter").unwrap();
        let err = load_filesystem_agents_strict(&agents).unwrap_err();
        assert!(matches!(
            err,
            AgentDefinitionError::MissingFrontmatter { .. }
        ));
    }

    #[test]
    // Inline-flow lists in frontmatter are accepted for tools.
    fn inline_flow_tools_list_accepted() {
        let dir = TempDir::new().unwrap();
        let p = write_file(
            &dir,
            "x.md",
            "---\nname: x\ndescription: d\ntools: [Read, Grep, Glob]\n---\nbody\n",
        );
        let text = std::fs::read_to_string(&p).unwrap();
        let def = parse_definition_file(&text, &p).unwrap();
        assert_eq!(def.tools.as_ref().unwrap().len(), 3);
    }

    #[cfg(unix)]
    #[test]
    // A symlink under the agents dir that points outside the dir is rejected.
    fn symlink_escape_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        let outside_file = outside.path().join("evil.md");
        std::fs::write(
            &outside_file,
            "---\nname: evil\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let dir = TempDir::new().unwrap();
        let agents = dir.path().to_path_buf();
        let link = agents.join("link.md");
        symlink(&outside_file, &link).unwrap();
        let (defs, errors) = load_filesystem_agents(&agents);
        assert!(defs.is_empty(), "must not load through escaping symlink");
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], AgentDefinitionError::UnsafePath { .. }));
    }

    #[test]
    // The loader sorts files for deterministic registration order.
    fn loader_orders_files_deterministically() {
        let dir = TempDir::new().unwrap();
        let agents = dir.path().to_path_buf();
        std::fs::write(
            agents.join("zulu.md"),
            "---\nname: zulu\ndescription: d\n---\nb\n",
        )
        .unwrap();
        std::fs::write(
            agents.join("alpha.md"),
            "---\nname: alpha\ndescription: d\n---\nb\n",
        )
        .unwrap();
        let (defs, errors) = load_filesystem_agents(&agents);
        assert!(errors.is_empty());
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
    }

    #[test]
    // If `name` is omitted the file stem is used.
    fn name_falls_back_to_file_stem() {
        let dir = TempDir::new().unwrap();
        let p = write_file(&dir, "test-analyst.md", "---\ndescription: d\n---\nbody\n");
        let text = std::fs::read_to_string(&p).unwrap();
        let def = parse_definition_file(&text, &p).unwrap();
        assert_eq!(def.name, "test-analyst");
    }
}
