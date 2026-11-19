//! Reading Crom Guard's `tools/call` results (MILESTONE_18 §9.5, §12.4).
//! Guard is an independent gate: a Guard denial, approval requirement,
//! malformed response, or an unreachable Guard must reach the model as a
//! tool error and the audit as `blocked` at layer `guard`, never as success.
//! A timeout is different: the call may have run, so it is never reported
//! as "not authorized".

use crom_mcp::{McpError, McpOptions, McpToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::events::{EnforcementLayer, SecurityDecision, SecurityOutcome};
use crate::policy::PolicyOutcome;
use crate::tool::ToolResult;
use crate::tool_rule::ToolRule;

/// `ToolResult.metadata` key holding the [`GuardVerdict`].
pub const GUARD_METADATA_KEY: &str = "guard";

/// Name under which `ToolPolicy::guard` registers Guard; see
/// [`check_guard_allow_rules`].
pub const GUARD_SERVER_NAME: &str = "guard";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardStatus {
    Executed,
    Denied,
    ApprovalRequired,
    /// Authorized, then the precondition check or executor failed.
    Failed,
    /// Connection or JSON-RPC error: Guard gave no verdict.
    Unavailable,
    /// The request timed out: Guard may have executed it.
    OutcomeUnknown,
    /// An error result Crom could not read as a Guard verdict.
    Malformed,
}

impl GuardStatus {
    /// Whether the call is recorded `blocked`: Guard did not authorize it.
    pub fn is_blocked(self) -> bool {
        !matches!(self, Self::Executed | Self::Failed | Self::OutcomeUnknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardVerdict {
    pub status: GuardStatus,
    /// Guard's stable code (`resource_not_found`, `approval_required`, ...)
    /// or a Crom-side code for `unavailable` / `outcome_unknown` / `malformed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// `_meta.decision_id`, the join key to Guard's audit log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// What the host's approver did with an `approval_required` result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<GuardApprovalRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardApprovalStep {
    /// No approval id, or one that is not a safe identifier.
    Declined,
    /// The approver did not answer within the configured timeout.
    TimedOut,
    /// The run was cancelled while the approver was asked.
    Cancelled,
    /// The call was retried once with the approver's approval id.
    Retried,
}

/// Audit record of a Guard approval round. It never holds the approval id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuardApprovalRecord {
    pub step: GuardApprovalStep,
    /// Guard's decision id for the first, `approval_required`, call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_required_decision_id: Option<String>,
}

/// A Guard `approval_required` result, shown to the host's [`GuardApprover`].
/// It carries no arguments: the approver should review Guard's own pending
/// request (`GET /admin/approvals/{action_digest}`), which is what Guard
/// will bind the approval to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardApprovalRequest {
    /// The Crom session and tool call that hit `approval_required`.
    pub session_id: String,
    pub tool_use_id: Option<String>,
    pub server_name: String,
    /// Guard's operation name, e.g. `refund.issue`.
    pub tool_name: String,
    pub action_digest: String,
    pub required_role: Option<String>,
}

/// The trusted application's source of Guard approvals (MILESTONE_18 §9.4).
/// Distinct from Crom's own approval callback, which never satisfies Guard.
#[async_trait::async_trait]
pub trait GuardApprover: Send + Sync {
    /// Return the approval id obtained from Guard's admin API to retry the
    /// call once with `_meta.approval_id`, or `None` to leave it blocked.
    async fn request_approval(&self, request: GuardApprovalRequest) -> Option<String>;
}

/// A run's Guard approver. It travels in that run's `ToolContext`, so a
/// subagent run, whose options never carry it, cannot use it even though it
/// shares the parent's Guard tool objects.
#[derive(Clone)]
pub struct GuardApprovalHook {
    pub approver: std::sync::Arc<dyn GuardApprover>,
    /// How long to wait for the approver; `None` waits until it answers or
    /// the run is cancelled. Expiry leaves the call `approval_required`.
    pub timeout: Option<std::time::Duration>,
}

impl std::fmt::Debug for GuardApprovalHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardApprovalHook")
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

/// The approval request in a Guard `approval_required` result, if it has a
/// well-formed digest.
pub fn approval_request(
    session_id: &str,
    tool_use_id: Option<&str>,
    server_name: &str,
    tool_name: &str,
    result: &McpToolResult,
) -> Option<GuardApprovalRequest> {
    if verdict_from_result(result).status != GuardStatus::ApprovalRequired {
        return None;
    }
    let structured = result.structured_content.as_ref()?;
    let digest = structured.get("action_digest")?.as_str()?;
    if !is_safe_id(digest) {
        return None;
    }
    Some(GuardApprovalRequest {
        session_id: session_id.to_owned(),
        tool_use_id: tool_use_id.map(str::to_owned),
        server_name: server_name.to_owned(),
        tool_name: tool_name.to_owned(),
        action_digest: digest.to_owned(),
        required_role: structured
            .get("role")
            .and_then(Value::as_str)
            .filter(|r| is_safe_id(r))
            .map(str::to_owned),
    })
}

/// The `_meta` for a retry, or `None` if the approver's id is not a safe
/// identifier (it is then never sent).
pub fn approval_meta(approval_id: &str) -> Option<Value> {
    is_safe_id(approval_id).then(|| serde_json::json!({ "approval_id": approval_id }))
}

/// Classify a `tools/call` result from a Guard server. `isError: false` is
/// always `executed`: Guard then puts the operation's own output in
/// `structuredContent`, whose `status`/`code` are business data.
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
    let (status, code) = if !result.is_error {
        (GuardStatus::Executed, None)
    } else {
        match field("status") {
            Some("denied") => (GuardStatus::Denied, code()),
            Some("approval_required") => (
                GuardStatus::ApprovalRequired,
                Some("approval_required".to_owned()),
            ),
            Some("failed") => (GuardStatus::Failed, code()),
            _ => (
                GuardStatus::Malformed,
                Some("malformed_response".to_owned()),
            ),
        }
    };
    GuardVerdict {
        status,
        code,
        decision_id,
        approval: None,
    }
}

/// Classify a failed `tools/call` request. The error text is never kept:
/// transport errors can echo request details.
pub fn verdict_from_error(err: &McpError) -> GuardVerdict {
    let (status, code) = match err {
        McpError::Timeout { .. } => (GuardStatus::OutcomeUnknown, "guard_timeout"),
        McpError::JsonRpc { error, .. } => (
            GuardStatus::Unavailable,
            error
                .data
                .as_ref()
                .and_then(|d| d.get("code"))
                .and_then(Value::as_str)
                .map_or("guard_error", safe_code),
        ),
        _ => (GuardStatus::Unavailable, "guard_unavailable"),
    };
    GuardVerdict {
        status,
        code: Some(code.to_owned()),
        decision_id: None,
        approval: None,
    }
}

impl GuardVerdict {
    /// The tool error the model sees when Guard gave no result.
    pub fn into_error_result(self) -> ToolResult {
        let code = self.code.as_deref().unwrap_or("guard_unavailable");
        let text = if self.status == GuardStatus::OutcomeUnknown {
            format!(
                "Guard request failed ({code}); outcome unknown — the action may have run; \
                 check before retrying."
            )
        } else {
            format!("Guard request failed ({code}); the call was not authorized.")
        };
        let mut res = ToolResult::error_text(text);
        res.metadata = serde_json::json!({ GUARD_METADATA_KEY: self });
        res
    }

    fn sanitized(mut self) -> Self {
        self.code = self.code.map(|c| safe_code(&c).to_owned());
        self.decision_id = self.decision_id.filter(|d| is_safe_id(d));
        if let Some(a) = &mut self.approval {
            a.approval_required_decision_id = a
                .approval_required_decision_id
                .take()
                .filter(|d| is_safe_id(d));
        }
        self
    }
}

/// The verdict a Guard-backed MCP tool attached to its result, if any. The
/// caller must only ask for results of `guard: true` MCP tools.
pub fn verdict_of(result: &ToolResult) -> Option<GuardVerdict> {
    result
        .metadata
        .get(GUARD_METADATA_KEY)
        .and_then(|v| serde_json::from_value::<GuardVerdict>(v.clone()).ok())
        .map(GuardVerdict::sanitized)
}

/// Record `verdict` on the call's `SecurityDecision` (layer `guard`).
pub fn apply_verdict(audit: &mut SecurityDecision, verdict: GuardVerdict) {
    audit.enforcement = EnforcementLayer::Guard;
    audit.guard_decision_id = verdict.decision_id;
    audit.guard_approval = verdict.approval;
    match verdict.status {
        GuardStatus::Executed => {}
        GuardStatus::Failed | GuardStatus::OutcomeUnknown => {
            audit.outcome = SecurityOutcome::Failed;
            audit.reason = verdict.code;
        }
        _ => {
            audit.decision = PolicyOutcome::Deny;
            audit.outcome = SecurityOutcome::Blocked;
            audit.reason = verdict.code;
        }
    }
}

/// Guard's own `guard.*` tools (e.g. `guard.delegate`, which returns a
/// child session handle). Crom never exposes them to the agent: a handle in
/// a tool result would reach the model, events and the session file.
pub fn is_reserved_guard_tool(remote_name: &str) -> bool {
    remote_name.starts_with("guard.")
}

/// Refuse allow rules that target the MCP server named `guard` unless that
/// server is configured `guard: true`, so a same-named ordinary server
/// cannot inherit them.
pub fn check_guard_allow_rules(rules: &[ToolRule], mcp: &McpOptions) -> Result<(), String> {
    let Some(server) = mcp.servers.get(GUARD_SERVER_NAME) else {
        return Ok(());
    };
    if !mcp.enabled || server.is_guard() {
        return Ok(());
    }
    let prefix = format!("mcp__{GUARD_SERVER_NAME}__");
    let targets_guard = |r: &ToolRule| match r {
        ToolRule::McpServerGlob { server, .. } => server == GUARD_SERVER_NAME,
        ToolRule::ExactTool(n) | ToolRule::ToolGlob(n) => n.starts_with(&prefix),
        _ => false,
    };
    match rules.iter().find(|r| targets_guard(r)) {
        Some(r) => Err(format!(
            "allow rule {r:?} targets MCP server {GUARD_SERVER_NAME:?}, which is not configured \
             `guard: true`"
        )),
        None => Ok(()),
    }
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

    /// `isError: false` is `executed` whatever `structuredContent` says: Guard puts the operation's own output there.
    #[test]
    fn executed_result_ignores_business_status_field() {
        for status in ["failed", "denied", "approval_required"] {
            let v = verdict_from_result(&result(json!({
                "content": [{"type": "text", "text": "{}"}],
                "structuredContent": {"status": status, "code": "refund_pending", "refund_id": "r1"},
                "isError": false,
                "_meta": meta()
            })));
            assert_eq!(v.status, GuardStatus::Executed, "{status}");
            assert_eq!(v.code, None);
            assert_eq!(v.decision_id.as_deref(), Some("dec-0001"));
        }
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

    /// Connection and JSON-RPC errors are `unavailable` (blocked); a JSON-RPC error keeps Guard's safe `data.code`.
    #[test]
    fn transport_errors_are_unavailable() {
        let h = verdict_from_error(&McpError::Transport("connection refused".into()));
        assert_eq!(h.status, GuardStatus::Unavailable);
        assert_eq!(h.code.as_deref(), Some("guard_unavailable"));
        assert!(h.status.is_blocked());
        let r = verdict_from_error(&McpError::JsonRpc {
            id: None,
            error: Box::new(JsonRpcError {
                code: -32002,
                message: "rate_limited".into(),
                data: Some(json!({"code": "rate_limited"})),
            }),
        });
        assert_eq!(r.status, GuardStatus::Unavailable);
        assert_eq!(r.code.as_deref(), Some("rate_limited"));
    }

    /// A timeout is `outcome_unknown`: not blocked, and the model is told the action may have run.
    #[test]
    fn timeout_is_outcome_unknown() {
        let t = verdict_from_error(&McpError::Timeout { seconds: 5 });
        assert_eq!(t.status, GuardStatus::OutcomeUnknown);
        assert_eq!(t.code.as_deref(), Some("guard_timeout"));
        assert!(!t.status.is_blocked());
        let res = t.into_error_result();
        assert!(res.is_error);
        let text = res.flatten_text();
        assert!(text.contains("outcome unknown"), "{text}");
        assert!(text.contains("may have run"), "{text}");
        assert!(!text.contains("not authorized"), "{text}");
        let u = verdict_from_error(&McpError::Transport("refused".into())).into_error_result();
        assert!(u.flatten_text().contains("not authorized"));
    }

    /// The verdict round-trips through tool-result metadata and is absent for ordinary results.
    #[test]
    fn verdict_round_trips_through_tool_result_metadata() {
        let v = verdict_from_error(&McpError::Timeout { seconds: 5 });
        let res = v.clone().into_error_result();
        assert_eq!(verdict_of(&res), Some(v));
        assert_eq!(verdict_of(&ToolResult::ok_text("x")), None);
    }

    /// Codes and decision ids read back from metadata are re-filtered to the safe character set.
    #[test]
    fn verdict_of_refilters_metadata() {
        let mut res = ToolResult::error_text("x");
        res.metadata = json!({GUARD_METADATA_KEY: {
            "status": "denied", "code": "Bearer gsh_x y", "decision_id": "a b"
        }});
        let v = verdict_of(&res).unwrap();
        assert_eq!(v.code.as_deref(), Some("unknown"));
        assert_eq!(v.decision_id, None);
    }

    fn base_decision() -> SecurityDecision {
        serde_json::from_value(json!({
            "session_id": "s", "run_id": "r", "turn_index": 1,
            "principal": {"kind": "session"}, "tool_use_id": "t",
            "tool_name": "mcp__guard__refund_issue", "decision": "allow",
            "enforcement": "policy", "outcome": "failed"
        }))
        .unwrap()
    }

    /// Blocked verdicts audit as blocked/deny; outcome-unknown audits as failed with Crom's allow kept.
    #[test]
    fn apply_verdict_sets_layer_outcome_and_reason() {
        let mut d = base_decision();
        apply_verdict(
            &mut d,
            GuardVerdict {
                status: GuardStatus::Denied,
                code: Some("resource_not_found".into()),
                decision_id: Some("dec-1".into()),
                approval: Some(GuardApprovalRecord {
                    step: GuardApprovalStep::Declined,
                    approval_required_decision_id: Some("dec-0".into()),
                }),
            },
        );
        assert_eq!(d.enforcement, EnforcementLayer::Guard);
        assert_eq!(d.outcome, SecurityOutcome::Blocked);
        assert_eq!(d.decision, PolicyOutcome::Deny);
        assert_eq!(d.reason.as_deref(), Some("resource_not_found"));
        assert_eq!(d.guard_decision_id.as_deref(), Some("dec-1"));
        let a = d.guard_approval.clone().unwrap();
        assert_eq!(a.step, GuardApprovalStep::Declined);
        assert_eq!(a.approval_required_decision_id.as_deref(), Some("dec-0"));

        let mut d = base_decision();
        apply_verdict(
            &mut d,
            verdict_from_error(&McpError::Timeout { seconds: 5 }),
        );
        assert_eq!(d.enforcement, EnforcementLayer::Guard);
        assert_eq!(d.outcome, SecurityOutcome::Failed);
        assert_eq!(d.decision, PolicyOutcome::Allow);
        assert_eq!(d.reason.as_deref(), Some("guard_timeout"));
    }

    /// A Guard-reserved `guard.*` tool (e.g. `guard.delegate`) is never exposed to the agent.
    #[test]
    fn reserved_guard_tools_are_not_exposed() {
        assert!(is_reserved_guard_tool("guard.delegate"));
        assert!(!is_reserved_guard_tool("customer.read"));
    }

    fn mcp(servers: Value) -> crom_mcp::McpOptions {
        crom_mcp::McpOptions {
            enabled: true,
            servers: serde_json::from_value(servers).unwrap(),
            ..Default::default()
        }
    }

    /// An allow rule for server `guard` is refused unless that server is configured `guard: true`.
    #[test]
    fn guard_allow_rule_requires_guard_server() {
        let rules = [ToolRule::McpServerGlob {
            server: "guard".into(),
            tool_glob: "*".into(),
        }];
        let plain = mcp(json!({"guard": {"type": "http", "url": "http://127.0.0.1:1/mcp"}}));
        assert!(check_guard_allow_rules(&rules, &plain).is_err());
        let glob = [ToolRule::ToolGlob("mcp__guard__*".into())];
        assert!(check_guard_allow_rules(&glob, &plain).is_err());
        let exact = [ToolRule::ExactTool("mcp__guard__customer_read".into())];
        assert!(check_guard_allow_rules(&exact, &plain).is_err());
        let real =
            mcp(json!({"guard": {"type": "http", "url": "http://127.0.0.1:1/mcp", "guard": true}}));
        assert!(check_guard_allow_rules(&rules, &real).is_ok());
        let other = [ToolRule::ExactTool("mcp__docs__search".into())];
        assert!(check_guard_allow_rules(&other, &plain).is_ok());
    }

    /// Only an `approval_required` result with a safe digest yields an approval request carrying the session and call ids; an unsafe role is dropped.
    #[test]
    fn approval_request_needs_approval_required_and_safe_digest() {
        let req = |sc: Value, is_error: bool| {
            approval_request(
                "sess-1",
                Some("call-1"),
                "guard",
                "refund.issue",
                &result(json!({"structuredContent": sc, "isError": is_error, "_meta": meta()})),
            )
        };
        let r = req(
            json!({"status": "approval_required", "role": "supervisor", "action_digest": "ab12"}),
            true,
        )
        .unwrap();
        assert_eq!(r.action_digest, "ab12");
        assert_eq!(r.tool_name, "refund.issue");
        assert_eq!(r.required_role.as_deref(), Some("supervisor"));
        assert_eq!(r.session_id, "sess-1");
        assert_eq!(r.tool_use_id.as_deref(), Some("call-1"));
        let bad_role = req(
            json!({"status": "approval_required", "role": "a b", "action_digest": "ab"}),
            true,
        )
        .unwrap();
        assert_eq!(bad_role.required_role, None);
        assert!(req(
            json!({"status": "approval_required", "action_digest": "a/b"}),
            true
        )
        .is_none());
        assert!(req(json!({"status": "approval_required"}), true).is_none());
        assert!(req(json!({"status": "denied", "action_digest": "ab"}), true).is_none());
        assert!(req(
            json!({"status": "approval_required", "action_digest": "ab"}),
            false
        )
        .is_none());
    }

    /// An approval id is sent as `_meta.approval_id` only if it is a safe identifier.
    #[test]
    fn approval_meta_rejects_unsafe_ids() {
        assert_eq!(
            approval_meta("ap_0123"),
            Some(json!({"approval_id": "ap_0123"}))
        );
        assert_eq!(approval_meta(""), None);
        assert_eq!(approval_meta("ap_1\nX: y"), None);
        assert_eq!(approval_meta(&"a".repeat(129)), None);
    }
}
