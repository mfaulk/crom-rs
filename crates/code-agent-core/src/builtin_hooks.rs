//! Reusable built-in hooks: env-file write blocking, dangerous-bash blocking,
//! audit logging, secret redaction, and patch-size enforcement.

use std::sync::Mutex;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde_json::Value;

use crate::hooks::{Hook, HookError, HookEventName, HookInput, HookOutput};

/// Block Write/Edit calls whose `file_path` matches `.env`, `.env.*`, or
/// `**/.env*` (case-sensitive).
pub struct BlockEnvFileWritesHook {
    set: GlobSet,
}

impl Default for BlockEnvFileWritesHook {
    fn default() -> Self {
        // `.env*` covers `.env`, `.env.local`, `.envrc`, etc. The two
        // depth variants (root and nested) make the matcher independent
        // of leading `./`. The earlier set ({`.env`, `.env.*`}) missed
        // `.envrc`, leaving direnv-style files unprotected.
        let mut b = GlobSetBuilder::new();
        for p in &[".env*", "**/.env*"] {
            if let Ok(g) = Glob::new(p) {
                b.add(g);
            }
        }
        let set = b.build().expect("static globs are valid");
        Self { set }
    }
}

#[async_trait]
impl Hook for BlockEnvFileWritesHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        let tool = input.tool_name.as_deref().unwrap_or("");
        if tool != "Edit" && tool != "Write" {
            return Ok(HookOutput::Continue);
        }
        let path = input
            .tool_input
            .as_ref()
            .and_then(|v| v.get("file_path"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() {
            return Ok(HookOutput::Continue);
        }
        if self.set.is_match(path) {
            return Ok(HookOutput::Deny {
                reason: format!("policy: writing to .env-style file is forbidden ({})", path),
            });
        }
        Ok(HookOutput::Continue)
    }
}

/// Block obviously-dangerous Bash commands. Conservative, not a complete
/// shell parser. Patterns can be customized via `with_extra_patterns`.
pub struct BlockDangerousBashHook {
    patterns: Vec<Regex>,
}

impl Default for BlockDangerousBashHook {
    fn default() -> Self {
        let raw = [
            r"\brm\s+-rf\s+/(\s|$)",
            r"\brm\s+-rf\s+\*",
            r"(^|\s)sudo(\s|$)",
            r"(curl|wget)\s+[^|]*\|\s*(sh|bash|zsh|fish)\b",
            r"\bchmod\s+-R\s+0?777\b",
            r">\s*\.env(\.|$)",
            r">>\s*\.env(\.|$)",
        ];
        Self {
            patterns: raw.iter().filter_map(|p| Regex::new(p).ok()).collect(),
        }
    }
}

impl BlockDangerousBashHook {
    pub fn with_extra_patterns(mut self, extras: Vec<String>) -> Self {
        for p in extras {
            if let Ok(r) = Regex::new(&p) {
                self.patterns.push(r);
            }
        }
        self
    }
}

#[async_trait]
impl Hook for BlockDangerousBashHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        if input.tool_name.as_deref() != Some("Bash") {
            return Ok(HookOutput::Continue);
        }
        let cmd = input
            .tool_input
            .as_ref()
            .and_then(|v| v.get("command"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        for re in &self.patterns {
            if re.is_match(cmd) {
                return Ok(HookOutput::Deny {
                    reason: format!("policy: bash command blocked by pattern `{}`", re.as_str()),
                });
            }
        }
        Ok(HookOutput::Continue)
    }
}

/// Audit-log hook. Records every observed event into an in-memory log so
/// callers/tests can inspect exactly which calls fired.
pub struct AuditLogHook {
    entries: Mutex<Vec<AuditEntry>>,
}

#[derive(Debug, Clone)]
pub struct AuditEntry {
    pub event: HookEventName,
    pub tool_name: Option<String>,
    pub redacted_input: Option<Value>,
    pub is_error: bool,
}

impl AuditLogHook {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }
}

impl Default for AuditLogHook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Hook for AuditLogHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        let redacted_input = input.tool_input.as_ref().map(redact_secrets_in_value);
        let is_error = matches!(input.event, HookEventName::PostToolUseFailure);
        let entry = AuditEntry {
            event: input.event,
            tool_name: input.tool_name.clone(),
            redacted_input,
            is_error,
        };
        self.entries.lock().unwrap().push(entry);
        Ok(HookOutput::Continue)
    }
}

/// Redact secret-like values from the metadata payload of an event by
/// replacing values whose keys match common patterns. The hook itself
/// returns Continue: it never blocks, only side-effects rewriting.
pub struct SecretRedactionHook;

