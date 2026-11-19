//! The agent endpoint: MCP Streamable HTTP over [`crom_guard::Guard`].
//! Every agent-visible error is a pipeline code (`DenyReason::code`, or
//! `precondition_failed` / `execution_failed`); nothing else leaks.

use std::sync::Arc;

use crom_guard::{
    CallMeta, CallOutcome, CallResult, DenyReason, GrantStore, SessionHandle, UntrustedCorrelation,
};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorCode,
    Implementation, JsonObject, ListToolsResult, MetaObject, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use serde_json::{json, Value};
use zeroize::Zeroizing;

use crate::Shared;

/// JSON-RPC error code for a `tools/list` the session may not make. The
/// message and `data.code` are the pipeline's deny code.
pub const SESSION_ERROR: ErrorCode = ErrorCode(-32001);

#[derive(Clone)]
pub(crate) struct AgentHandler {
    pub(crate) shared: Arc<Shared>,
}

/// `Authorization: Bearer <handle>` from the HTTP request; empty if absent.
fn bearer(ctx: &RequestContext<RoleServer>) -> Zeroizing<String> {
    let value = ctx
        .extensions
        .get::<http::request::Parts>()
        .and_then(|p| p.headers.get(http::header::AUTHORIZATION))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    Zeroizing::new(crate::strip_bearer(value).unwrap_or("").to_owned())
}

fn session_error(reason: &DenyReason) -> ErrorData {
    let code = match reason {
        DenyReason::AuditUnavailable => DenyReason::EngineUnavailable.code(),
        r => r.code(),
    };
    ErrorData::new(SESSION_ERROR, code, Some(json!({ "code": code })))
}

fn meta_str(meta: &JsonObject, key: &str) -> Option<String> {
    meta.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Untrusted correlation from `_meta`; the pipeline truncates and records it
/// apart from trusted fields.
fn call_meta(req: &CallToolRequestParams, ctx: &RequestContext<RoleServer>) -> CallMeta {
    let mut meta = ctx.meta.0 .0.clone();
    if let Some(m) = &req.meta {
        meta.extend(m.0 .0.clone());
    }
    let client_info = ctx
        .peer
        .peer_info()
        .and_then(|i| serde_json::to_value(&i.client_info).ok());
    CallMeta {
        approval_id: meta_str(&meta, "approval_id"),
        correlation: UntrustedCorrelation {
            tool_use_id: meta_str(&meta, "tool_use_id"),
            client_request_id: meta_str(&meta, "client_request_id"),
            requested_tool: None,
            client_info,
        },
    }
}

/// `CallOutcome` → MCP result (MILESTONE_18 §10.2 step 8). `_meta` carries
/// the decision id and the audit head for anchoring.
pub(crate) fn to_result(outcome: CallOutcome) -> CallToolResult {
    let mut meta = MetaObject::new();
    meta.0
        .insert("decision_id".into(), outcome.decision_id.as_str().into());
    if let Some(h) = &outcome.audit_head {
        meta.0.insert(
            "audit_head".into(),
            json!({ "seq": h.seq, "head_hash": h.head_hash }),
        );
    }
    let code = outcome.code();
    let mut result = match outcome.result {
        CallResult::Executed { content } => {
            let mut r = CallToolResult::success(vec![ContentBlock::text(content.to_string())]);
            r.structured_content = Some(content);
            r
        }
        other => {
            let code = code.unwrap_or(DenyReason::EngineUnavailable.code());
            let mut r = CallToolResult::error(vec![ContentBlock::text(code)]);
            r.structured_content = serde_json::to_value(&other).ok();
            r
        }
    };
    result.meta = Some(meta);
    result
}

impl ServerHandler for AgentHandler {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("crom-guard", env!("CARGO_PKG_VERSION")))
    }

    /// Only the operations every grant in the caller's chain lists, with
    /// metadata from the registry alone.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        ctx: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let s = &self.shared;
        if s.guard.is_failed_closed() {
            return Err(session_error(&DenyReason::EngineUnavailable));
        }
        let bearer = bearer(&ctx);
        let Ok(handle) = SessionHandle::parse(&bearer) else {
            return Err(session_error(&DenyReason::NoGrant));
        };
        let exec = s
            .store
            .authenticate(&handle, s.clock.now())
            .await
            .map_err(|r| session_error(&r))?;
        let tools = s
            .guard
            .registry()
            .tools()
            .filter(|t| exec.chain().iter().all(|g| g.actions().contains(t.name)))
            .map(|t| {
                let schema = t.input_schema.as_object().cloned().unwrap_or_default();
                Tool::new(
                    t.name.as_str().to_owned(),
                    t.description.to_owned(),
                    Arc::new(schema),
                )
            })
            .collect();
        let mut result = ListToolsResult::with_all_items(tools);
        // Per-session: never cacheable across callers.
        result.cache_scope = Some(CacheScope::Private);
        result.ttl_ms = Some(0);
        Ok(result)
    }

    /// Runs `Guard::call` on its own task and awaits it. Cancellation or a
    /// client disconnect drops only this wrapper; the call still completes,
    /// so Guard never latches closed for an agent-side cancel
    /// (`docs/guard/pipeline.md`, "Cancellation").
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        ctx: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let s = self.shared.clone();
        let mut bearer = bearer(&ctx);
        if s.admin_token.verify(&bearer) {
            // Never usable here; the pipeline then denies it as `no_grant`.
            tracing::warn!("admin credential presented to the agent endpoint; rejected");
            bearer = Zeroizing::new(String::new());
        }
        let meta = call_meta(&request, &ctx);
        let args = Value::Object(request.arguments.unwrap_or_default());
        let tool = request.name.into_owned();
        let task = s.calls.spawn({
            let s = s.clone();
            async move { s.guard.call(&bearer, &tool, args, meta).await }
        });
        match task.await {
            Ok(outcome) => Ok(to_result(outcome).into()),
            // Only reachable if the runtime is shutting down under the call.
            Err(_) => Err(session_error(&DenyReason::EngineUnavailable)),
        }
    }
}
