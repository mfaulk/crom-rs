//! Formal permission policy engine.
//!
//! Evaluation order is fixed:
//! 1. Hooks
//! 2. Disallow rules
//! 3. Permission mode
//! 4. Allow rules
//! 5. Approval callback
//! 6. Default deny
//!
//! Hook decisions can rewrite the tool input; downstream stages then see the
//! rewritten input. Disallow rules always override allow rules.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;

use crate::hooks::{
    aggregate, HookAddedContext, HookAggregateDecision, HookEventName, HookInput, Hooks,
};
use crate::options::SessionId;
use crate::permission::{
    ApprovalMemoryMode, PermissionCallback, PermissionDecision, PermissionKind, PermissionMode,
    PermissionRequest,
};
use crate::run_id::RunId;
use crate::tool_rule::ToolRule;

/// Final outcome of policy evaluation for a tool call.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyOutcome {
    Allow,
    Deny,
    Ask,
}

/// What stage of the pipeline produced the decision.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecisionSource {
    Hook,
    DisallowRule,
    PermissionMode,
    AllowRule,
    ApprovalCallback,
    DefaultDeny,
    ApprovalMemory,
    ExternalEffectPolicy,
}

/// A single policy decision with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    pub outcome: PolicyOutcome,
    pub source: PolicyDecisionSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_tool_input: Option<Value>,
    /// Concatenated `AddContext` content from `UserPromptSubmit` hooks.
    /// Empty for tool decisions.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub added_context: String,
}

impl PolicyDecision {
    pub fn allow(source: PolicyDecisionSource, reason: Option<String>) -> Self {
        Self {
            outcome: PolicyOutcome::Allow,
            source,
            reason,
            updated_tool_input: None,
            added_context: String::new(),
        }
    }

    pub fn deny(source: PolicyDecisionSource, reason: String) -> Self {
        Self {
            outcome: PolicyOutcome::Deny,
            source,
            reason: Some(reason),
            updated_tool_input: None,
            added_context: String::new(),
        }
    }
}

/// In-memory approval cache keyed by approval-key strings.
///
/// `AllowForSession` and `AllowForProject` decisions are stored in
/// separate vectors so a future on-disk persistence layer (M3) can
/// distinguish them. At M2-runtime the evaluator treats both as
/// "allow this key again," so within a single live process they are
/// behaviorally equivalent. The intent of the variant is preserved
/// for the snapshot/persistence path.
#[derive(Default, Clone)]
pub struct ApprovalMemory {
    inner: Arc<Mutex<ApprovalMemoryInner>>,
}

#[derive(Default)]
struct ApprovalMemoryInner {
    session_allow: Vec<String>,
    session_deny: Vec<(String, String)>,
    project_allow: Vec<String>,
}

impl ApprovalMemory {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn record(&self, key: String, decision: &PermissionDecision) {
        let mut g = self.inner.lock().await;
        match decision {
            PermissionDecision::AllowForSession if !g.session_allow.contains(&key) => {
                g.session_allow.push(key);
            }
            PermissionDecision::AllowForProject if !g.project_allow.contains(&key) => {
                g.project_allow.push(key);
            }
            PermissionDecision::DenyForSession { reason }
                if !g.session_deny.iter().any(|(k, _)| k == &key) =>
            {
                g.session_deny.push((key, reason.clone()));
            }
            _ => {}
        }
    }

    pub async fn lookup(&self, key: &str) -> Option<MemoryDecision> {
        let g = self.inner.lock().await;
        if let Some((_, r)) = g.session_deny.iter().find(|(k, _)| k == key) {
            return Some(MemoryDecision::Deny(r.clone()));
        }
        if g.session_allow.iter().any(|k| k == key) || g.project_allow.iter().any(|k| k == key) {
            return Some(MemoryDecision::Allow);
        }
        None
    }

