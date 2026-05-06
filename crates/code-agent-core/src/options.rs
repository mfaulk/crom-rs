use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::agents::{AgentRegistry, AgentRuntimeOptions};
use crate::permission::{ApprovalMemoryMode, PermissionCallback, PermissionMode};
use crate::plugins::PluginRegistry;
use crate::skills::{SkillDiscoveryOptions, SkillRegistry};
use crate::slash::SlashCommandRegistry;
use crate::tool_catalog::ToolSearchOptions;
use crate::tool_rule::ToolRule;
use code_agent_mcp::McpOptions;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns true iff the id is safe to use as a filesystem path
    /// component: non-empty, only `[A-Za-z0-9_-]`, no path separators
    /// or `..`. UUID v4 ids satisfy this. Used to defend against
    /// path-traversal via user-supplied `--session-id`.
    pub fn is_valid_format(&self) -> bool {
        is_valid_session_id_str(&self.0)
    }
}

/// Windows reserved device basenames. Even with a `.jsonl` extension these
/// resolve to special device paths on Windows, not regular files. We reject
/// them here so the same validation contract holds across platforms.
const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Returns true iff `s` is a syntactically valid session id.
/// Accepts only `[A-Za-z0-9_-]+`, rejects empty strings, and rejects
/// Windows reserved device basenames (case-insensitive).
pub fn is_valid_session_id_str(s: &str) -> bool {
    if s.is_empty()
        || !s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return false;
    }
    let upper = s.to_ascii_uppercase();
    !WINDOWS_RESERVED_NAMES.iter().any(|r| *r == upper)
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct AgentOptions {
    pub cwd: PathBuf,
    pub model: String,
    /// Tool names allowed by simple-name matching (M1 compatibility).
    /// Empty means: register all known tools.
    pub allowed_tools: Vec<String>,
    /// Structured allow rules. Empty means: rely on `allowed_tools` only.
    pub allowed_tool_rules: Vec<ToolRule>,
    /// Structured disallow rules.
    pub disallowed_tool_rules: Vec<ToolRule>,
    pub permission_mode: PermissionMode,
    pub max_turns: Option<u32>,
    pub session_id: Option<SessionId>,
    pub permission_callback: Option<Arc<dyn PermissionCallback>>,
    pub system_prompt: Option<String>,
    pub project_instructions: Option<String>,
    pub max_tokens: u32,
    pub approval_memory: ApprovalMemoryMode,
    pub mcp: McpOptions,
    pub tool_search: ToolSearchOptions,
    /// Subagent registry (M6). Empty by default; the `Agent` tool is only
    /// surfaced when this contains at least one definition AND
    /// `agent_runtime.enabled` is true AND `Agent` is in `allowed_tools`
    /// or an explicit allow rule.
    pub agents: AgentRegistry,
    /// Subagent runtime knobs (M6).
    pub agent_runtime: AgentRuntimeOptions,
    /// Skill registry (M7). Empty by default; the `Skill` tool is only
    /// surfaced when this contains at least one definition AND `Skill` is
    /// in `allowed_tools` (or an explicit allow rule).
    pub skills: SkillRegistry,
    /// Skill discovery / safety knobs (M7).
    pub skill_options: SkillDiscoveryOptions,
    /// Slash-command registry (M7). Built-ins are enabled by default;
    /// custom command files are loaded via the SDK builder or CLI flags.
    pub slash_commands: SlashCommandRegistry,
    /// Plugin registry (M8). Empty by default. When non-empty, plugin
    /// summaries appear in `SystemInit` and plugin-derived components
    /// appear in their respective registries above.
    pub plugins: PluginRegistry,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            model: "claude-sonnet-4-6".to_string(),
            allowed_tools: vec![
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
                "Edit".to_string(),
                "Write".to_string(),
                "Bash".to_string(),
                "TodoWrite".to_string(),
                "SearchTools".to_string(),
            ],
            allowed_tool_rules: Vec::new(),
            disallowed_tool_rules: Vec::new(),
            permission_mode: PermissionMode::Default,
            max_turns: Some(8),
            session_id: None,
            permission_callback: None,
            system_prompt: None,
            project_instructions: None,
            max_tokens: 4096,
            approval_memory: ApprovalMemoryMode::Session,
            mcp: McpOptions::default(),
            tool_search: ToolSearchOptions::default(),
            agents: AgentRegistry::default(),
            agent_runtime: AgentRuntimeOptions::default(),
            skills: SkillRegistry::default(),
            skill_options: SkillDiscoveryOptions::default(),
            slash_commands: SlashCommandRegistry::new(true),
            plugins: PluginRegistry::default(),
        }
    }
}

