use globset::{Glob, GlobMatcher};
use regex::Regex;
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

/// Structured tool-rule used by the policy engine to allow or deny tool use.
#[derive(Debug, Clone)]
pub enum ToolRule {
    /// Match a single tool by exact name.
    ExactTool(String),
    /// Match tools whose names match a glob (e.g. `*` or `Edit*`).
    ToolGlob(String),
    /// Match Bash invocations whose `command` matches a regex.
    BashCommandRegex(String),
    /// Match a tool whose input contains a `file_path` matching the path glob.
    FilePathGlob { tool: String, pattern: String },
}

impl ToolRule {
    /// Returns true if this rule matches the requested tool name and input.
    pub fn matches(&self, tool_name: &str, input: &Value) -> bool {
        match self {
            ToolRule::ExactTool(n) => n == tool_name,
            ToolRule::ToolGlob(g) => glob_match(g, tool_name),
            ToolRule::BashCommandRegex(re) => {
                if tool_name != "Bash" {
                    return false;
                }
                let cmd = input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                Regex::new(re).map(|r| r.is_match(cmd)).unwrap_or(false)
            }
            ToolRule::FilePathGlob { tool, pattern } => {
                if tool != tool_name {
                    return false;
                }
                let path = input
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                glob_match(pattern, path)
            }
        }
    }
}

fn glob_match(pattern: &str, candidate: &str) -> bool {
    match Glob::new(pattern) {
        Ok(g) => {
            let m: GlobMatcher = g.compile_matcher();
            m.is_match(candidate)
        }
        Err(_) => false,
    }
}

