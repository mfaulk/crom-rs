//! Tiny YAML-subset parser for agent-definition frontmatter.
//!
//! Subset:
//! - Top-level `key: value` lines (scalar values).
//! - Top-level `key:` followed by indented `- item` lines for list-of-strings.
//! - Top-level `key:` followed by indented `subkey: value` lines for a flat
//!   map of strings/bools/ints.
//! - `# ...` line comments (must be the first non-whitespace on the line, or
//!   appear at end-of-line after a value/`#`-not-quoted).
//! - Inline flow-list values: `key: [a, b, c]`.
//! - Optional double-quoted strings on the right-hand side.
//!
//! What we deliberately do NOT support: nested maps deeper than one level,
//! anchors, multi-line scalars, complex flow-mapping, or YAML's many
//! tagging/typing edge cases. Anything we don't recognize produces an error
//! (with the offending line number) so misformatted definitions fail loudly.

use std::collections::BTreeMap;

/// A single parsed YAML value from frontmatter.
#[derive(Debug, Clone, PartialEq)]
pub enum YamlValue {
    String(String),
    Bool(bool),
    Int(i64),
    List(Vec<YamlValue>),
    Map(BTreeMap<String, YamlValue>),
}

impl YamlValue {
    pub fn as_string(&self) -> Option<&str> {
        if let YamlValue::String(s) = self {
            Some(s.as_str())
        } else {
            None
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        if let YamlValue::Bool(b) = self {
            Some(*b)
        } else {
            None
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        if let YamlValue::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }

    pub fn as_list(&self) -> Option<&[YamlValue]> {
        if let YamlValue::List(l) = self {
            Some(l)
        } else {
            None
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, YamlValue>> {
        if let YamlValue::Map(m) = self {
            Some(m)
        } else {
            None
        }
    }
}

/// Parse a frontmatter body (the text between the opening and closing `---`
/// fences). Lines are 1-based for error reporting.
pub fn parse_frontmatter(body: &str) -> Result<BTreeMap<String, YamlValue>, YamlError> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out: BTreeMap<String, YamlValue> = BTreeMap::new();
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let lineno = i + 1;
        let owned = strip_comment(raw);
        let trimmed = owned.trim_end();
        if trimmed.trim().is_empty() {
            i += 1;
            continue;
        }
        if leading_indent(raw) != 0 {
            return Err(YamlError::new(
                lineno,
                "unexpected indentation at top level",
            ));
        }
        let (key, rhs) = split_key_value(trimmed, lineno)?;
        if rhs.is_empty() {
            // Block list or block map? Peek the next non-blank line.
            let (block_value, consumed) = parse_block(&lines, i + 1, lineno)?;
            out.insert(key, block_value);
            i += 1 + consumed;
        } else {
            out.insert(key, parse_scalar_or_flow(rhs, lineno)?);
            i += 1;
        }
    }
    Ok(out)
}

/// Parse the indented block following a `key:` line. The block ends at the
/// first line whose indentation is zero (or end of input). Returns the
/// parsed value and the number of lines consumed.
fn parse_block(
    lines: &[&str],
    start: usize,
    key_line: usize,
) -> Result<(YamlValue, usize), YamlError> {
    let mut i = start;
    // Skip leading blanks.
    while i < lines.len() && strip_comment(lines[i]).trim().is_empty() {
        i += 1;
    }
    if i >= lines.len() {
        // The key had no body — treat as empty string (e.g. `notes:` alone).
        return Ok((YamlValue::String(String::new()), i - start));
    }
    let indent = leading_indent(lines[i]);
    if indent == 0 {
        return Ok((YamlValue::String(String::new()), i - start));
    }
    let first_owned = strip_comment(lines[i]);
    let first_trim = first_owned.trim();
    if first_trim.starts_with("- ") || first_trim == "-" {
        // List block.
        let mut items: Vec<YamlValue> = Vec::new();
        while i < lines.len() {
            let raw = lines[i];
            let trimmed = strip_comment(raw);
            if trimmed.trim().is_empty() {
                i += 1;
                continue;
            }
            let row_indent = leading_indent(raw);
            if row_indent == 0 {
                break;
            }
            if row_indent != indent {
                return Err(YamlError::new(
                    i + 1,
                    "inconsistent indentation in list block",
                ));
            }
            let item = trimmed.trim();
            if let Some(rest) = item.strip_prefix("- ") {
                items.push(parse_scalar_or_flow(rest.trim(), i + 1)?);
            } else if item == "-" {
                items.push(YamlValue::String(String::new()));
            } else {
                return Err(YamlError::new(
                    i + 1,
                    "expected `- value` list item or end of block",
                ));
            }
            i += 1;
        }
        return Ok((YamlValue::List(items), i - start));
    }
    // Map block.
    let mut map: BTreeMap<String, YamlValue> = BTreeMap::new();
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = strip_comment(raw);
        if trimmed.trim().is_empty() {
            i += 1;
            continue;
        }
        let row_indent = leading_indent(raw);
        if row_indent == 0 {
            break;
        }
        if row_indent != indent {
            return Err(YamlError::new(
                i + 1,
                "inconsistent indentation in mapping block",
            ));
        }
        let body = trimmed.trim();
        let (k, v) = split_key_value(body, i + 1)?;
        if v.is_empty() {
            return Err(YamlError::new(
                i + 1,
                "nested-block mappings deeper than one level are not supported in agent \
                 frontmatter",
            ));
        }
        map.insert(k, parse_scalar_or_flow(v, i + 1)?);
        i += 1;
    }
    if map.is_empty() && i == start {
        // Should be unreachable, but be explicit.
        return Err(YamlError::new(key_line, "expected list or map block"));
    }
    Ok((YamlValue::Map(map), i - start))
}

fn split_key_value(line: &str, lineno: usize) -> Result<(String, &str), YamlError> {
    let line = line.trim();
    let colon = line
        .find(':')
        .ok_or_else(|| YamlError::new(lineno, "expected `key: value`"))?;
    let key_raw = line[..colon].trim();
    if key_raw.is_empty() {
        return Err(YamlError::new(lineno, "empty key"));
    }
    if !is_valid_key(key_raw) {
        return Err(YamlError::new(
            lineno,
            "key must match [A-Za-z_][A-Za-z0-9_-]*",
        ));
    }
    let rhs = line[colon + 1..].trim();
    // Need an owned copy for the key but the rhs is a &str borrowing from
    // `line`.
    Ok((key_raw.to_string(), rhs))
}

fn parse_scalar_or_flow(value: &str, lineno: usize) -> Result<YamlValue, YamlError> {
    let v = value.trim();
    if v.is_empty() {
        return Ok(YamlValue::String(String::new()));
    }
    // Inline flow list: `[a, b, "c"]`.
    if v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        let mut items = Vec::new();
        for piece in split_flow_items(inner) {
            let p = piece.trim();
            if p.is_empty() {
                continue;
            }
            items.push(parse_simple_scalar(p, lineno)?);
        }
        return Ok(YamlValue::List(items));
    }
    parse_simple_scalar(v, lineno)
}