    pub async fn snapshot(&self) -> ApprovalMemorySnapshot {
        let g = self.inner.lock().await;
        ApprovalMemorySnapshot {
            session_allow: g.session_allow.clone(),
            session_deny: g.session_deny.clone(),
            project_allow: g.project_allow.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum MemoryDecision {
    Allow,
    Deny(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalMemorySnapshot {
    pub session_allow: Vec<String>,
    pub session_deny: Vec<(String, String)>,
    pub project_allow: Vec<String>,
}

/// Build a stable approval-memory key for a tool invocation.
///
/// The key embeds tool-name and the most-identifying portion of the input
/// (the `command` for Bash, the `file_path` for filesystem tools). This means
/// "allow Bash for the rest of the session" allows that exact command again,
/// not arbitrary other Bash invocations — which is the intent of allow-once-
/// for-session for execution tools.
pub fn approval_key(tool_name: &str, input: &Value) -> String {
    if tool_name == "Bash" {
        let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
        format!("Bash::{}", cmd)
    } else if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
        format!("{}::{}", tool_name, fp)
    } else {
        tool_name.to_string()
    }
}

/// Configuration for the policy engine.
pub struct PolicyConfig {
    pub allowed_tools: Vec<ToolRule>,
    /// Simple tool-name allowlist used by `dontAsk` mode only. In other
    /// modes this list acts purely as a registry filter; in `dontAsk` it
    /// also implicitly allows the listed tools without prompting.
    pub allowed_tool_names: Vec<String>,
    pub disallowed_tools: Vec<ToolRule>,
    pub permission_mode: PermissionMode,
    pub callback: Option<Arc<dyn PermissionCallback>>,
    pub memory: ApprovalMemory,
    pub memory_mode: ApprovalMemoryMode,
    pub mcp_external_effect_policy: code_agent_mcp::McpExternalEffectPolicy,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            allowed_tools: Vec::new(),
            allowed_tool_names: Vec::new(),
            disallowed_tools: Vec::new(),
            permission_mode: PermissionMode::Default,
            callback: None,
            memory: ApprovalMemory::new(),
            memory_mode: ApprovalMemoryMode::default(),
            mcp_external_effect_policy:
                code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
        }
    }
}

/// Context for evaluating one tool call.
pub struct PolicyToolContext<'a> {
    pub session_id: &'a SessionId,
    pub run_id: &'a RunId,
    pub turn_index: u32,
    pub cwd: &'a std::path::Path,
    pub tool_name: &'a str,
    pub permission_kind: PermissionKind,
    pub tool_input: Value,
    pub tool_summary: String,
    pub metadata: Value,
}

/// The policy engine itself.
pub struct PolicyEngine {
    pub config: PolicyConfig,
    pub hooks: Hooks,
}

impl PolicyEngine {
    pub fn new(config: PolicyConfig, hooks: Hooks) -> Self {
        Self { config, hooks }
    }

    /// Run hooks for `UserPromptSubmit` and aggregate `AddContext` outputs.
    pub async fn evaluate_user_prompt(
        &self,
        session_id: &SessionId,
        run_id: &RunId,
        cwd: &std::path::Path,
        prompt: &str,
    ) -> (HookAggregateDecision, HookAddedContext) {
        if self.hooks.is_empty() {
            return (HookAggregateDecision::Continue, HookAddedContext::default());
        }
        let mut input = HookInput::new(
            HookEventName::UserPromptSubmit,
            session_id.clone(),
            cwd.to_path_buf(),
            run_id.clone(),
        );
        input.prompt = Some(prompt.to_string());
        let records = self.hooks.dispatch(&input).await;
        aggregate(&records)
    }