impl std::fmt::Debug for AgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentOptions")
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("allowed_tools", &self.allowed_tools)
            .field("allowed_tool_rules", &self.allowed_tool_rules)
            .field("disallowed_tool_rules", &self.disallowed_tool_rules)
            .field("permission_mode", &self.permission_mode)
            .field("max_turns", &self.max_turns)
            .field("session_id", &self.session_id)
            .field("max_tokens", &self.max_tokens)
            .field("approval_memory", &self.approval_memory)
            .field("mcp_enabled", &self.mcp.enabled)
            .field("tool_search", &self.tool_search)
            .field("agents_count", &self.agents.len())
            .field("agent_runtime_enabled", &self.agent_runtime.enabled)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // A freshly generated UUID v4 session id is recognized as valid.
    fn uuid_v4_is_valid() {
        let id = SessionId::new();
        assert!(id.is_valid_format(), "uuid id should be valid: {id}");
    }

    #[test]
    // Plain ASCII alphanumeric, dash, and underscore ids are valid.
    fn alnum_dash_underscore_is_valid() {
        assert!(SessionId::from("abc123".to_string()).is_valid_format());
        assert!(SessionId::from("a-b_c-1".to_string()).is_valid_format());
        assert!(SessionId::from("Z".to_string()).is_valid_format());
    }

    #[test]
    // A bare `..` is rejected as a session id.
    fn dotdot_is_rejected() {
        assert!(!SessionId::from("..".to_string()).is_valid_format());
    }

    #[test]
    // Path separators in a session id are rejected.
    fn slash_is_rejected() {
        assert!(!SessionId::from("foo/bar".to_string()).is_valid_format());
        assert!(!SessionId::from("../etc/passwd".to_string()).is_valid_format());
    }

    #[test]
    // Absolute Unix paths are rejected as session ids.
    fn absolute_path_is_rejected() {
        assert!(!SessionId::from("/etc/passwd".to_string()).is_valid_format());
    }

    #[test]
    // Empty session ids are rejected.
    fn empty_is_rejected() {
        assert!(!SessionId::from(String::new()).is_valid_format());
    }

    #[test]
    // A single dot is rejected because `.` is not in [A-Za-z0-9_-].
    fn dot_is_rejected() {
        assert!(!SessionId::from(".".to_string()).is_valid_format());
        assert!(!SessionId::from("a.b".to_string()).is_valid_format());
    }

    #[test]
    // Windows reserved device basenames are rejected case-insensitively
    // even though they would otherwise pass the alphanumeric check —
    // `NUL.jsonl` resolves to the null device on Windows, not a file.
    fn windows_reserved_names_rejected() {
        for name in &[
            "NUL", "nul", "Nul", "CON", "con", "PRN", "AUX", "COM1", "lpt9",
        ] {
            assert!(
                !is_valid_session_id_str(name),
                "{name} should be rejected as a Windows reserved name"
            );
        }
    }

    #[test]
    // Names that *contain* a reserved substring but aren't equal to one are fine.
    fn names_containing_reserved_substring_accepted() {
        assert!(is_valid_session_id_str("CONversation"));
        assert!(is_valid_session_id_str("my-NUL-id"));
        assert!(is_valid_session_id_str("nullable"));
    }
}