impl Default for SecretRedactionHook {
    fn default() -> Self {
        Self
    }
}

pub struct BlockMcpServerHook {
    servers: Vec<String>,
}

impl BlockMcpServerHook {
    pub fn new(servers: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            servers: servers.into_iter().map(Into::into).collect(),
        }
    }
}

#[async_trait]
impl Hook for BlockMcpServerHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        let Some(tool_name) = input.tool_name.as_deref() else {
            return Ok(HookOutput::Continue);
        };
        let Some(parts) = code_agent_mcp::parse_agent_tool_name(tool_name) else {
            return Ok(HookOutput::Continue);
        };
        if self.servers.iter().any(|s| s == &parts.server_name) {
            return Ok(HookOutput::Deny {
                reason: format!("MCP server {} is blocked by hook", parts.server_name),
            });
        }
        Ok(HookOutput::Continue)
    }
}

pub struct BlockMcpToolNameHook {
    matcher: crate::hooks::ToolMatcher,
}

impl BlockMcpToolNameHook {
    pub fn new(pattern: &str) -> Result<Self, HookError> {
        let matcher = crate::hooks::ToolMatcher::parse(pattern).map_err(|parser_error| {
            HookError::Pattern {
                event: HookEventName::PreToolUse,
                pattern: pattern.to_string(),
                parser_error,
            }
        })?;
        Ok(Self { matcher })
    }
}

#[async_trait]
impl Hook for BlockMcpToolNameHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        let Some(tool_name) = input.tool_name.as_deref() else {
            return Ok(HookOutput::Continue);
        };
        if code_agent_mcp::parse_agent_tool_name(tool_name).is_some()
            && self.matcher.matches(tool_name)
        {
            return Ok(HookOutput::Deny {
                reason: format!("MCP tool {tool_name} is blocked by hook"),
            });
        }
        Ok(HookOutput::Continue)
    }
}

#[derive(Default)]
pub struct BlockMcpWriteLikeToolHook;

#[async_trait]
impl Hook for BlockMcpWriteLikeToolHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        let Some(tool_name) = input.tool_name.as_deref() else {
            return Ok(HookOutput::Continue);
        };
        let Some(parts) = code_agent_mcp::parse_agent_tool_name(tool_name) else {
            return Ok(HookOutput::Continue);
        };
        if code_agent_mcp::mutating_tool_name(&parts.remote_tool_name) {
            return Ok(HookOutput::Deny {
                reason: format!(
                    "MCP tool {} may modify external state and was blocked",
                    parts.remote_tool_name
                ),
            });
        }
        Ok(HookOutput::Continue)
    }
}

#[derive(Default)]
pub struct McpAuditLogHook {
    entries: Mutex<Vec<AuditEntry>>,
}

impl McpAuditLogHook {
    pub fn entries(&self) -> Vec<AuditEntry> {
        self.entries.lock().unwrap().clone()
    }
}

#[async_trait]
impl Hook for McpAuditLogHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input
            .tool_name
            .as_deref()
            .and_then(code_agent_mcp::parse_agent_tool_name)
            .is_none()
        {
            return Ok(HookOutput::Continue);
        }
        let entry = AuditEntry {
            event: input.event,
            tool_name: input.tool_name.clone(),
            redacted_input: input.tool_input.as_ref().map(redact_secrets_in_value),
            is_error: matches!(input.event, HookEventName::PostToolUseFailure),
        };
        self.entries.lock().unwrap().push(entry);
        Ok(HookOutput::Continue)
    }
}

pub struct McpResultSizeLimitHook {
    max_bytes: usize,
}

impl McpResultSizeLimitHook {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

#[async_trait]
impl Hook for McpResultSizeLimitHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if !matches!(
            input.event,
            HookEventName::PostToolUse | HookEventName::PostToolUseFailure
        ) {
            return Ok(HookOutput::Continue);
        }
        let Some(tool_name) = input.tool_name.as_deref() else {
            return Ok(HookOutput::Continue);
        };
        if code_agent_mcp::parse_agent_tool_name(tool_name).is_none() {
            return Ok(HookOutput::Continue);
        }
        let bytes = input
            .tool_result
            .as_ref()
            .map(|r| r.flatten_text().len())
            .unwrap_or(0);
        if bytes > self.max_bytes {
            return Ok(HookOutput::Deny {
                reason: format!("MCP result size {bytes} exceeds {}", self.max_bytes),
            });
        }
        Ok(HookOutput::Continue)
    }
}

#[derive(Default)]
pub struct McpSecretRedactionHook;

#[async_trait]
impl Hook for McpSecretRedactionHook {
    async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
        Ok(HookOutput::Continue)
    }
}