fn parse_simple_scalar(v: &str, _lineno: usize) -> Result<YamlValue, YamlError> {
    if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
        || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2)
    {
        let inner = &v[1..v.len() - 1];
        // Unescape only `\"`, `\\`, and `\n` for double-quoted strings; single-
        // quoted strings keep contents verbatim.
        if v.starts_with('"') {
            let mut out = String::with_capacity(inner.len());
            let mut chars = inner.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    match chars.next() {
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some(other) => {
                            out.push('\\');
                            out.push(other);
                        }
                        None => out.push('\\'),
                    }
                } else {
                    out.push(c);
                }
            }
            return Ok(YamlValue::String(out));
        }
        return Ok(YamlValue::String(inner.to_string()));
    }
    match v {
        "true" | "True" | "TRUE" => return Ok(YamlValue::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(YamlValue::Bool(false)),
        "null" | "Null" | "NULL" | "~" => return Ok(YamlValue::String(String::new())),
        _ => {}
    }
    if let Ok(n) = v.parse::<i64>() {
        return Ok(YamlValue::Int(n));
    }
    Ok(YamlValue::String(v.to_string()))
}

fn split_flow_items(inner: &str) -> Vec<String> {
    let mut items: Vec<String> = Vec::new();
    let mut depth = 0i32;
    let mut in_dq = false;
    let mut in_sq = false;
    let mut current = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_sq => {
                in_dq = !in_dq;
                current.push(c);
            }
            '\'' if !in_dq => {
                in_sq = !in_sq;
                current.push(c);
            }
            '[' if !in_dq && !in_sq => {
                depth += 1;
                current.push(c);
            }
            ']' if !in_dq && !in_sq => {
                depth -= 1;
                current.push(c);
            }
            ',' if !in_dq && !in_sq && depth == 0 => {
                items.push(std::mem::take(&mut current));
            }
            '\\' if in_dq => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        items.push(current);
    }
    items
}

