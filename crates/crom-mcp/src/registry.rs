use serde::{Deserialize, Serialize};

use crate::transport::McpTransportKind;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolNameParts {
    pub server_name: String,
    pub remote_tool_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolSummary {
    pub server_name: String,
    pub remote_tool_name: String,
    pub agent_tool_name: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpRegisteredTool {
    pub server_name: String,
    pub remote_tool_name: String,
    pub agent_tool_name: String,
    pub transport: McpTransportKind,
}

pub fn is_identifier_safe(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn agent_tool_name(server: &str, tool: &str) -> Result<String, String> {
    if !is_identifier_safe(server) {
        return Err(format!("unsafe MCP server name {server:?}"));
    }
    // `mcp__{server}__{tool}` uses `__` as the single server/tool delimiter,
    // and `parse_agent_tool_name` splits on the *first* `__` after the prefix.
    // The remote tool name is allowed to contain `__`, so the server name
    // must not — otherwise `mcp__a__b__tool` would be ambiguous and parse back
    // to the wrong (server, tool) split, breaking server-scoped policy/hooks.
    if server.contains("__") {
        return Err(format!(
            "MCP server name {server:?} must not contain \"__\" (reserved as the tool-name delimiter)"
        ));
    }
    if !is_identifier_safe(tool) {
        return Err(format!("unsafe MCP tool name {tool:?}"));
    }
    Ok(format!("mcp__{server}__{tool}"))
}

pub fn parse_agent_tool_name(name: &str) -> Option<McpToolNameParts> {
    let rest = name.strip_prefix("mcp__")?;
    // Split only on the first `__` after the prefix: the server name never
    // contains `__` (identifier-safe, joined by exactly one `__`), but a
    // remote tool name may, so the entire remainder is the tool name. This
    // round-trips every name `agent_tool_name` produces.
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some(McpToolNameParts {
        server_name: server.to_string(),
        remote_tool_name: tool.to_string(),
    })
}

pub fn mutating_tool_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        "delete", "remove", "create", "update", "write", "insert", "send", "post", "merge",
        "close", "archive", "publish", "deploy",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Safe MCP names are joined with the required namespace format.
    fn builds_agent_tool_name() {
        assert_eq!(
            agent_tool_name("github", "list_issues").unwrap(),
            "mcp__github__list_issues"
        );
    }

    #[test]
    // Unsafe names are rejected before remote tools enter the registry.
    fn rejects_unsafe_names() {
        assert!(agent_tool_name("git-hub", "list_issues").is_err());
        assert!(agent_tool_name("github", "list issues").is_err());
    }

    #[test]
    // A server name containing `__` is rejected: it would otherwise make
    // `mcp__{server}__{tool}` ambiguous and parse back to the wrong split
    // (e.g. server "prod__github" -> parsed server "prod"), breaking
    // server-scoped allow/deny policy, hooks, and telemetry. The remote tool
    // name may still contain `__`; only the server name is constrained.
    fn rejects_server_name_with_double_underscore() {
        assert!(agent_tool_name("prod__github", "delete_issue").is_err());
        // The dangerous case must not silently round-trip to the wrong split.
        assert!(agent_tool_name("prod__github", "delete_issue").is_err());
        // A single-underscore server name is still fine.
        let name = agent_tool_name("prod_github", "delete_issue").unwrap();
        let parts = parse_agent_tool_name(&name).unwrap();
        assert_eq!(parts.server_name, "prod_github");
        assert_eq!(parts.remote_tool_name, "delete_issue");
    }

    #[test]
    // Namespaced MCP tool names parse back into server and remote tool names.
    fn parses_namespaced_tool_name() {
        let parts = parse_agent_tool_name("mcp__docs__search").unwrap();
        assert_eq!(parts.server_name, "docs");
        assert_eq!(parts.remote_tool_name, "search");
    }

    #[test]
    // A remote tool name containing `__` round-trips: the producer joins on a
    // single `__`, and the parser splits only on the first one.
    fn round_trips_tool_name_with_double_underscore() {
        let name = agent_tool_name("srv", "do__thing").unwrap();
        assert_eq!(name, "mcp__srv__do__thing");
        let parts = parse_agent_tool_name(&name).unwrap();
        assert_eq!(parts.server_name, "srv");
        assert_eq!(parts.remote_tool_name, "do__thing");
    }
}
