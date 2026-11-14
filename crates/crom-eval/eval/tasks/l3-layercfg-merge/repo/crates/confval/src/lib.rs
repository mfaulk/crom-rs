//! Configuration values and the file format.

use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    Table(BTreeMap<String, Value>),
}

impl Value {
    pub fn table() -> Value {
        Value::Table(BTreeMap::new())
    }

    /// Interpret a raw scalar: `true`/`false`, a decimal integer, a
    /// double-quoted string, or otherwise a bare string.
    pub fn parse_scalar(raw: &str) -> Value {
        let raw = raw.trim();
        if raw == "true" {
            Value::Bool(true)
        } else if raw == "false" {
            Value::Bool(false)
        } else if let Ok(i) = raw.parse::<i64>() {
            Value::Int(i)
        } else if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
            Value::Str(raw[1..raw.len() - 1].to_string())
        } else {
            Value::Str(raw.to_string())
        }
    }

    /// Look up a dotted path such as `db.pool.max`.
    pub fn get(&self, path: &str) -> Option<&Value> {
        let mut cur = self;
        for part in path.split('.') {
            match cur {
                Value::Table(t) => cur = t.get(part)?,
                _ => return None,
            }
        }
        Some(cur)
    }

    /// Set a dotted path, creating intermediate tables. Fails if a non-table
    /// value is in the way.
    pub fn set(&mut self, path: &str, value: Value) -> Result<(), ConfigError> {
        let parts: Vec<&str> = path.split('.').collect();
        let mut cur = self;
        for (i, part) in parts.iter().enumerate() {
            let Value::Table(t) = cur else {
                return Err(ConfigError::NotATable(parts[..i].join(".")));
            };
            if i == parts.len() - 1 {
                t.insert(part.to_string(), value);
                return Ok(());
            }
            cur = t.entry(part.to_string()).or_insert_with(Value::table);
        }
        unreachable!("path has at least one part")
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Syntax { line: usize, text: String },
    NotATable(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Syntax { line, text } => write!(f, "line {line}: cannot parse `{text}`"),
            ConfigError::NotATable(p) => write!(f, "`{p}` is not a table"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Parse the INI-style file format into a table.
pub fn parse(src: &str) -> Result<Value, ConfigError> {
    let mut root = Value::table();
    let mut section = String::new();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            if section.is_empty() {
                return Err(ConfigError::Syntax {
                    line: i + 1,
                    text: line.to_string(),
                });
            }
            continue;
        }
        let (k, v) = line.split_once('=').ok_or_else(|| ConfigError::Syntax {
            line: i + 1,
            text: line.to_string(),
        })?;
        let key = k.trim();
        if key.is_empty() {
            return Err(ConfigError::Syntax {
                line: i + 1,
                text: line.to_string(),
            });
        }
        let path = if section.is_empty() {
            key.to_string()
        } else {
            format!("{section}.{key}")
        };
        root.set(&path, Value::parse_scalar(v))?;
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections_and_scalars() {
        let v =
            parse("name = demo\n[server]\nport = 8080\ntls = false\n[db.pool]\nmax = 4\n").unwrap();
        assert_eq!(v.get("name"), Some(&Value::Str("demo".into())));
        assert_eq!(v.get("server.port").and_then(Value::as_int), Some(8080));
        assert_eq!(v.get("server.tls").and_then(Value::as_bool), Some(false));
        assert_eq!(v.get("db.pool.max").and_then(Value::as_int), Some(4));
    }

    #[test]
    fn quoted_strings_stay_strings() {
        let v = parse("[a]\nzip = \"02134\"\n").unwrap();
        assert_eq!(v.get("a.zip"), Some(&Value::Str("02134".into())));
    }

    #[test]
    fn syntax_errors() {
        assert!(matches!(
            parse("[]\n"),
            Err(ConfigError::Syntax { line: 1, .. })
        ));
        assert!(matches!(
            parse("x\n"),
            Err(ConfigError::Syntax { line: 1, .. })
        ));
    }
}
