use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::ToolError;
use crate::options::SessionId;
use crate::permission::PermissionKind;
use crate::run_id::RunId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub content: Vec<ToolContent>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl ToolResult {
    pub fn ok_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            is_error: false,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn error_text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolContent::Text(text.into())],
            is_error: true,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn flatten_text(&self) -> String {
        let mut s = String::new();
        for c in &self.content {
            match c {
                ToolContent::Text(t) => {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str(t);
                }
            }
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolContent {
    Text(String),
}

/// Per-call context provided to tool execution.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub session_id: SessionId,
    /// Run id of the parent agent loop. `None` for ad-hoc tool invocations
    /// outside the loop (CLI-direct calls). M6 uses this to link subagent
    /// runs back to the parent run.
    pub run_id: Option<RunId>,
    /// Zero-indexed turn within the parent run (0 if unknown). M6 uses
    /// this for subagent telemetry; existing tools may ignore it.
    pub turn_index: u32,
    /// Identifier of the tool call this `ToolContext` was constructed for.
    /// `None` for tool invocations outside a turn loop. M6 uses this to
    /// link a subagent run to a specific Agent-tool call in the parent.
    pub tool_call_id: Option<String>,
}

impl ToolContext {
    /// Convenience constructor used by call sites that do not have a parent
    /// run id or tool call id (e.g. tests, ad-hoc invocations).
    pub fn new_minimal(cwd: PathBuf, session_id: SessionId) -> Self {
        Self {
            cwd,
            session_id,
            run_id: None,
            turn_index: 0,
            tool_call_id: None,
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> serde_json::Value;
    fn permission_kind(&self) -> PermissionKind;

    /// Build a short, human-readable summary of an invocation for approval prompts.
    fn approval_summary(&self, input: &serde_json::Value) -> String {
        format!("{}({})", self.name(), input)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError>;
}

/// Tools are stored in a `BTreeMap` so iteration order is lexicographic by
/// name. The Anthropic API caches the `tools` array as part of the prompt
/// prefix; if the order varies across resumed or forked sessions the cache
/// is invalidated, which raises cost and lowers hit rate. Lookups remain
/// O(log n), which is negligible for the small tool sets we register.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<String> {
        // BTreeMap keys already iterate in lexicographic order.
        self.tools.keys().cloned().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = Arc<dyn Tool>> + '_ {
        self.tools.values().cloned()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Filter to a subset of tools by name. Unknown names are silently ignored.
    pub fn filter(&self, allowed: &[String]) -> Self {
        let mut out = ToolRegistry::new();
        for n in allowed {
            if n.contains('*') || n.contains('?') {
                if let Ok(glob) = globset::Glob::new(n) {
                    let matcher = glob.compile_matcher();
                    for (name, tool) in &self.tools {
                        if matcher.is_match(name) {
                            out.register(tool.clone());
                        }
                    }
                }
            } else if let Some(t) = self.get(n) {
                out.register(t);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::ToolError;
    use crate::permission::PermissionKind;

    struct StubTool {
        name: String,
    }

    #[async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn permission_kind(&self) -> PermissionKind {
            PermissionKind::ReadOnly
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: ToolContext,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::ok_text(""))
        }
    }

    fn stub(name: &str) -> Arc<dyn Tool> {
        Arc::new(StubTool {
            name: name.to_string(),
        })
    }

    /// Registry::iter yields tools in lexicographic order regardless of insertion order.
    #[test]
    fn iter_returns_tools_in_lexicographic_order() {
        let mut registry = ToolRegistry::new();
        for n in ["zebra", "alpha", "mango", "bravo"] {
            registry.register(stub(n));
        }
        let observed: Vec<String> = registry.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(
            observed,
            vec![
                "alpha".to_string(),
                "bravo".to_string(),
                "mango".to_string(),
                "zebra".to_string(),
            ],
            "ToolRegistry::iter must yield tools in lexicographic name order so the \
             Anthropic tools array stays stable across resumed/forked sessions"
        );
    }

    /// Registry::iter ordering equals its own sorted copy (direct invariant — no
    /// reliance on coincidental HashMap reordering to detect regressions).
    #[test]
    fn iter_order_equals_sorted_copy() {
        let mut registry = ToolRegistry::new();
        for n in [
            "zebra", "alpha", "mango", "bravo", "yak", "charlie", "tango", "delta",
        ] {
            registry.register(stub(n));
        }
        let observed: Vec<String> = registry.iter().map(|t| t.name().to_string()).collect();
        let mut sorted = observed.clone();
        sorted.sort();
        assert_eq!(
            observed, sorted,
            "ToolRegistry::iter must be in sorted order; any future change that \
             reintroduces unordered iteration will fail this invariant deterministically"
        );
    }

    /// Registry::names equals its own sorted copy — the catalog seen by the
    /// agent for cache-stable prompt prefixes is itself in sorted order.
    #[test]
    fn names_order_equals_sorted_copy() {
        let mut registry = ToolRegistry::new();
        for n in ["zebra", "alpha", "mango", "bravo", "yak", "charlie"] {
            registry.register(stub(n));
        }
        let observed = registry.names();
        let mut sorted = observed.clone();
        sorted.sort();
        assert_eq!(observed, sorted);
    }

    /// Registry::iter ordering is stable across distinct instances built in different insertion orders.
    #[test]
    fn iter_ordering_is_stable_across_instances() {
        let mut a = ToolRegistry::new();
        for n in ["zebra", "alpha", "mango", "bravo"] {
            a.register(stub(n));
        }
        let mut b = ToolRegistry::new();
        for n in ["mango", "bravo", "zebra", "alpha"] {
            b.register(stub(n));
        }
        let a_names: Vec<String> = a.iter().map(|t| t.name().to_string()).collect();
        let b_names: Vec<String> = b.iter().map(|t| t.name().to_string()).collect();
        assert_eq!(a_names, b_names);
    }
}