    /// Evaluate the full pipeline for a tool call.
    pub async fn evaluate_tool(&self, ctx: PolicyToolContext<'_>) -> PolicyDecision {
        // 1. Pre-tool-use hooks.
        let mut tool_input = ctx.tool_input.clone();
        // When a hook returns `Ask`, downstream stages must NOT auto-allow
        // (e.g. acceptEdits-on-Edit, default-on-readonly). Track the demand.
        let mut force_callback = false;
        if !self.hooks.is_empty() {
            let mut input = HookInput::new(
                HookEventName::PreToolUse,
                ctx.session_id.clone(),
                ctx.cwd.to_path_buf(),
                ctx.run_id.clone(),
            );
            input.turn_index = ctx.turn_index;
            input.tool_name = Some(ctx.tool_name.to_string());
            input.tool_input = Some(tool_input.clone());
            input.metadata = ctx.metadata.clone();
            let records = self.hooks.dispatch(&input).await;
            let (agg, _) = aggregate(&records);
            match agg {
                HookAggregateDecision::Deny { reason } => {
                    return PolicyDecision::deny(PolicyDecisionSource::Hook, reason);
                }
                HookAggregateDecision::Allow { updated_input } => {
                    if let Some(v) = updated_input {
                        tool_input = v;
                    }
                    // Plan mode is an unconditional execution block: a hook
                    // saying "allow" must not bypass it. Hooks have already
                    // fired (observability is preserved); the *outcome* in
                    // Plan mode is always Deny.
                    if matches!(self.config.permission_mode, PermissionMode::Plan) {
                        return PolicyDecision::deny(
                            PolicyDecisionSource::PermissionMode,
                            "plan mode does not execute tools".into(),
                        );
                    }
                    let mut d =
                        PolicyDecision::allow(PolicyDecisionSource::Hook, Some("hook".into()));
                    d.updated_tool_input = Some(tool_input.clone());
                    // Hook Allow short-circuits ahead of allow rules / callback.
                    return d;
                }
                HookAggregateDecision::Ask { reason: _ } => {
                    force_callback = true;
                }
                HookAggregateDecision::Continue => {}
            }
        }

        // 2. Disallow rules.
        for r in &self.config.disallowed_tools {
            if r.matches(ctx.tool_name, &tool_input) {
                return PolicyDecision::deny(
                    PolicyDecisionSource::DisallowRule,
                    format!("disallowed: tool {} blocked by policy rule", ctx.tool_name),
                );
            }
        }

        // 2b. Approval memory (deny side checked here so deny-for-session
        // overrides allow rules, matching the intent of `DenyForSession`).
        if !matches!(self.config.memory_mode, ApprovalMemoryMode::None) {
            let key = approval_key(ctx.tool_name, &tool_input);
            if let Some(MemoryDecision::Deny(reason)) = self.config.memory.lookup(&key).await {
                return PolicyDecision::deny(PolicyDecisionSource::ApprovalMemory, reason);
            }
        }

        // 3a. Plan mode always denies — even when a hook returned `Ask`
        // (force_callback). Plan mode is an unconditional execution block.
        if matches!(self.config.permission_mode, PermissionMode::Plan) {
            return PolicyDecision::deny(
                PolicyDecisionSource::PermissionMode,
                "plan mode does not execute tools".into(),
            );
        }

        // 3b. Permission mode pre-checks. Skipped when a hook returned `Ask`
        // (force_callback): the user has been explicitly asked, so we must
        // not auto-allow under any mode.
        if !force_callback {
            match self.config.permission_mode {
                PermissionMode::Plan => unreachable!("handled above"),
                PermissionMode::DontAsk => {
                    // Allow when the structured rules match,
                    // or when the simple-name list contains the tool.
                    if let Some(d) = self.match_allow(ctx.tool_name, &tool_input, &tool_input) {
                        return d;
                    }
                    if self
                        .config
                        .allowed_tool_names
                        .iter()
                        .any(|n| n == ctx.tool_name)
                    {
                        let mut d = PolicyDecision::allow(
                            PolicyDecisionSource::AllowRule,
                            Some("dontAsk: tool name allowlist".into()),
                        );
                        d.updated_tool_input = Some(tool_input.clone());
                        return d;
                    }
                    return PolicyDecision::deny(
                        PolicyDecisionSource::DefaultDeny,
                        "dontAsk: tool not permitted by allow rules".into(),
                    );
                }
                PermissionMode::AcceptEdits => {
                    if matches!(ctx.permission_kind, PermissionKind::Mutating) {
                        let mut d = PolicyDecision::allow(
                            PolicyDecisionSource::PermissionMode,
                            Some("acceptEdits".into()),
                        );
                        d.updated_tool_input = Some(tool_input.clone());
                        return d;
                    }
                    if matches!(ctx.permission_kind, PermissionKind::ReadOnly) {
                        let mut d = PolicyDecision::allow(
                            PolicyDecisionSource::PermissionMode,
                            Some("read-only auto-allow".into()),
                        );
                        d.updated_tool_input = Some(tool_input.clone());
                        return d;
                    }
                    // Execution falls through.
                }
                PermissionMode::Default => {
                    if matches!(ctx.permission_kind, PermissionKind::ReadOnly) {
                        // Read-only tools auto-allow under default mode.
                        let mut d = PolicyDecision::allow(
                            PolicyDecisionSource::PermissionMode,
                            Some("read-only auto-allow".into()),
                        );
                        d.updated_tool_input = Some(tool_input.clone());
                        return d;
                    }
                }
            }
        }

        // 4. Allow rules. Hook-`Ask` forces a user prompt, so we skip
        // structured allow-rule auto-allow when force_callback is set.
        let mut matched_allow_rule = false;
        if !force_callback {
            if let Some(d) = self.match_allow(ctx.tool_name, &tool_input, &tool_input) {
                return d;
            }
            matched_allow_rule = self
                .config
                .allowed_tools
                .iter()
                .any(|r| r.matches(ctx.tool_name, &tool_input));
        }

        // 4a. MCP external-effect policy. MCP tools are external even when
        // their remote names look read-only; never let permission modes like
        // acceptEdits imply access to them.
        if matches!(ctx.permission_kind, PermissionKind::External) {
            match self.config.mcp_external_effect_policy {
                code_agent_mcp::McpExternalEffectPolicy::AllowWithApproval => {}
                code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule => {
                    if !matched_allow_rule {
                        return PolicyDecision::deny(
                            PolicyDecisionSource::ExternalEffectPolicy,
                            "MCP external tools require an explicit allow rule".into(),
                        );
                    }
                }
                code_agent_mcp::McpExternalEffectPolicy::DenyMutatingNameByDefault => {
                    if code_agent_mcp::registry::parse_agent_tool_name(ctx.tool_name)
                        .map(|p| code_agent_mcp::registry::mutating_tool_name(&p.remote_tool_name))
                        .unwrap_or(false)
                    {
                        return PolicyDecision::deny(
                            PolicyDecisionSource::ExternalEffectPolicy,
                            "MCP tool may modify external state and is denied by policy".into(),
                        );
                    }
                }
            }
        }

        // 4b. Approval-memory allow lookup. Skipped when a hook returned
        // `Ask`, because the hook explicitly demanded user confirmation —
        // a prior `AllowForSession` must not silently bypass that.
        if !force_callback && !matches!(self.config.memory_mode, ApprovalMemoryMode::None) {
            let key = approval_key(ctx.tool_name, &tool_input);
            if let Some(MemoryDecision::Allow) = self.config.memory.lookup(&key).await {
                let mut d = PolicyDecision::allow(
                    PolicyDecisionSource::ApprovalMemory,
                    Some("session/project memory allow".into()),
                );
                d.updated_tool_input = Some(tool_input.clone());
                return d;
            }
        }

        // 5. Approval callback.
        if let Some(cb) = &self.config.callback {
            let req = PermissionRequest {
                tool_name: ctx.tool_name.to_string(),
                permission_kind: ctx.permission_kind,
                input: tool_input.clone(),
                summary: ctx.tool_summary,
                metadata: ctx.metadata.clone(),
            };
            let decision = cb.approve(req).await;
            // Record memory if applicable.
            if !matches!(self.config.memory_mode, ApprovalMemoryMode::None) {
                let key = approval_key(ctx.tool_name, &tool_input);
                self.config.memory.record(key, &decision).await;
            }
            return match decision {
                PermissionDecision::AllowOnce
                | PermissionDecision::AllowForSession
                | PermissionDecision::AllowForProject => {
                    let mut d = PolicyDecision::allow(
                        PolicyDecisionSource::ApprovalCallback,
                        Some("user approved".into()),
                    );
                    d.updated_tool_input = Some(tool_input);
                    d
                }
                PermissionDecision::Deny { reason }
                | PermissionDecision::DenyForSession { reason } => {
                    PolicyDecision::deny(PolicyDecisionSource::ApprovalCallback, reason)
                }
                PermissionDecision::RequestRevision { message } => PolicyDecision::deny(
                    PolicyDecisionSource::ApprovalCallback,
                    format!("user requested revision: {message}"),
                ),
            };
        }

        // 6. Default deny.
        PolicyDecision::deny(
            PolicyDecisionSource::DefaultDeny,
            "no policy rule allowed this tool".into(),
        )
    }