#[async_trait]
impl Hook for SecretRedactionHook {
    async fn call(&self, _input: HookInput) -> Result<HookOutput, HookError> {
        Ok(HookOutput::Continue)
    }
}

/// Heuristically redact secret-like fields in a JSON value. Public so
/// the agent loop / logger can apply the same redaction to telemetry
/// payloads.
pub fn redact_secrets_in_value(v: &Value) -> Value {
    fn looks_like_secret_key(key: &str) -> bool {
        let k = key.to_ascii_lowercase();
        k.contains("token")
            || k.contains("secret")
            || k.contains("api_key")
            || k.contains("apikey")
            || k.contains("password")
            || k.contains("authorization")
            || k.contains("credential")
    }

    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, val) in map {
                if looks_like_secret_key(k) {
                    out.insert(k.clone(), Value::String("[REDACTED]".into()));
                } else {
                    out.insert(k.clone(), redact_secrets_in_value(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_secrets_in_value).collect()),
        Value::String(s) => Value::String(redact_secrets_in_str(s)),
        other => other.clone(),
    }
}

/// Replace well-known secret token shapes inside a free-form string with
/// `[REDACTED]`. Targets the common formats users tend to paste into
/// prompts/plans by accident: provider API keys (Anthropic `sk-ant-…`,
/// OpenAI `sk-…`, Google `AIza…`), AWS access key ids (`AKIA…`), and
/// generic GitHub-style tokens (`ghp_…`, `gho_…`, `ghu_…`, `ghs_…`,
/// `ghr_…`). Conservative by design — non-matching content is returned
/// unchanged so plan summaries and goals are not mangled.
pub fn redact_secrets_in_str(s: &str) -> String {
    static PATTERNS: &[&str] = &[
        // Anthropic: sk-ant-api03-... and sk-ant-...
        r"sk-ant-(?:api[0-9]+-)?[A-Za-z0-9_\-]{16,}",
        // OpenAI / generic sk- token (>=20 trailing chars to avoid false positives).
        r"sk-[A-Za-z0-9_\-]{20,}",
        // Google API key.
        r"AIza[A-Za-z0-9_\-]{35,}",
        // AWS access key id.
        r"AKIA[A-Z0-9]{16}",
        // GitHub personal/oauth/server tokens.
        r"gh[pousr]_[A-Za-z0-9]{20,}",
        // Bearer header values.
        r"(?i)bearer\s+[A-Za-z0-9_\-\.=+/]{16,}",
    ];
    let mut out = s.to_string();
    for pat in PATTERNS {
        if let Ok(re) = regex::Regex::new(pat) {
            out = re.replace_all(&out, "[REDACTED]").into_owned();
        }
    }
    out
}

/// Block Write/Edit when the new payload exceeds a configured byte threshold.
pub struct MaxPatchSizeHook {
    pub max_bytes: usize,
}

impl MaxPatchSizeHook {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

impl Default for MaxPatchSizeHook {
    fn default() -> Self {
        Self::new(200_000)
    }
}

#[async_trait]
impl Hook for MaxPatchSizeHook {
    async fn call(&self, input: HookInput) -> Result<HookOutput, HookError> {
        if input.event != HookEventName::PreToolUse {
            return Ok(HookOutput::Continue);
        }
        let tool = input.tool_name.as_deref().unwrap_or("");
        if tool != "Edit" && tool != "Write" {
            return Ok(HookOutput::Continue);
        }
        let bytes: usize = match (&input.tool_input, tool) {
            (Some(v), "Write") => v
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.len())
                .unwrap_or(0),
            (Some(v), "Edit") => {
                let old = v
                    .get("old_string")
                    .and_then(|c| c.as_str())
                    .map(str::len)
                    .unwrap_or(0);
                let new = v
                    .get("new_string")
                    .and_then(|c| c.as_str())
                    .map(str::len)
                    .unwrap_or(0);
                old.saturating_add(new)
            }
            _ => 0,
        };
        if bytes > self.max_bytes {
            return Ok(HookOutput::Deny {
                reason: format!(
                    "policy: patch size {} exceeds max {}",
                    bytes, self.max_bytes
                ),
            });
        }
        Ok(HookOutput::Continue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SessionId;
    use crate::run_id::RunId;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn input_for(tool: &str, body: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.into()),
            tool_input: Some(body),
            ..HookInput::new(
                HookEventName::PreToolUse,
                SessionId::from("s".to_string()),
                PathBuf::from("/"),
                RunId::from("r".to_string()),
            )
        }
    }