/// Strip a `#` line comment, preserving `#` characters inside quoted
/// strings. Honors `\"` (and `\\`) escapes inside double-quoted strings so
/// a description like `"foo \" # bar"` keeps the literal `#` instead of
/// being prematurely truncated mid-string.
fn strip_comment(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_dq = false;
    let mut in_sq = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if in_dq => {
                // Inside a double-quoted string, copy the backslash and
                // the next char verbatim so an escaped quote does not
                // toggle `in_dq`.
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '"' if !in_sq => {
                in_dq = !in_dq;
                out.push(c);
            }
            '\'' if !in_dq => {
                in_sq = !in_sq;
                out.push(c);
            }
            '#' if !in_dq && !in_sq => break,
            _ => out.push(c),
        }
    }
    out
}

fn leading_indent(line: &str) -> usize {
    line.chars().take_while(|c| *c == ' ').count()
}

fn is_valid_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[derive(Debug)]
pub struct YamlError {
    pub line: usize,
    pub message: String,
}

impl YamlError {
    fn new(line: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for YamlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for YamlError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Top-level scalars parse as the expected primitive types.
    fn parses_top_level_scalars() {
        let body = "name: alpha\ncount: 7\nflag: true\n";
        let m = parse_frontmatter(body).unwrap();
        assert_eq!(m.get("name").unwrap().as_string(), Some("alpha"));
        assert_eq!(m.get("count").unwrap().as_int(), Some(7));
        assert_eq!(m.get("flag").unwrap().as_bool(), Some(true));
    }

    #[test]
    // Indented `- item` lines parse as a list-of-strings.
    fn parses_block_list() {
        let body = "tools:\n  - Read\n  - Grep\n  - Glob\n";
        let m = parse_frontmatter(body).unwrap();
        let list = m.get("tools").unwrap().as_list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].as_string(), Some("Read"));
    }

    #[test]
    // Inline `[a, b, c]` flow-list parses as a list-of-strings.
    fn parses_inline_flow_list() {
        let body = "tools: [Read, \"Grep\", Glob]\n";
        let m = parse_frontmatter(body).unwrap();
        let list = m.get("tools").unwrap().as_list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[1].as_string(), Some("Grep"));
    }

    #[test]
    // Indented `subkey: value` lines parse as a flat map.
    fn parses_block_map() {
        let body = "tool_search:\n  mode: search\n  max_results: 5\n";
        let m = parse_frontmatter(body).unwrap();
        let inner = m.get("tool_search").unwrap().as_map().unwrap();
        assert_eq!(inner.get("mode").unwrap().as_string(), Some("search"));
        assert_eq!(inner.get("max_results").unwrap().as_int(), Some(5));
    }

    #[test]
    // Quoted strings preserve embedded special characters like `:` and `#`.
    fn quoted_strings_preserve_specials() {
        let body = "description: \"a: tricky # hash\"\n";
        let m = parse_frontmatter(body).unwrap();
        assert_eq!(
            m.get("description").unwrap().as_string(),
            Some("a: tricky # hash")
        );
    }

    #[test]
    // Trailing `# comment` text is stripped from unquoted values.
    fn line_comment_stripped() {
        let body = "name: alpha # this is a comment\n";
        let m = parse_frontmatter(body).unwrap();
        assert_eq!(m.get("name").unwrap().as_string(), Some("alpha"));
    }

    #[test]
    // Missing colon on a top-level line fails with a line number.
    fn missing_colon_errors() {
        let err = parse_frontmatter("name alpha\n").unwrap_err();
        assert_eq!(err.line, 1);
    }

    #[test]
    // Two-deep nesting (a block map inside a block map) is rejected.
    fn rejects_two_level_nesting() {
        let body = "outer:\n  inner:\n    leaf: 1\n";
        let err = parse_frontmatter(body).unwrap_err();
        assert!(err.message.contains("nested-block"));
    }

    #[test]
    // Empty body produces an empty map.
    fn empty_body_is_empty_map() {
        assert!(parse_frontmatter("").unwrap().is_empty());
        assert!(parse_frontmatter("\n\n").unwrap().is_empty());
    }
}