    fn match_allow(
        &self,
        tool_name: &str,
        input: &Value,
        new_input: &Value,
    ) -> Option<PolicyDecision> {
        for r in &self.config.allowed_tools {
            if r.matches(tool_name, input) {
                let mut d = PolicyDecision::allow(
                    PolicyDecisionSource::AllowRule,
                    Some("allow rule".into()),
                );
                d.updated_tool_input = Some(new_input.clone());
                return Some(d);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::{Hook, HookError, HookOutput};
    use async_trait::async_trait;

    fn ctx_for<'a>(
        sid: &'a SessionId,
        rid: &'a RunId,
        cwd: &'a std::path::Path,
        tool: &'a str,
        kind: PermissionKind,
        input: serde_json::Value,
    ) -> PolicyToolContext<'a> {
        PolicyToolContext {
            session_id: sid,
            run_id: rid,
            turn_index: 0,
            cwd,
            tool_name: tool,
            permission_kind: kind,
            tool_input: input,
            tool_summary: tool.to_string(),
            metadata: serde_json::Value::Null,
        }
    }

    fn sid() -> SessionId {
        SessionId::from("sid".to_string())
    }
    fn rid() -> RunId {
        RunId::from("rid".to_string())
    }

    #[tokio::test]
    // Disallow rule overrides allow rule for the same tool.
    async fn disallow_overrides_allow() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::ExactTool("Edit".into())],
            disallowed_tools: vec![ToolRule::FilePathGlob {
                tool: "Edit".into(),
                pattern: "**/.env*".into(),
            }],
            permission_mode: PermissionMode::Default,
            callback: None,
            memory: ApprovalMemory::new(),
            memory_mode: ApprovalMemoryMode::Session,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Edit",
                PermissionKind::Mutating,
                serde_json::json!({"file_path": ".env"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::DisallowRule);
    }

