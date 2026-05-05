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
    /// Match MCP tools from a server by remote tool-name glob.
    McpServerGlob { server: String, tool_glob: String },
}

impl ToolRule {
    /// Construct a `BashCommandRegex` rule, validating the regex string.
    /// Returns a human-readable error if the regex does not compile.
    pub fn try_bash_command_regex(pattern: impl Into<String>) -> Result<Self, String> {
        let p = pattern.into();
        Regex::new(&p).map_err(|e| format!("invalid command_regex {:?}: {e}", p))?;
        Ok(ToolRule::BashCommandRegex(p))
    }

    /// Construct a `FilePathGlob` rule, validating the glob string.
    pub fn try_file_path_glob(
        tool: impl Into<String>,
        pattern: impl Into<String>,
    ) -> Result<Self, String> {
        let p = pattern.into();
        Glob::new(&p).map_err(|e| format!("invalid path_glob {:?}: {e}", p))?;
        Ok(ToolRule::FilePathGlob {
            tool: tool.into(),
            pattern: p,
        })
    }

    /// Construct a `ToolGlob` rule, validating the glob string.
    pub fn try_tool_glob(pattern: impl Into<String>) -> Result<Self, String> {
        let p = pattern.into();
        Glob::new(&p).map_err(|e| format!("invalid tool glob {:?}: {e}", p))?;
        Ok(ToolRule::ToolGlob(p))
    }