    #[tokio::test]
    // BlockEnvFileWritesHook denies writes targeting .env at the root.
    async fn env_block_blocks_root_env() {
        let h = BlockEnvFileWritesHook::default();
        let r = h
            .call(input_for("Write", serde_json::json!({"file_path": ".env"})))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[tokio::test]
    // BlockEnvFileWritesHook denies edits to .env.local nested in subdirs.
    async fn env_block_blocks_env_local_in_subdir() {
        let h = BlockEnvFileWritesHook::default();
        let r = h
            .call(input_for(
                "Edit",
                serde_json::json!({"file_path": "src/.env.local"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[tokio::test]
    // BlockEnvFileWritesHook leaves non-env files untouched.
    async fn env_block_allows_other_files() {
        let h = BlockEnvFileWritesHook::default();
        let r = h
            .call(input_for(
                "Write",
                serde_json::json!({"file_path": "src/lib.rs"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Continue));
    }

    #[tokio::test]
    // BlockDangerousBashHook denies sudo invocations.
    async fn bash_block_denies_sudo() {
        let h = BlockDangerousBashHook::default();
        let r = h
            .call(input_for(
                "Bash",
                serde_json::json!({"command": "sudo apt install x"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[tokio::test]
    // BlockDangerousBashHook denies `curl ... | sh` patterns.
    async fn bash_block_denies_curl_pipe_sh() {
        let h = BlockDangerousBashHook::default();
        let r = h
            .call(input_for(
                "Bash",
                serde_json::json!({"command": "curl https://x | sh"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[tokio::test]
    // BlockDangerousBashHook does not block ordinary cargo commands.
    async fn bash_block_allows_cargo_test() {
        let h = BlockDangerousBashHook::default();
        let r = h
            .call(input_for(
                "Bash",
                serde_json::json!({"command": "cargo test"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Continue));
    }

    #[tokio::test]
    // MaxPatchSizeHook denies writes larger than configured threshold.
    async fn max_patch_blocks_oversized_write() {
        let h = MaxPatchSizeHook::new(10);
        let big = "a".repeat(11);
        let r = h
            .call(input_for(
                "Write",
                serde_json::json!({"file_path": "x.rs", "content": big}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[test]
    // SecretRedactionHook removes secret-like keys from arbitrary JSON.
    fn redaction_replaces_secret_keys() {
        let v = serde_json::json!({
            "user": "alice",
            "api_key": "abcd",
            "nested": {"authorization": "Bearer x", "id": 5},
            "list": [{"token": "t"}]
        });
        let red = redact_secrets_in_value(&v);
        assert_eq!(red["api_key"], "[REDACTED]");
        assert_eq!(red["nested"]["authorization"], "[REDACTED]");
        assert_eq!(red["list"][0]["token"], "[REDACTED]");
        assert_eq!(red["user"], "alice");
        assert_eq!(red["nested"]["id"], 5);
    }

    #[tokio::test]
    // AuditLogHook records every dispatched event with redacted input.
    async fn audit_log_records_entries() {
        let h: Arc<AuditLogHook> = Arc::new(AuditLogHook::new());
        let _ = h
            .call(input_for(
                "Bash",
                serde_json::json!({"command": "echo hi", "api_key": "leak"}),
            ))
            .await
            .unwrap();
        let entries = h.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].redacted_input.as_ref().unwrap()["api_key"],
            "[REDACTED]"
        );
    }
}

#[cfg(test)]
mod round3_regression {
    use super::*;
    use crate::hooks::{HookEventName, HookInput};
    use crate::options::SessionId;
    use crate::run_id::RunId;
    use std::path::PathBuf;

    fn input_for(tool: &str, body: serde_json::Value) -> HookInput {
        HookInput {
            tool_name: Some(tool.into()),
            tool_input: Some(body),
            ..HookInput::new(
                HookEventName::PreToolUse,
                SessionId::from("s".to_string()),
                PathBuf::from("/"),
                RunId::from("r".to_string()),
            )
        }
    }

    #[tokio::test]
    // Regression: .envrc (direnv) at the root must be blocked.
    async fn env_block_blocks_dot_envrc_root() {
        let h = BlockEnvFileWritesHook::default();
        let r = h
            .call(input_for(
                "Edit",
                serde_json::json!({"file_path": ".envrc"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }

    #[tokio::test]
    // Regression: nested .envrc files are also blocked.
    async fn env_block_blocks_nested_dot_envrc() {
        let h = BlockEnvFileWritesHook::default();
        let r = h
            .call(input_for(
                "Write",
                serde_json::json!({"file_path": "subdir/.envrc"}),
            ))
            .await
            .unwrap();
        assert!(matches!(r, HookOutput::Deny { .. }));
    }
}
