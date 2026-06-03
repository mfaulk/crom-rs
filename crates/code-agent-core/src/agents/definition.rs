use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::permission::PermissionMode;
use crate::tool_catalog::ToolSearchOptions;
use crate::tool_rule::ToolRule;

use super::errors::AgentDefinitionError;

/// Default cap on `max_turns` for a subagent. Definitions may set a lower
/// number, but anything above [`MAX_TURNS_HARD_LIMIT`] is rejected.
pub const DEFAULT_MAX_TURNS: u32 = 8;
/// Hard cap on `max_turns` enforced at validation time.
pub const MAX_TURNS_HARD_LIMIT: u32 = 50;
/// Maximum length of the agent name. 64 chars is plenty for human-readable
/// kebab/snake names and short enough to keep paths well under filesystem
/// limits.
pub const MAX_AGENT_NAME_LEN: usize = 64;

/// What format the subagent's *final* result should take.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentOutputMode {
    /// Plain prose. The default.
    #[default]
    Text,
    /// Markdown-formatted prose.
    Markdown,
    /// JSON conforming to the supplied schema. The runtime does not enforce
    /// the schema in M6 — the field is metadata for downstream consumers.
    JsonSchema { schema: serde_json::Value },
}

/// Hint to the orchestrator about whether this agent is safe to run in
/// parallel with other invocations of itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AgentConcurrencyHint {
    /// Agent does work that should not overlap with another instance (e.g.
    /// touches a shared file the registry cannot scope).
    SequentialOnly,
    /// Agent is read-only or otherwise safe to fan out.
    #[default]
    ParallelSafe,
}

/// A typed subagent definition. Loaded programmatically or from
/// `.agent/agents/<name>.md`.
#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub prompt: String,
    /// Allow rules. `None` means "inherit parent's allow set".
    pub tools: Option<Vec<ToolRule>>,
    pub disallowed_tools: Vec<ToolRule>,
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    pub permission_mode: Option<PermissionMode>,
    /// Names of MCP servers the subagent may speak to. `None` means
    /// "inherit parent's MCP scope (only if the runtime is configured to
    /// inherit at all)". An empty list explicitly grants no MCP servers.
    pub mcp_servers: Option<Vec<String>>,
    pub tool_search: Option<ToolSearchOptions>,
    pub checkpointing: Option<super::runtime::SubagentCheckpointPolicy>,
    pub validation: Option<crate::settings::ValidationConfig>,
    pub output_mode: AgentOutputMode,
    pub concurrency: AgentConcurrencyHint,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            prompt: String::new(),
            tools: None,
            disallowed_tools: Vec::new(),
            model: None,
            max_turns: Some(DEFAULT_MAX_TURNS),
            permission_mode: None,
            mcp_servers: None,
            tool_search: None,
            checkpointing: None,
            validation: None,
            output_mode: AgentOutputMode::default(),
            concurrency: AgentConcurrencyHint::default(),
        }
    }
}

