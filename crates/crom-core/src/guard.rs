//! Reading Crom Guard's `tools/call` results (MILESTONE_18 §9.5, §12.4).
//! Guard is an independent gate: a Guard denial, approval requirement,
//! malformed response, or an unreachable Guard must reach the model as a
//! tool error and the audit as `blocked` at layer `guard`, never as success.

use crom_mcp::{McpError, McpToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::tool::ToolResult;

/// `ToolResult.metadata` key holding the [`GuardVerdict`].
pub const GUARD_METADATA_KEY: &str = "guard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardStatus {
    Executed,
    Denied,
    ApprovalRequired,
    /// Authorized, then the precondition check or executor failed.
    Failed,
    /// Transport error, timeout, or JSON-RPC error: Guard gave no verdict.
    Unavailable,
    /// An error result Crom could not read as a Guard verdict.
    Malformed,
}

impl GuardStatus {
    /// Whether the call is recorded `blocked`: Guard did not authorize it.
    pub fn is_blocked(self) -> bool {
        !matches!(self, Self::Executed | Self::Failed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardVerdict {
    pub status: GuardStatus,
    /// Guard's stable code (`resource_not_found`, `approval_required`, ...)
    /// or a Crom-side code for `unavailable` / `malformed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// `_meta.decision_id`, the join key to Guard's audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
}

/// Classify a `tools/call` result from a Guard server.
pub fn verdict_from_result(result: &McpToolResult) -> GuardVerdict {
    let decision_id = result
        .extra
        .get("_meta")
        .and_then(|m| m.get("decision_id"))
        .and_then(Value::as_str)
        .filter(|s| is_safe_id(s))
        .map(str::to_owned);
    let structured = result.structured_content.as_ref();
    let field = |k: &str| structured.and_then(|s| s.get(k)).and_then(Value::as_str);
    let code = || Some(field("code").map_or("unknown", safe_code).to_owned());
    let (status, code) = match field("status") {
        Some("denied") => (GuardStatus::Denied, code()),
        Some("approval_required") => (
            GuardStatus::ApprovalRequired,
            Some("approval_required".to_owned()),
        ),
        Some("failed") => (GuardStatus::Failed, code()),
        _ if !result.is_error => (GuardStatus::Executed, None),
        _ => (
            GuardStatus::Malformed,
            Some("malformed_response".to_owned()),
        ),
    };
    GuardVerdict {
        status,
        code,
        decision_id,
    }
}

/// Classify a failed `tools/call` request. The error text is never kept:
/// transport errors can echo request details.
pub fn verdict_from_error(err: &McpError) -> GuardVerdict {
    let code = match err {
        McpError::Timeout { .. } => "guard_timeout",
        McpError::JsonRpc { error, .. } => error
            .data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .map_or("guard_error", safe_code),
        _ => "guard_unavailable",
    };
    GuardVerdict {
        status: GuardStatus::Unavailable,
        code: Some(code.to_owned()),
        decision_id: None,
    }
}

impl GuardVerdict {
    /// The tool error the model sees when Guard gave no result.
    pub fn into_unavailable_result(self) -> ToolResult {
        let code = self.code.as_deref().unwrap_or("guard_unavailable");
        let mut res = ToolResult::error_text(format!(
            "Guard request failed ({code}); the call was not authorized."
        ));
        res.metadata = serde_json::json!({ GUARD_METADATA_KEY: self });
        res
    }
}

/// The verdict a Guard-backed MCP tool attached to its result, if any.
pub fn verdict_of(result: &ToolResult) -> Option<GuardVerdict> {
    result
        .metadata
        .get(GUARD_METADATA_KEY)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn is_safe_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
}

fn safe_code(s: &str) -> &str {
    let ok = (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if ok {
        s
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_mcp::protocol::JsonRpcError;
    use serde_json::json;

    fn result(v: Value) -> McpToolResult {
        serde_json::from_value(v).unwrap()
    }

    fn meta() -> Value {
        json!({"decision_id": "dec-0001", "audit_head": {"seq": 2, "head_hash": "ab"}})
    }

    /// An executed Guard result is `executed` and carries the decision id from `_meta`.
    #[test]
    fn executed_result_carries_decision_id() {
        let v = verdict_from_result(&result(json!({
            "content": [{"type": "text", "text": "{}"}],
            "structuredContent": {"resource": "acme-cus-000"},
            "isError": false,
            "_meta": meta()
        })));
        assert_eq!(v.status, GuardStatus::Executed);
        assert_eq!(v.code, None);
        assert_eq!(v.decision_id.as_deref(), Some("dec-0001"));
        assert!(!v.status.is_blocked());
    }

    /// A Guard denial is blocked with its stable code.
    #[test]
    fn denied_result_is_blocked_with_code() {
        let v = verdict_from_result(&result(json!({
            "content": [{"type": "text", "text": "resource_not_found"}],
            "structuredContent": {"status": "denied", "code": "resource_not_found"},
            "isError": true,
            "_meta": meta()
        })));
        assert_eq!(v.status, GuardStatus::Denied);
        assert_eq!(v.code.as_deref(), Some("resource_not_found"));
        assert!(v.status.is_blocked());
    }

    /// `approval_required` is blocked; `failed` is not blocked but keeps its code.
    #[test]
    fn approval_required_blocks_and_failed_does_not() {
        let a = verdict_from_result(&result(json!({
            "structuredContent": {"status": "approval_required", "role": "x", "action_digest": "d"},
            "isError": true, "_meta": meta()
        })));
        assert_eq!(a.status, GuardStatus::ApprovalRequired);
        assert_eq!(a.code.as_deref(), Some("approval_required"));
        assert!(a.status.is_blocked());
        let f = verdict_from_result(&result(json!({
            "structuredContent": {"status": "failed", "code": "execution_failed"},
            "isError": true, "_meta": meta()
        })));
        assert_eq!(f.status, GuardStatus::Failed);
        assert_eq!(f.code.as_deref(), Some("execution_failed"));
        assert!(!f.status.is_blocked());
    }

    /// A denial status is never read as success, even when `isError` is false.
    #[test]
    fn denied_status_without_is_error_is_still_denied() {
        let v = verdict_from_result(&result(json!({
            "structuredContent": {"status": "denied", "code": "policy_denied"},
            "isError": false, "_meta": meta()
        })));
        assert_eq!(v.status, GuardStatus::Denied);
    }

    /// An error result without a recognizable status is malformed and blocked.
    #[test]
    fn error_without_status_is_malformed() {
        let v = verdict_from_result(&result(json!({
            "content": [{"type": "text", "text": "boom"}], "isError": true
        })));
        assert_eq!(v.status, GuardStatus::Malformed);
        assert_eq!(v.code.as_deref(), Some("malformed_response"));
        assert_eq!(v.decision_id, None);
        assert!(v.status.is_blocked());
    }

    /// Codes and decision ids outside the safe character set are dropped, never copied into audit.
    #[test]
    fn unsafe_code_and_decision_id_are_dropped() {
        let v = verdict_from_result(&result(json!({
            "structuredContent": {"status": "denied", "code": "Bearer gsh_secret value"},
            "isError": true,
            "_meta": {"decision_id": "dec 1\nx"}
        })));
        assert_eq!(v.status, GuardStatus::Denied);
        assert_eq!(v.code.as_deref(), Some("unknown"));
        assert_eq!(v.decision_id, None);
    }

    /// Transport failures are `unavailable` with a fixed code; a JSON-RPC error keeps Guard's safe `data.code`.
    #[test]
    fn transport_errors_are_unavailable() {
        let t = verdict_from_error(&McpError::Timeout { seconds: 5 });
        assert_eq!(t.status, GuardStatus::Unavailable);
        assert_eq!(t.code.as_deref(), Some("guard_timeout"));
        assert!(t.status.is_blocked());
        let h = verdict_from_error(&McpError::Transport("connection refused".into()));
        assert_eq!(h.code.as_deref(), Some("guard_unavailable"));
        let r = verdict_from_error(&McpError::JsonRpc {
            id: None,
            error: Box::new(JsonRpcError {
                code: -32002,
                message: "rate_limited".into(),
                data: Some(json!({"code": "rate_limited"})),
            }),
        });
        assert_eq!(r.code.as_deref(), Some("rate_limited"));
    }

    /// The verdict round-trips through tool-result metadata and is absent for ordinary results.
    #[test]
    fn verdict_round_trips_through_tool_result_metadata() {
        let v = verdict_from_error(&McpError::Timeout { seconds: 5 });
        let res = v.clone().into_unavailable_result();
        assert!(res.is_error);
        assert_eq!(verdict_of(&res), Some(v));
        assert_eq!(verdict_of(&ToolResult::ok_text("x")), None);
    }
}
