//! Loader for `.agent/skills/<name>/SKILL.md` definitions.
//!
//! Each Skill is a *directory* containing a `SKILL.md` file with YAML
//! frontmatter and a markdown body. Optional resources live alongside the
//! `SKILL.md`. The loader does not execute or evaluate anything in the
//! Skill — it parses metadata, captures the body as instructions, and
//! produces a `SkillDefinition`. It refuses to follow symlinks that escape
//! the skills root and rejects names that would be unsafe as filesystem
//! components or CLI tokens.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agents::yaml::{parse_frontmatter, YamlValue};
use crate::tool_rule::ToolRule;

use super::definition::{
    is_identifier_safe_skill_name, SkillDefinition, SkillSource, MAX_SKILL_NAME_LEN,
};
use super::errors::SkillError;

/// Default maximum size of a SKILL.md.
pub const DEFAULT_MAX_SKILL_MD_BYTES: usize = 64 * 1024;

/// Convenience: the canonical project-local skills directory.
pub fn filesystem_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".agent").join("skills")
}

/// Compatibility skills directory.
pub fn compat_claude_skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".claude").join("skills")
}

/// Load every Skill defined under `dir`. Skips entries that aren't
/// directories containing a `SKILL.md`. Errors are collected and returned
/// as the second tuple element so partial successes can still be shown.
pub fn load_filesystem_skills(
    dir: &Path,
    source: SkillSource,
    max_skill_md_bytes: usize,
) -> (Vec<SkillDefinition>, Vec<SkillError>) {
    let mut defs = Vec::new();
    let mut errors = Vec::new();
    if !dir.exists() {
        return (defs, errors);
    }
    let canonical_root = match dir.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            errors.push(SkillError::Io {
                path: dir.to_path_buf(),
                source: e,
            });
            return (defs, errors);
        }
    };
    let entries = match std::fs::read_dir(&canonical_root) {
        Ok(rd) => rd,
        Err(e) => {
            errors.push(SkillError::Io {
                path: canonical_root,
                source: e,
            });
            return (defs, errors);
        }
    };
    let mut child_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            child_dirs.push(p);
        }
    }
    child_dirs.sort();
    for child in child_dirs {
        match load_one_skill(&canonical_root, &child, source, max_skill_md_bytes) {
            Ok(def) => defs.push(def),
            Err(e) => errors.push(e),
        }
    }
    (defs, errors)
}

fn load_one_skill(
    root: &Path,
    skill_dir: &Path,
    source: SkillSource,
    max_skill_md_bytes: usize,
) -> Result<SkillDefinition, SkillError> {
    // Symlink-escape protection: canonicalize both root and skill_dir.
    let canonical_root = root.canonicalize().map_err(|e| SkillError::Io {
        path: root.to_path_buf(),
        source: e,
    })?;
    let canonical_skill_dir = skill_dir
        .canonicalize()
        .map_err(|_| SkillError::UnsafePath {
            path: skill_dir.to_path_buf(),
            root: root.to_path_buf(),
        })?;
    if !canonical_skill_dir.starts_with(&canonical_root) {
        return Err(SkillError::UnsafePath {
            path: skill_dir.to_path_buf(),
            root: root.to_path_buf(),
        });
    }
    let skill_md = canonical_skill_dir.join("SKILL.md");
    if !skill_md.exists() {
        return Err(SkillError::Other(format!(
            "directory {} does not contain SKILL.md",
            canonical_skill_dir.display()
        )));
    }
    let canonical_skill_md = skill_md
        .canonicalize()
        .map_err(|_| SkillError::UnsafePath {
            path: skill_md.clone(),
            root: canonical_skill_dir.clone(),
        })?;
    if !canonical_skill_md.starts_with(&canonical_skill_dir) {
        return Err(SkillError::UnsafePath {
            path: skill_md,
            root: canonical_skill_dir,
        });
    }
    let bytes = std::fs::read(&canonical_skill_md).map_err(|source| SkillError::Io {
        path: canonical_skill_md.clone(),
        source,
    })?;
    let dir_name = canonical_skill_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    if bytes.len() > max_skill_md_bytes {
        return Err(SkillError::SkillMdTooLarge {
            name: dir_name,
            max: max_skill_md_bytes,
        });
    }
    let text = String::from_utf8(bytes).map_err(|e| SkillError::InvalidField {
        path: canonical_skill_md.clone(),
        message: format!("file is not valid UTF-8: {e}"),
    })?;
    parse_skill_md(&text, &canonical_skill_md, &canonical_skill_dir, source)
}