impl AgentDefinition {
    /// Validate this definition without consulting the live tool catalog.
    /// Pure structural checks: name shape, required strings, max_turns
    /// bounds, no-recursive-Agent.
    pub fn validate_structural(&self) -> Result<(), AgentDefinitionError> {
        if self.name.is_empty() {
            return Err(AgentDefinitionError::MissingName);
        }
        if !is_identifier_safe_agent_name(&self.name) {
            return Err(AgentDefinitionError::UnsafeName {
                name: self.name.clone(),
            });
        }
        if self.description.trim().is_empty() {
            return Err(AgentDefinitionError::MissingDescription {
                name: self.name.clone(),
            });
        }
        if self.prompt.trim().is_empty() {
            return Err(AgentDefinitionError::MissingPrompt {
                name: self.name.clone(),
            });
        }
        if let Some(t) = self.max_turns {
            if !(1..=MAX_TURNS_HARD_LIMIT).contains(&t) {
                return Err(AgentDefinitionError::MaxTurnsOutOfRange {
                    name: self.name.clone(),
                    actual: t,
                    max: MAX_TURNS_HARD_LIMIT,
                });
            }
        }
        if let Some(allowed) = &self.tools {
            if any_rule_grants_agent_tool(allowed) {
                return Err(AgentDefinitionError::RecursiveAgentTool {
                    name: self.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// Effective `max_turns`, applying the documented default of 8 when
    /// unset.
    pub fn effective_max_turns(&self) -> u32 {
        self.max_turns.unwrap_or(DEFAULT_MAX_TURNS)
    }

    /// Project a compact summary (used for `agents list` and registry-loaded
    /// events).
    pub fn summary(&self) -> AgentDefinitionSummary {
        AgentDefinitionSummary {
            name: self.name.clone(),
            description: self.description.clone(),
            model: self.model.clone(),
            max_turns: self.max_turns,
            tool_count: self.tools.as_ref().map(Vec::len),
            disallowed_count: self.disallowed_tools.len(),
            permission_mode: self.permission_mode,
        }
    }
}

/// Compact projection of an [`AgentDefinition`] used for listing and event
/// payloads. Excludes the full prompt body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefinitionSummary {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_count: Option<usize>,
    #[serde(default)]
    pub disallowed_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

/// Returns true iff `name` is safe to use as a CLI argument, filesystem
/// component, and policy key. The allowlist is intentionally tight: lower-
/// case ASCII letters, digits, hyphen, and underscore. Length is bounded.
///
/// A single `:` is permitted between two identifier-safe segments to
/// support plugin-namespaced names like `rust-quality:security-reviewer`.
/// Both segments must be non-empty and identifier-safe.
pub fn is_identifier_safe_agent_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_AGENT_NAME_LEN {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    if let Some((ns, base)) = name.split_once(':') {
        if ns.is_empty() || base.is_empty() || base.contains(':') {
            return false;
        }
        return is_plain_agent_segment(ns) && is_plain_agent_segment(base);
    }
    is_plain_agent_segment(name)
}

fn is_plain_agent_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment
            .chars()
            .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_'))
}

/// Look at a list of tool-rules and decide whether any of them would grant
/// access to the built-in `Agent` tool (used to enforce the no-recursive-
/// subagent rule in M6).
pub fn any_rule_grants_agent_tool(rules: &[ToolRule]) -> bool {
    use globset::Glob;
    for r in rules {
        match r {
            ToolRule::ExactTool(n) if n == super::tool::SUBAGENT_TOOL_NAME => return true,
            ToolRule::ToolGlob(g) => {
                if let Ok(glob) = Glob::new(g) {
                    if glob
                        .compile_matcher()
                        .is_match(super::tool::SUBAGENT_TOOL_NAME)
                    {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Returns the list of tool names referenced by a definition's allow/disallow
/// rules (only `ExactTool` and explicit MCP names — globs are not enumerated
/// here). Used to surface "unknown tool" warnings against a [`crate::ToolCatalog`].
pub fn referenced_tool_names(def: &AgentDefinition) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    if let Some(allowed) = &def.tools {
        for r in allowed {
            if let ToolRule::ExactTool(n) = r {
                names.insert(n.clone());
            }
        }
    }
    for r in &def.disallowed_tools {
        if let ToolRule::ExactTool(n) = r {
            names.insert(n.clone());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.to_string(),
            description: "desc".to_string(),
            prompt: "you are helpful".to_string(),
            ..Default::default()
        }
    }

    #[test]
    // A definition with name, description, and prompt validates cleanly.
    fn structural_validate_passes() {
        let d = make("security-reviewer");
        d.validate_structural().expect("should validate");
    }

    #[test]
    // Empty names are rejected.
    fn empty_name_rejected() {
        let d = make("");
        let err = d.validate_structural().expect_err("must reject empty name");
        assert!(matches!(err, AgentDefinitionError::MissingName));
    }

    #[test]
    // Names with disallowed characters are rejected.
    fn unsafe_names_rejected() {
        for bad in &[
            "Up",
            "../etc",
            "with space",
            "../foo",
            "..",
            ".",
            "name/with/slash",
            "name;rm-rf",
            "name`backtick",
            "name\\backslash",
            "name with space",
        ] {
            let d = make(bad);
            let err = d.validate_structural().expect_err("must reject");
            assert!(
                matches!(err, AgentDefinitionError::UnsafeName { .. }),
                "expected UnsafeName for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    // Names longer than 64 chars are rejected.
    fn overlong_name_rejected() {
        let d = make(&"a".repeat(65));
        let err = d.validate_structural().expect_err("must reject overlong");
        assert!(matches!(err, AgentDefinitionError::UnsafeName { .. }));
    }

    #[test]
    // Missing description is rejected.
    fn missing_description_rejected() {
        let mut d = make("ok");
        d.description = String::new();
        let err = d
            .validate_structural()
            .expect_err("must reject missing desc");
        assert!(matches!(
            err,
            AgentDefinitionError::MissingDescription { .. }
        ));
    }

    #[test]
    // Missing prompt is rejected.
    fn missing_prompt_rejected() {
        let mut d = make("ok");
        d.prompt = String::new();
        let err = d
            .validate_structural()
            .expect_err("must reject missing prompt");
        assert!(matches!(err, AgentDefinitionError::MissingPrompt { .. }));
    }

    #[test]
    // max_turns must be 1..=50.
    fn max_turns_range_enforced() {
        let mut d = make("ok");
        d.max_turns = Some(0);
        assert!(matches!(
            d.validate_structural().unwrap_err(),
            AgentDefinitionError::MaxTurnsOutOfRange { .. }
        ));
        let mut d = make("ok");
        d.max_turns = Some(51);
        assert!(matches!(
            d.validate_structural().unwrap_err(),
            AgentDefinitionError::MaxTurnsOutOfRange { .. }
        ));
        let mut d = make("ok");
        d.max_turns = Some(50);
        assert!(d.validate_structural().is_ok());
    }

    #[test]
    // A subagent that lists `Agent` in its allowed tools is rejected up front
    // so M6 can guarantee no recursive subagents.
    fn recursive_agent_rejected() {
        let mut d = make("ok");
        d.tools = Some(vec![ToolRule::ExactTool("Agent".into())]);
        let err = d
            .validate_structural()
            .expect_err("recursive Agent must be rejected");
        assert!(matches!(
            err,
            AgentDefinitionError::RecursiveAgentTool { .. }
        ));
    }

    #[test]
    // A glob that would match `Agent` (e.g. `A*`) is also rejected.
    fn recursive_agent_glob_rejected() {
        let mut d = make("ok");
        d.tools = Some(vec![ToolRule::try_tool_glob("Age*").unwrap()]);
        let err = d.validate_structural().expect_err("must reject");
        assert!(matches!(
            err,
            AgentDefinitionError::RecursiveAgentTool { .. }
        ));
    }

    #[test]
    // `effective_max_turns` falls back to 8 when unset.
    fn effective_max_turns_defaults_to_eight() {
        let mut d = make("ok");
        d.max_turns = None;
        assert_eq!(d.effective_max_turns(), 8);
        d.max_turns = Some(20);
        assert_eq!(d.effective_max_turns(), 20);
    }

    #[test]
    // The summary projection drops the prompt body.
    fn summary_projects_compact_view() {
        let d = make("rev");
        let s = d.summary();
        assert_eq!(s.name, "rev");
        assert_eq!(s.description, "desc");
        assert_eq!(s.tool_count, None);
    }
}