    #[tokio::test]
    // ExactTool allow rule allows a tool that's otherwise behind the callback.
    async fn allow_rule_exact_works() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::ExactTool("Read".into())],
            permission_mode: PermissionMode::DontAsk,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Read",
                PermissionKind::ReadOnly,
                serde_json::json!({"file_path": "x.rs"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    // BashCommandRegex allow rule allows whitelisted commands.
    async fn allow_rule_bash_regex_works() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::BashCommandRegex("^cargo (test|check)".into())],
            permission_mode: PermissionMode::Default,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "cargo test"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    // FilePathGlob disallow rule blocks Edit/Write to .env files.
    async fn disallow_path_glob_blocks_env() {
        let cfg = PolicyConfig {
            disallowed_tools: vec![ToolRule::FilePathGlob {
                tool: "Edit".into(),
                pattern: "**/.env*".into(),
            }],
            permission_mode: PermissionMode::AcceptEdits,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Edit",
                PermissionKind::Mutating,
                serde_json::json!({"file_path": ".env.local"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    // AcceptEdits allows Edit but Bash still goes through the callback.
    async fn accept_edits_allows_edit_not_bash() {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::AcceptEdits,
            callback: None,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d_edit = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Edit",
                PermissionKind::Mutating,
                serde_json::json!({"file_path": "x.rs"}),
            ))
            .await;
        let d_bash = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d_edit.outcome, PolicyOutcome::Allow);
        assert_eq!(d_bash.outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    // Default mode auto-allows read-only tools but defers mutating to callback.
    async fn default_mode_read_auto_allow_mutating_needs_callback() {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Default,
            callback: None,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let read = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Read",
                PermissionKind::ReadOnly,
                serde_json::json!({"file_path": "x"}),
            ))
            .await;
        let write = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Write",
                PermissionKind::Mutating,
                serde_json::json!({"file_path": "x"}),
            ))
            .await;
        assert_eq!(read.outcome, PolicyOutcome::Allow);
        assert_eq!(write.outcome, PolicyOutcome::Deny);
    }

    #[tokio::test]
    // DontAsk mode denies tools that don't match an allow rule, without prompting.
    async fn dont_ask_denies_without_prompt() {
        struct CountingCb {
            calls: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl PermissionCallback for CountingCb {
            async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
                *self.calls.lock().await += 1;
                PermissionDecision::AllowOnce
            }
        }
        let calls = Arc::new(Mutex::new(0u32));
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::DontAsk,
            callback: Some(Arc::new(CountingCb {
                calls: calls.clone(),
            })),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(*calls.lock().await, 0, "callback must not be invoked");
    }