    pub fn try_mcp_server_glob(
        server: impl Into<String>,
        tool_glob: impl Into<String>,
    ) -> Result<Self, String> {
        let server = server.into();
        let tool_glob = tool_glob.into();
        if !code_agent_mcp::is_identifier_safe(&server) {
            return Err(format!("invalid MCP server name {:?}", server));
        }
        Glob::new(&tool_glob).map_err(|e| format!("invalid MCP tool_glob {:?}: {e}", tool_glob))?;
        Ok(ToolRule::McpServerGlob { server, tool_glob })
    }

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
            ToolRule::McpServerGlob { server, tool_glob } => {
                let Some(parts) = code_agent_mcp::parse_agent_tool_name(tool_name) else {
                    return false;
                };
                server == &parts.server_name && glob_match(tool_glob, &parts.remote_tool_name)
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
            ToolRule::McpServerGlob { server, tool_glob } => {
                let mut m = serializer.serialize_map(Some(2))?;
                m.serialize_entry("server", server)?;
                m.serialize_entry("tool_glob", tool_glob)?;
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
                    ToolRule::try_tool_glob(value).map_err(de::Error::custom)
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
                let mut server: Option<String> = None;
                let mut tool_glob: Option<String> = None;

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
                        "server" => server = Some(map.next_value::<String>()?),
                        "tool_glob" => tool_glob = Some(map.next_value::<String>()?),
                        other => {
                            unknown_keys.push(other.to_string());
                            let _: serde_json::Value = map.next_value()?;
                        }
                    }
                }
                if !unknown_keys.is_empty() {
                    return Err(de::Error::custom(format!(
                        "tool rule object has unknown key(s): {} (allowed: tool, command_regex, path_glob, server, tool_glob)",
                        unknown_keys.join(", ")
                    )));
                }
                if server.is_some() || tool_glob.is_some() {
                    if tool.is_some() || command_regex.is_some() || path_glob.is_some() {
                        return Err(de::Error::custom(
                            "MCP tool rule cannot be combined with tool, command_regex, or path_glob",
                        ));
                    }
                    let server = server
                        .ok_or_else(|| de::Error::custom("MCP tool rule missing `server`"))?;
                    let tool_glob = tool_glob
                        .ok_or_else(|| de::Error::custom("MCP tool rule missing `tool_glob`"))?;
                    return ToolRule::try_mcp_server_glob(server, tool_glob)
                        .map_err(de::Error::custom);
                }
                let tool =
                    tool.ok_or_else(|| de::Error::custom("tool rule object missing `tool`"))?;
                // Reject ambiguous rule objects up front: if both `command_regex`
                // and `path_glob` are present we'd silently pick one and discard
                // the other (potentially hiding a malformed pattern in the
                // discarded field). Fail loud instead.
                match (command_regex, path_glob) {
                    (Some(_), Some(_)) => Err(de::Error::custom(
                        "tool rule object cannot specify both `command_regex` and `path_glob`",
                    )),
                    (Some(re), None) => {
                        // `command_regex` is Bash-only. A non-Bash `tool` value would
                        // be silently ignored at match time, so reject up front.
                        if tool != "Bash" {
                            return Err(de::Error::custom(format!(
                                "command_regex is only valid with tool=\"Bash\", got tool={:?}",
                                tool
                            )));
                        }
                        ToolRule::try_bash_command_regex(re).map_err(de::Error::custom)
                    }
                    (None, Some(p)) => {
                        ToolRule::try_file_path_glob(tool, p).map_err(de::Error::custom)
                    }
                    (None, None) => Ok(ToolRule::ExactTool(tool)),
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

    #[test]
    // Parses MCP server/tool glob rule objects.
    fn parses_mcp_server_glob() {
        let v: ToolRule =
            serde_json::from_str(r#"{"server":"github","tool_glob":"list_*"}"#).unwrap();
        assert!(matches!(v, ToolRule::McpServerGlob { .. }));
    }

    #[test]
    // MCP server glob rules match namespaced MCP tool names.
    fn mcp_server_glob_matches() {
        let r = ToolRule::McpServerGlob {
            server: "github".into(),
            tool_glob: "list_*".into(),
        };
        assert!(r.matches("mcp__github__list_issues", &serde_json::json!({})));
        assert!(!r.matches("mcp__github__delete_issue", &serde_json::json!({})));
        assert!(!r.matches("mcp__docs__list_issues", &serde_json::json!({})));
    }
}

#[cfg(test)]
mod issue6_regression {
    use super::*;

    #[test]
    // Malformed BashCommandRegex pattern fails deserialization and the error
    // names the field and quotes the bad pattern.
    fn rejects_malformed_bash_command_regex() {
        let r: Result<ToolRule, _> =
            serde_json::from_str(r#"{"tool": "Bash", "command_regex": "["}"#);
        let err = r.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("command_regex"),
            "error must name the field, got: {msg}"
        );
        assert!(
            msg.contains("\"[\""),
            "error must quote the bad pattern, got: {msg}"
        );
    }

    #[test]
    // Malformed FilePathGlob pattern fails deserialization and the error
    // names the field and quotes the bad pattern.
    fn rejects_malformed_file_path_glob() {
        let r: Result<ToolRule, _> = serde_json::from_str(r#"{"tool": "Edit", "path_glob": "["}"#);
        let err = r.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("path_glob"),
            "error must name the field, got: {msg}"
        );
        assert!(
            msg.contains("\"[\""),
            "error must quote the bad pattern, got: {msg}"
        );
    }

    #[test]
    // Malformed ToolGlob string fails deserialization and the error mentions
    // the tool-glob context.
    fn rejects_malformed_tool_glob() {
        let r: Result<ToolRule, _> = serde_json::from_str(r#""[*""#);
        let err = r.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("tool glob"),
            "error must name the rule shape, got: {msg}"
        );
        assert!(
            msg.contains("\"[*\""),
            "error must quote the bad pattern, got: {msg}"
        );
    }

    #[test]
    // try_bash_command_regex returns Err for malformed regex.
    fn try_bash_command_regex_rejects_invalid() {
        let r = ToolRule::try_bash_command_regex("[");
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("command_regex") && msg.contains("\"[\""));
    }

    #[test]
    // try_file_path_glob returns Err for malformed glob.
    fn try_file_path_glob_rejects_invalid() {
        let r = ToolRule::try_file_path_glob("Edit", "[");
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("path_glob") && msg.contains("\"[\""));
    }

    #[test]
    // try_tool_glob returns Err for malformed glob.
    fn try_tool_glob_rejects_invalid() {
        let r = ToolRule::try_tool_glob("[*");
        assert!(r.is_err());
        let msg = r.unwrap_err();
        assert!(msg.contains("tool glob") && msg.contains("\"[*\""));
    }

    #[test]
    // Object that specifies both command_regex and path_glob is rejected so a
    // malformed pattern cannot hide in the discarded field.
    fn rejects_both_command_regex_and_path_glob() {
        let r: Result<ToolRule, _> = serde_json::from_str(
            r#"{"tool": "Bash", "command_regex": "^cargo$", "path_glob": "["}"#,
        );
        let err = r.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("command_regex") && msg.contains("path_glob"),
            "error must mention both fields, got: {msg}"
        );
    }

    #[test]
    // command_regex with a non-Bash tool is rejected because the rule would
    // silently never match at runtime (matches() short-circuits when tool != "Bash").
    fn rejects_command_regex_with_non_bash_tool() {
        let r: Result<ToolRule, _> =
            serde_json::from_str(r#"{"tool": "Edit", "command_regex": "^cargo$"}"#);
        let err = r.unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("Bash") && msg.contains("\"Edit\""),
            "error must mention the requirement and the offending tool, got: {msg}"
        );
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