/// Parse a Skill markdown source. Public so tests and SDK callers who
/// already hold the bytes can avoid re-reading from disk.
pub fn parse_skill_md(
    text: &str,
    skill_md_path: &Path,
    skill_dir: &Path,
    source: SkillSource,
) -> Result<SkillDefinition, SkillError> {
    let (frontmatter_body, prompt_body) = split_frontmatter(text, skill_md_path)?;
    let map = parse_frontmatter(frontmatter_body).map_err(|e| SkillError::InvalidFrontmatter {
        path: skill_md_path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut def = SkillDefinition {
        name: String::new(),
        description: String::new(),
        instructions: prompt_body.trim().to_string(),
        argument_hint: None,
        allowed_tools: Vec::new(),
        disallowed_tools: Vec::new(),
        resources: Vec::new(),
        metadata: serde_json::Value::Null,
        source,
        root_dir: skill_dir.to_path_buf(),
    };
    apply_frontmatter(&mut def, &map, skill_md_path)?;

    if def.name.is_empty() {
        if let Some(dir_name) = skill_dir.file_name().and_then(|s| s.to_str()) {
            def.name = dir_name.to_string();
        }
    }
    if def.name.len() > MAX_SKILL_NAME_LEN {
        return Err(SkillError::UnsafeName { name: def.name });
    }
    if !is_identifier_safe_skill_name(&def.name) {
        return Err(SkillError::UnsafeName { name: def.name });
    }
    if def.instructions.is_empty() {
        return Err(SkillError::MissingBody {
            name: def.name.clone(),
        });
    }
    def.validate_structural()?;
    Ok(def)
}

fn split_frontmatter<'a>(text: &'a str, path: &Path) -> Result<(&'a str, &'a str), SkillError> {
    let trimmed_start = text.trim_start_matches('\u{FEFF}');
    if !trimmed_start.starts_with("---") {
        return Err(SkillError::MissingFrontmatter {
            path: path.to_path_buf(),
        });
    }
    let after_open = trimmed_start.strip_prefix("---").unwrap_or(trimmed_start);
    let after_open = match after_open.strip_prefix('\n') {
        Some(s) => s,
        None => match after_open.strip_prefix("\r\n") {
            Some(s) => s,
            None => {
                return Err(SkillError::MissingFrontmatter {
                    path: path.to_path_buf(),
                });
            }
        },
    };
    let mut search_from = 0usize;
    let close_offset = loop {
        let needle = "\n---";
        let Some(rel) = after_open[search_from..].find(needle) else {
            return Err(SkillError::MissingFrontmatter {
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
    def: &mut SkillDefinition,
    map: &BTreeMap<String, YamlValue>,
    path: &Path,
) -> Result<(), SkillError> {
    let invalid = |message: &str| SkillError::InvalidField {
        path: path.to_path_buf(),
        message: message.to_string(),
    };
    let mut metadata = serde_json::Map::new();
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
            "resources" => {
                def.resources = parse_string_list(v).map_err(|e| invalid(&e))?;
            }
            // Metadata we don't model but tolerate. Unknown keys are also
            // tolerated below and surfaced through `metadata`.
            other => {
                let json = yaml_to_json(v);
                metadata.insert(other.to_string(), json);
                tracing::debug!(unknown = other, "preserving unknown skill frontmatter key");
            }
        }
    }
    if !metadata.is_empty() {
        def.metadata = serde_json::Value::Object(metadata);
    }
    Ok(())
}

fn parse_string_list(v: &YamlValue) -> Result<Vec<String>, String> {
    let list = v.as_list().ok_or_else(|| "expected a list".to_string())?;
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
            YamlValue::String(s) => rules.push(parse_tool_rule_string(s)?),
            _ => return Err("tool rule entries must be strings".to_string()),
        }
    }
    Ok(rules)
}

/// Parse a single tool rule from a string. Recognizes:
/// - `Tool` -> ExactTool
/// - `Tool*` or other globs -> ToolGlob
/// - `Bash(pattern)` -> BashCommandRegex(pattern)
/// - `Edit(path-glob)` / `Read(path-glob)` etc -> FilePathGlob
fn parse_tool_rule_string(s: &str) -> Result<ToolRule, String> {
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
            // Treat as a literal-prefix command rule. The trailing `*` is
            // treated as a wildcard for clarity; we convert to a regex.
            let regex = bash_pattern_to_regex(inner);
            return Ok(ToolRule::BashCommandRegex(regex));
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
    // Convert a shell-glob-style pattern with `*` wildcards into a regex
    // anchored at the start. Trailing `*` becomes `.*`. Other shell metas
    // are escaped. This is intentionally conservative: anything more
    // complex should be expressed as a real regex with explicit anchors.
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

fn yaml_to_json(v: &YamlValue) -> serde_json::Value {
    match v {
        YamlValue::String(s) => serde_json::Value::String(s.clone()),
        YamlValue::Bool(b) => serde_json::Value::Bool(*b),
        YamlValue::Int(i) => serde_json::Value::Number((*i).into()),
        YamlValue::List(items) => {
            serde_json::Value::Array(items.iter().map(yaml_to_json).collect())
        }
        YamlValue::Map(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                out.insert(k.clone(), yaml_to_json(v));
            }
            serde_json::Value::Object(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &TempDir, name: &str, body: &str) -> PathBuf {
        let skill_dir = dir.path().join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        let p = skill_dir.join("SKILL.md");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    // A well-formed SKILL.md loads with all fields populated.
    fn loads_well_formed_skill() {
        let dir = TempDir::new().unwrap();
        write_skill(
            &dir,
            "release-pr",
            "---\nname: release-pr\ndescription: Create a release PR summary\nargument-hint: <version>\nallowed-tools:\n  - Read\n  - Bash(git log *)\nresources:\n  - templates/pr-summary.md\n---\nYou are helping create a release PR.\n",
        );
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(errors.is_empty(), "got errors: {errors:?}");
        assert_eq!(defs.len(), 1);
        let d = &defs[0];
        assert_eq!(d.name, "release-pr");
        assert_eq!(d.argument_hint.as_deref(), Some("<version>"));
        assert_eq!(d.allowed_tools.len(), 2);
        assert_eq!(d.resources.len(), 1);
        assert!(d.instructions.contains("release PR"));
    }

    #[test]
    // Missing SKILL.md is reported with a clear error.
    fn missing_skill_md_errors() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("empty-skill")).unwrap();
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(defs.is_empty());
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            SkillError::Other(msg) => assert!(msg.contains("SKILL.md")),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    // Body must not be empty.
    fn empty_body_rejected() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "empty", "---\nname: empty\ndescription: d\n---\n");
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(defs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], SkillError::MissingBody { .. }));
    }

    #[test]
    // Missing frontmatter is rejected.
    fn missing_frontmatter_rejected() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "bad", "no frontmatter\n");
        let (_defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(matches!(errors[0], SkillError::MissingFrontmatter { .. }));
    }

    #[test]
    // Skill name falls back to the directory name when frontmatter omits it.
    fn name_falls_back_to_dir() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "from-dir", "---\ndescription: d\n---\nbody\n");
        let (defs, _) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "from-dir");
    }

    #[test]
    // Skills are returned in deterministic (sorted) order.
    fn loader_sorts_skills() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "zulu", "---\ndescription: d\n---\nb\n");
        write_skill(&dir, "alpha", "---\ndescription: d\n---\nb\n");
        let (defs, _) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zulu"]);
    }

    #[test]
    // Files larger than the cap are rejected.
    fn oversized_skill_md_rejected() {
        let dir = TempDir::new().unwrap();
        let big_body = format!("---\ndescription: d\n---\n{}\n", "x".repeat(2000));
        write_skill(&dir, "big", &big_body);
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 200);
        assert!(defs.is_empty());
        assert!(matches!(errors[0], SkillError::SkillMdTooLarge { .. }));
    }

    #[test]
    // Unsafe name (uppercase) is rejected.
    fn unsafe_name_rejected() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "BAD", "---\ndescription: d\n---\nbody\n");
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(defs.is_empty());
        assert!(matches!(errors[0], SkillError::UnsafeName { .. }));
    }

    #[cfg(unix)]
    #[test]
    // A symlink that points outside the skills root is rejected.
    fn symlink_escape_rejected() {
        use std::os::unix::fs::symlink;
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("evil")).unwrap();
        std::fs::write(
            outside.path().join("evil").join("SKILL.md"),
            "---\nname: evil\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        let dir = TempDir::new().unwrap();
        symlink(outside.path().join("evil"), dir.path().join("link")).unwrap();
        let (defs, errors) = load_filesystem_skills(dir.path(), SkillSource::Project, 65536);
        assert!(defs.is_empty());
        assert_eq!(errors.len(), 1);
        assert!(matches!(errors[0], SkillError::UnsafePath { .. }));
    }

    #[test]
    // Bash(pattern) tool rule is parsed as a BashCommandRegex.
    fn bash_pattern_parses_as_regex() {
        let r = parse_tool_rule_string("Bash(git diff *)").unwrap();
        match r {
            ToolRule::BashCommandRegex(rx) => assert!(rx.starts_with("^git diff")),
            other => panic!("expected BashCommandRegex, got {other:?}"),
        }
    }

    #[test]
    // Edit(path-glob) tool rule parses as a FilePathGlob.
    fn edit_pattern_parses_as_filepath_glob() {
        let r = parse_tool_rule_string("Edit(**/*.md)").unwrap();
        match r {
            ToolRule::FilePathGlob { tool, pattern } => {
                assert_eq!(tool, "Edit");
                assert_eq!(pattern, "**/*.md");
            }
            other => panic!("expected FilePathGlob, got {other:?}"),
        }
    }
}
