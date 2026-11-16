use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::builtin_hooks::redact_secrets_in_str;
use crate::conversation::ToolCall;
use crate::events::{EnforcementLayer, SecurityDecision, SecurityOutcome, SecurityPrincipal};
use crate::options::SessionId;
use crate::permission::PermissionKind;
use crate::policy::{PolicyDecision, PolicyDecisionSource, PolicyOutcome};
use crate::run_id::RunId;

/// Per-call context shared by every `SecurityDecision` the loop emits.
pub struct AuditScope<'a> {
    pub session_id: &'a SessionId,
    pub run_id: &'a RunId,
    pub turn_index: u32,
    pub principal: &'a SecurityPrincipal,
    pub cwd: &'a Path,
}

impl AuditScope<'_> {
    /// A `blocked` record for `call` with no policy attribution yet.
    pub fn blocked(
        &self,
        call: &ToolCall,
        action_class: Option<PermissionKind>,
        enforcement: EnforcementLayer,
        reason: &str,
    ) -> SecurityDecision {
        SecurityDecision {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_index: self.turn_index,
            principal: self.principal.clone(),
            tool_use_id: call.id.clone(),
            tool_name: call.name.clone(),
            action_class,
            resource: audit_resource(&call.name, &call.input, self.cwd),
            policy_source: None,
            rule: None,
            decision: PolicyOutcome::Deny,
            enforcement,
            outcome: SecurityOutcome::Blocked,
            reason: Some(redact_secrets_in_str(reason)),
        }
    }

    /// A record carrying the policy engine's decision for `call`, evaluated
    /// against `input` (the hook-rewritten input when a hook rewrote it).
    pub fn from_policy(
        &self,
        call: &ToolCall,
        input: &Value,
        action_class: PermissionKind,
        pdec: &PolicyDecision,
        outcome: SecurityOutcome,
    ) -> SecurityDecision {
        let enforcement = if pdec.source == PolicyDecisionSource::Hook {
            EnforcementLayer::Hook
        } else {
            EnforcementLayer::Policy
        };
        SecurityDecision {
            session_id: self.session_id.clone(),
            run_id: self.run_id.clone(),
            turn_index: self.turn_index,
            principal: self.principal.clone(),
            tool_use_id: call.id.clone(),
            tool_name: call.name.clone(),
            action_class: Some(action_class),
            resource: audit_resource(&call.name, input, self.cwd),
            policy_source: Some(pdec.source),
            rule: pdec.rule.clone(),
            decision: pdec.outcome,
            enforcement,
            outcome,
            reason: pdec.reason.as_deref().map(redact_secrets_in_str),
        }
    }
}

/// Normalized, secret-redacted target of a tool call: a workspace-relative
/// (or lexically normalized absolute) path, the command head for `Bash`, the
/// URL host, `mcp:<server>`, or `agent:<name>`. Never the full command line
/// or URL, which can carry credentials.
pub fn audit_resource(tool_name: &str, input: &Value, cwd: &Path) -> Option<String> {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let raw = if let Some(parts) = crom_mcp::parse_agent_tool_name(tool_name) {
        Some(format!("mcp:{}", parts.server_name))
    } else if tool_name == "Bash" {
        s("command").and_then(command_head)
    } else if tool_name == "Agent" {
        s("agent").map(|a| format!("agent:{a}"))
    } else if let Some(url) = s("url") {
        url_host(url)
    } else {
        ["file_path", "notebook_path", "path"]
            .iter()
            .find_map(|k| s(k))
            .map(|p| normalize_path(p, cwd))
    };
    raw.map(|r| redact_secrets_in_str(&r))
}

/// First program word of a shell command, skipping leading `NAME=value`
/// environment assignments so their values are never recorded.
fn command_head(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .find(|w| !is_env_assignment(w))
        .map(str::to_string)
}

fn is_env_assignment(word: &str) -> bool {
    match word.split_once('=') {
        Some((name, _)) => {
            !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        }
        None => false,
    }
}

/// `host[:port]` of a URL, dropping scheme, userinfo, path and query.
fn url_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map(|(_, h)| h)
        .unwrap_or(authority);
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Lexically normalize `p` (resolving `.` and `..`) and make it relative to
/// `cwd` when it lies inside it.
fn normalize_path(p: &str, cwd: &Path) -> String {
    let joined = if Path::new(p).is_absolute() {
        PathBuf::from(p)
    } else {
        cwd.join(p)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    match out.strip_prefix(cwd) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
        Ok(_) => ".".into(),
        Err(_) => out.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    // Relative and dot-laden paths inside the workspace normalize to a workspace-relative path.
    fn path_inside_workspace_is_relative_and_normalized() {
        let cwd = Path::new("/work");
        let r = audit_resource("Write", &json!({"file_path": "./src/../src/a.rs"}), cwd);
        assert_eq!(r.as_deref(), Some("src/a.rs"));
    }

    #[test]
    // Paths that escape the workspace via `..` are recorded as their normalized absolute form.
    fn escaping_path_is_recorded_absolute() {
        let cwd = Path::new("/work");
        let r = audit_resource("Read", &json!({"file_path": "../etc/passwd"}), cwd);
        assert_eq!(r.as_deref(), Some("/etc/passwd"));
    }

    #[test]
    // Bash records only the command head and skips env assignments that could carry secrets.
    fn bash_records_command_head_without_env_values() {
        let r = audit_resource(
            "Bash",
            &json!({"command": "API_TOKEN=abc123456789 curl https://x"}),
            Path::new("/w"),
        );
        assert_eq!(r.as_deref(), Some("curl"));
    }

    #[test]
    // URL targets are reduced to the host, dropping userinfo credentials and the path.
    fn url_records_host_without_credentials() {
        let r = audit_resource(
            "WebFetch",
            &json!({"url": "https://user:pw@Example.com:8443/a?b=c"}),
            Path::new("/w"),
        );
        assert_eq!(r.as_deref(), Some("example.com:8443"));
    }

    #[test]
    // MCP tools are recorded by server name.
    fn mcp_tool_records_server() {
        let r = audit_resource("mcp__github__create_issue", &json!({}), Path::new("/w"));
        assert_eq!(r.as_deref(), Some("mcp:github"));
    }
}