impl Serialize for ToolRule {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;
        match self {
            ToolRule::ExactTool(n) => serializer.serialize_str(n),
            ToolRule::ToolGlob(g) => serializer.serialize_str(g),
            ToolRule::BashCommandRegex(re) => {
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("tool", "Bash")?;
                m.serialize_entry("command_regex", re)?;
                m.end()
            }
            ToolRule::FilePathGlob { tool, pattern } => {
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("tool", tool)?;
                m.serialize_entry("path_glob", pattern)?;
                m.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for ToolRule {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;

        impl<'de> Visitor<'de> for V {
            type Value = ToolRule;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a tool rule string or object")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.contains('*') || value.contains('?') {
                    Ok(ToolRule::ToolGlob(value.to_string()))
                } else {
                    Ok(ToolRule::ExactTool(value.to_string()))
                }
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
                self.visit_str(&value)
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut tool: Option<String> = None;
                let mut command_regex: Option<String> = None;
                let mut path_glob: Option<String> = None;

                // Track unknown keys so a typo like `commandRegex` fails fast
                // instead of silently degrading to an unconditional
                // `ExactTool(tool)` rule, which would widen Bash permissions
                // unintentionally.
                let mut unknown_keys: Vec<String> = Vec::new();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "tool" => tool = Some(map.next_value::<String>()?),
                        "command_regex" => command_regex = Some(map.next_value::<String>()?),
                        "path_glob" => path_glob = Some(map.next_value::<String>()?),
                        other => {
                            unknown_keys.push(other.to_string());
                            let _: serde_json::Value = map.next_value()?;
                        }
                    }
                }
                if !unknown_keys.is_empty() {
                    return Err(de::Error::custom(format!(
                        "tool rule object has unknown key(s): {} (allowed: tool, command_regex, path_glob)",
                        unknown_keys.join(", ")
                    )));
                }
                let tool =
                    tool.ok_or_else(|| de::Error::custom("tool rule object missing `tool`"))?;
                if let Some(re) = command_regex {
                    Ok(ToolRule::BashCommandRegex(re))
                } else if let Some(p) = path_glob {
                    Ok(ToolRule::FilePathGlob { tool, pattern: p })
                } else {
                    Ok(ToolRule::ExactTool(tool))
                }
            }
        }

        deserializer.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Parses simple string into ExactTool when no wildcards present.
    fn parse_exact() {
        let v: ToolRule = serde_json::from_str("\"Read\"").unwrap();
        assert!(matches!(v, ToolRule::ExactTool(ref n) if n == "Read"));
    }

    #[test]
    // Parses string with glob wildcards as ToolGlob.
    fn parse_glob() {
        let v: ToolRule = serde_json::from_str("\"Edit*\"").unwrap();
        assert!(matches!(v, ToolRule::ToolGlob(ref g) if g == "Edit*"));
    }

    #[test]
    // Parses object with command_regex as BashCommandRegex.
    fn parse_bash_command_regex() {
        let v: ToolRule = serde_json::from_str(
            "{\"tool\": \"Bash\", \"command_regex\": \"^cargo (test|check)\"}",
        )
        .unwrap();
        assert!(matches!(v, ToolRule::BashCommandRegex(_)));
    }

    #[test]
    // Parses object with path_glob as FilePathGlob.
    fn parse_path_glob() {
        let v: ToolRule =
            serde_json::from_str("{\"tool\": \"Edit\", \"path_glob\": \"**/.env*\"}").unwrap();
        assert!(matches!(v, ToolRule::FilePathGlob { .. }));
    }

    #[test]
    // ExactTool rule matches its tool name.
    fn matches_exact_tool() {
        let r = ToolRule::ExactTool("Read".into());
        assert!(r.matches("Read", &serde_json::json!({})));
        assert!(!r.matches("Write", &serde_json::json!({})));
    }

    #[test]
    // ToolGlob with `*` matches any tool name.
    fn matches_tool_glob_star() {
        let r = ToolRule::ToolGlob("*".into());
        assert!(r.matches("Bash", &serde_json::json!({})));
        assert!(r.matches("Read", &serde_json::json!({})));
    }

    #[test]
    // BashCommandRegex matches Bash command strings.
    fn matches_bash_command_regex() {
        let r = ToolRule::BashCommandRegex("^cargo (test|check)".into());
        assert!(r.matches("Bash", &serde_json::json!({"command": "cargo test"})));
        assert!(!r.matches("Bash", &serde_json::json!({"command": "rm -rf /"})));
        assert!(!r.matches("Edit", &serde_json::json!({"command": "cargo test"})));
    }

    #[test]
    // FilePathGlob matches a tool's file_path against the pattern.
    fn matches_file_path_glob() {
        let r = ToolRule::FilePathGlob {
            tool: "Edit".into(),
            pattern: "**/.env*".into(),
        };
        assert!(r.matches("Edit", &serde_json::json!({"file_path": ".env"})));
        assert!(r.matches("Edit", &serde_json::json!({"file_path": "src/.env.local"})));
        assert!(!r.matches("Edit", &serde_json::json!({"file_path": "src/lib.rs"})));
        assert!(!r.matches("Write", &serde_json::json!({"file_path": ".env"})));
    }
}

#[cfg(test)]
mod round2_regression {
    use super::*;

    #[test]
    // Regression: a typo'd discriminator key (e.g. `commandRegex` instead of
    // `command_regex`) must fail deserialization, not silently degrade to
    // ExactTool which would unconditionally allow the tool.
    fn rejects_unknown_discriminator_keys() {
        let r: Result<ToolRule, _> =
            serde_json::from_str(r#"{"tool": "Bash", "commandRegex": "^cargo test"}"#);
        assert!(r.is_err(), "expected deserialization error for typo'd key");
        let msg = format!("{}", r.unwrap_err());
        assert!(
            msg.contains("commandRegex") || msg.contains("unknown"),
            "error must mention the unknown key, got: {msg}"
        );
    }

    #[test]
    // Plain `{"tool": "Foo"}` (no rule discriminator) is still treated as an
    // exact tool — that's the documented escape hatch and should keep working.
    fn plain_tool_object_still_parses_as_exact() {
        let r: ToolRule = serde_json::from_str(r#"{"tool": "Foo"}"#).unwrap();
        assert!(matches!(r, ToolRule::ExactTool(ref n) if n == "Foo"));
    }
}
