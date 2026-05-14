//! Phase F read-only snapshots of the in-process registries the side
//! panels expose: tools, MCP servers, subagents, skills, plugins.
//!
//! The reducer remains pure — it never touches the filesystem or runs
//! discovery. The host (CLI) builds these snapshots from the real core
//! registries before constructing `TuiAppState` and hands them in via
//! [`crate::state::TuiAppState::with_inspectors`]. Re-discovery (e.g.
//! after a `/plugins reload` slash command) is a Phase H+ concern; for
//! now the snapshot is immutable for the lifetime of a TUI session.
//!
//! Records here use plain `String` / number fields instead of importing
//! from `code-agent-core` / `code-agent-mcp` so the renderer crate stays
//! independent of those crates' enum/path types.

/// One row in the Tools inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorTool {
    pub name: String,
    pub description: String,
    /// Human-readable permission classification (e.g. `read_only`,
    /// `write_fs`, `execute_bash`). Comes from
    /// `code_agent_core::permission::PermissionKind` via the host.
    pub permission: String,
}

/// One row in the MCP servers inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorMcp {
    pub name: String,
    /// Transport label: `stdio` or `http`.
    pub transport: String,
    /// Redacted endpoint string. For Stdio this is the command basename;
    /// for HTTP it is the URL (host-only is fine, but the full URL is
    /// also acceptable since secrets ride in headers/env which are
    /// redacted separately).
    pub endpoint: String,
    /// Best-effort count of env entries (stdio) or header entries (http)
    /// so the inspector can show "(7 env vars)" without re-fetching.
    pub config_entries: usize,
}

/// One row in the Subagents inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSubagent {
    pub name: String,
    pub description: String,
    /// `None` means the subagent inherits the parent model.
    pub model: Option<String>,
    pub max_turns: Option<u32>,
    /// Count of explicit tool-allow rules; `None` means the subagent
    /// inherits the parent's tool set.
    pub tool_count: Option<usize>,
}

/// One row in the Skills inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorSkill {
    pub name: String,
    pub description: String,
    /// Source label: `project`, `user`, `built_in`, `compat_claude`.
    pub source: String,
    pub resource_count: usize,
}

/// One row in the Plugins inspector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InspectorPlugin {
    pub name: String,
    pub version: Option<String>,
    /// Scope label: `user`, `project`, `local`, `managed`.
    pub scope: String,
    pub enabled: bool,
    /// Status label: `loaded`, `disabled`, `error: <reason>`.
    pub status: String,
    /// Total components (skills + commands + agents + hooks + mcp_servers + bin entries).
    pub component_total: usize,
}

/// Snapshot of every inspector category. Empty by default; the host
/// populates the lists it can resolve at startup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspectors {
    pub tools: Vec<InspectorTool>,
    pub mcp: Vec<InspectorMcp>,
    pub subagents: Vec<InspectorSubagent>,
    pub skills: Vec<InspectorSkill>,
    pub plugins: Vec<InspectorPlugin>,
}

impl Inspectors {
    /// Convenience constructor for an empty snapshot. Same as
    /// `Inspectors::default()` — exposed so the intent is clearer at
    /// call sites.
    pub fn empty() -> Self {
        Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Default Inspectors snapshot has every category empty.
    #[test]
    fn default_is_empty() {
        let i = Inspectors::default();
        assert!(i.tools.is_empty());
        assert!(i.mcp.is_empty());
        assert!(i.subagents.is_empty());
        assert!(i.skills.is_empty());
        assert!(i.plugins.is_empty());
    }

    // Inspector record defaults produce empty strings + zero counts.
    #[test]
    fn record_defaults_are_blank() {
        let t = InspectorTool::default();
        assert!(t.name.is_empty());
        assert!(t.description.is_empty());
        assert!(t.permission.is_empty());
        let m = InspectorMcp::default();
        assert_eq!(m.config_entries, 0);
        let s = InspectorSubagent::default();
        assert!(s.tool_count.is_none());
        let sk = InspectorSkill::default();
        assert_eq!(sk.resource_count, 0);
        let p = InspectorPlugin::default();
        assert!(!p.enabled);
        assert_eq!(p.component_total, 0);
    }
}