    #[tokio::test]
    // MCP exact allow rules permit external tools under the default MCP policy.
    async fn mcp_exact_allow_works() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::ExactTool("mcp__fake__list_issues".into())],
            permission_mode: PermissionMode::Default,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__list_issues",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    // MCP wildcard allow rules permit matching external tools.
    async fn mcp_wildcard_allow_works() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::ToolGlob("mcp__fake__list_*".into())],
            permission_mode: PermissionMode::Default,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__list_issues",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    // McpServerGlob allow rules match server and remote tool names.
    async fn mcp_server_glob_allow_works() {
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::McpServerGlob {
                server: "fake".into(),
                tool_glob: "list_*".into(),
            }],
            permission_mode: PermissionMode::Default,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__list_issues",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
    }

    #[tokio::test]
    // acceptEdits does not approve unresolved MCP external tools.
    async fn accept_edits_does_not_approve_mcp() {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::AcceptEdits,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__list_issues",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::ExternalEffectPolicy);
    }

    #[tokio::test]
    // AllowWithApproval lets unresolved MCP tools reach the approval callback.
    async fn mcp_allow_with_approval_reaches_callback() {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Default,
            callback: Some(Arc::new(crate::permission::AlwaysAllow)),
            mcp_external_effect_policy: code_agent_mcp::McpExternalEffectPolicy::AllowWithApproval,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__list_issues",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
        assert_eq!(d.source, PolicyDecisionSource::ApprovalCallback);
    }

    #[tokio::test]
    // DenyMutatingNameByDefault blocks write-like MCP names without allow rules.
    async fn mcp_deny_mutating_name_by_default_blocks_write_like() {
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Default,
            callback: Some(Arc::new(crate::permission::AlwaysAllow)),
            mcp_external_effect_policy:
                code_agent_mcp::McpExternalEffectPolicy::DenyMutatingNameByDefault,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                std::path::Path::new("."),
                "mcp__fake__delete_issue",
                PermissionKind::External,
                serde_json::json!({}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::ExternalEffectPolicy);
    }

    #[tokio::test]
    // AllowForSession permits the same tool to run again in the same session
    // without re-prompting.
    async fn approval_memory_session_allow() {
        struct OnceCb {
            count: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl PermissionCallback for OnceCb {
            async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
                *self.count.lock().await += 1;
                PermissionDecision::AllowForSession
            }
        }
        let count = Arc::new(Mutex::new(0u32));
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Default,
            callback: Some(Arc::new(OnceCb {
                count: count.clone(),
            })),
            memory_mode: ApprovalMemoryMode::Session,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        for _ in 0..3 {
            let d = engine
                .evaluate_tool(ctx_for(
                    &sid,
                    &rid,
                    cwd,
                    "Bash",
                    PermissionKind::Execution,
                    serde_json::json!({"command": "cargo test"}),
                ))
                .await;
            assert_eq!(d.outcome, PolicyOutcome::Allow);
        }
        assert_eq!(*count.lock().await, 1, "callback only first time");
    }

    #[tokio::test]
    // DenyForSession persists deny across calls in the same session.
    async fn approval_memory_deny_for_session() {
        struct OnceCb {
            count: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl PermissionCallback for OnceCb {
            async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
                *self.count.lock().await += 1;
                PermissionDecision::DenyForSession {
                    reason: "no".into(),
                }
            }
        }
        let count = Arc::new(Mutex::new(0u32));
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Default,
            callback: Some(Arc::new(OnceCb {
                count: count.clone(),
            })),
            memory_mode: ApprovalMemoryMode::Session,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, Hooks::new());
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        for _ in 0..2 {
            let d = engine
                .evaluate_tool(ctx_for(
                    &sid,
                    &rid,
                    cwd,
                    "Bash",
                    PermissionKind::Execution,
                    serde_json::json!({"command": "cargo test"}),
                ))
                .await;
            assert_eq!(d.outcome, PolicyOutcome::Deny);
        }
        assert_eq!(*count.lock().await, 1);
    }

    #[tokio::test]
    // Pipeline order: hooks come before disallow rules, so a hook Allow
    // does NOT bypass a disallow rule? Actually no — hooks come first;
    // a hook Deny wins, but a hook Allow short-circuits past disallow.
    // This test pins down that hook Allow short-circuits past disallow.
    async fn hook_allow_short_circuits() {
        struct AllowHook;
        #[async_trait]
        impl Hook for AllowHook {
            async fn call(&self, _: HookInput) -> Result<HookOutput, HookError> {
                Ok(HookOutput::Allow {
                    updated_input: None,
                })
            }
        }
        let hooks = Hooks::new().pre_tool_use("*", Arc::new(AllowHook)).unwrap();
        let cfg = PolicyConfig {
            disallowed_tools: vec![ToolRule::ExactTool("Bash".into())],
            permission_mode: PermissionMode::Default,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, hooks);
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
        assert_eq!(d.source, PolicyDecisionSource::Hook);
    }

    #[tokio::test]
    // Plan mode denies even when a hook returns Ask, and the approval
    // callback is never invoked (Plan must short-circuit before the prompt).
    async fn plan_mode_denies_when_hook_asks() {
        struct AskHook;
        #[async_trait]
        impl Hook for AskHook {
            async fn call(&self, _: HookInput) -> Result<HookOutput, HookError> {
                Ok(HookOutput::Ask {
                    reason: "hook wants to ask".into(),
                })
            }
        }
        struct CountingCb {
            calls: Arc<Mutex<u32>>,
        }
        #[async_trait]
        impl PermissionCallback for CountingCb {
            async fn approve(&self, _: PermissionRequest) -> PermissionDecision {
                *self.calls.lock().await += 1;
                PermissionDecision::AllowOnce
            }
        }
        let calls = Arc::new(Mutex::new(0u32));
        let hooks = Hooks::new()
            .pre_tool_use("*", Arc::new(AskHook))
            .expect("\"*\" is a valid hook pattern");
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Plan,
            callback: Some(Arc::new(CountingCb {
                calls: calls.clone(),
            })),
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, hooks);
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::PermissionMode);
        assert_eq!(*calls.lock().await, 0, "callback must not be invoked");
    }

    #[tokio::test]
    // Plan mode denies even when a hook returns Allow — Plan is an
    // unconditional execution block and a hook cannot bypass it.
    async fn plan_mode_denies_when_hook_allows() {
        struct AllowHook;
        #[async_trait]
        impl Hook for AllowHook {
            async fn call(&self, _: HookInput) -> Result<HookOutput, HookError> {
                Ok(HookOutput::Allow {
                    updated_input: None,
                })
            }
        }
        let hooks = Hooks::new()
            .pre_tool_use("*", Arc::new(AllowHook))
            .expect("\"*\" is a valid hook pattern");
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::Plan,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, hooks);
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::PermissionMode);
    }

    #[tokio::test]
    // Hook Deny stops the pipeline immediately.
    async fn hook_deny_stops_pipeline() {
        struct DenyHook;
        #[async_trait]
        impl Hook for DenyHook {
            async fn call(&self, _: HookInput) -> Result<HookOutput, HookError> {
                Ok(HookOutput::Deny {
                    reason: "no".into(),
                })
            }
        }
        let hooks = Hooks::new().pre_tool_use("*", Arc::new(DenyHook)).unwrap();
        let cfg = PolicyConfig {
            allowed_tools: vec![ToolRule::ExactTool("Bash".into())],
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, hooks);
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Bash",
                PermissionKind::Execution,
                serde_json::json!({"command": "ls"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Deny);
        assert_eq!(d.source, PolicyDecisionSource::Hook);
    }

    #[tokio::test]
    // Hooks can rewrite tool input and downstream stages see the new value.
    async fn hook_rewrites_input() {
        struct RewriteHook;
        #[async_trait]
        impl Hook for RewriteHook {
            async fn call(&self, _: HookInput) -> Result<HookOutput, HookError> {
                Ok(HookOutput::Allow {
                    updated_input: Some(serde_json::json!({"file_path": "rewritten.rs"})),
                })
            }
        }
        let hooks = Hooks::new()
            .pre_tool_use("Edit", Arc::new(RewriteHook))
            .unwrap();
        let cfg = PolicyConfig {
            permission_mode: PermissionMode::AcceptEdits,
            ..PolicyConfig::default()
        };
        let engine = PolicyEngine::new(cfg, hooks);
        let sid = sid();
        let rid = rid();
        let cwd = std::path::Path::new(".");
        let d = engine
            .evaluate_tool(ctx_for(
                &sid,
                &rid,
                cwd,
                "Edit",
                PermissionKind::Mutating,
                serde_json::json!({"file_path": "orig.rs"}),
            ))
            .await;
        assert_eq!(d.outcome, PolicyOutcome::Allow);
        assert_eq!(d.updated_tool_input.unwrap()["file_path"], "rewritten.rs");
    }
}
