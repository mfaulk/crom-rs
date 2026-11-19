//! Side-effect, tool, and interactivity policy.
//!
//! Applications use these types to express what the SDK is allowed to do in
//! a given run. Defaults are conservative: read-only, no Bash, no mutation,
//! no plugins.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crom_core::Tool;
pub use crom_core::ToolRule;

use crate::provider::SecretString;

/// Highest side-effect level a tool may produce. Strict ordering (None <
/// WorkspaceRead < WorkspaceWrite < CommandExecution < ExternalNetwork <
/// ExternalMutation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectLevel {
    None,
    WorkspaceRead,
    WorkspaceWrite,
    CommandExecution,
    ExternalNetwork,
    ExternalMutation,
}

impl SideEffectLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WorkspaceRead => "workspace_read",
            Self::WorkspaceWrite => "workspace_write",
            Self::CommandExecution => "command_execution",
            Self::ExternalNetwork => "external_network",
            Self::ExternalMutation => "external_mutation",
        }
    }
}

/// Side-effect boundary policy.
///
/// `max_level` is the topmost level the SDK will allow. The boolean flags
/// give explicit yes/no for the most-asked categories so applications can
/// say "no Bash but yes external network" without weaving level math.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SideEffectPolicy {
    pub max_level: SideEffectLevel,
    pub allow_external_network: bool,
    pub allow_external_mutation: bool,
    pub allow_command_execution: bool,
    pub allow_workspace_write: bool,
}

impl SideEffectPolicy {
    pub fn none() -> Self {
        Self {
            max_level: SideEffectLevel::None,
            allow_external_network: false,
            allow_external_mutation: false,
            allow_command_execution: false,
            allow_workspace_write: false,
        }
    }

    pub fn read_only() -> Self {
        Self {
            max_level: SideEffectLevel::WorkspaceRead,
            allow_external_network: false,
            allow_external_mutation: false,
            allow_command_execution: false,
            allow_workspace_write: false,
        }
    }

    pub fn workspace_write() -> Self {
        Self {
            max_level: SideEffectLevel::WorkspaceWrite,
            allow_external_network: false,
            allow_external_mutation: false,
            allow_command_execution: false,
            allow_workspace_write: true,
        }
    }

    pub fn command_execution() -> Self {
        Self {
            max_level: SideEffectLevel::CommandExecution,
            allow_external_network: false,
            allow_external_mutation: false,
            allow_command_execution: true,
            allow_workspace_write: true,
        }
    }

    pub fn no_external_effects() -> Self {
        Self {
            max_level: SideEffectLevel::CommandExecution,
            allow_external_network: false,
            allow_external_mutation: false,
            allow_command_execution: true,
            allow_workspace_write: true,
        }
    }

    pub fn full() -> Self {
        Self {
            max_level: SideEffectLevel::ExternalMutation,
            allow_external_network: true,
            allow_external_mutation: true,
            allow_command_execution: true,
            allow_workspace_write: true,
        }
    }

    /// Decide whether a tool with the given `level` is permitted.
    pub fn allows(&self, level: SideEffectLevel) -> bool {
        if level > self.max_level {
            return false;
        }
        match level {
            SideEffectLevel::None | SideEffectLevel::WorkspaceRead => true,
            SideEffectLevel::WorkspaceWrite => self.allow_workspace_write,
            SideEffectLevel::CommandExecution => self.allow_command_execution,
            SideEffectLevel::ExternalNetwork => self.allow_external_network,
            SideEffectLevel::ExternalMutation => self.allow_external_mutation,
        }
    }
}

impl Default for SideEffectPolicy {
    fn default() -> Self {
        // Conservative SDK default: read-only.
        Self::read_only()
    }
}

/// Which built-in tools are enabled in a run. Defaults match the SDK's
/// safety story: only read-shaped tools.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BuiltinToolSet {
    pub read: bool,
    pub write: bool,
    pub edit: bool,
    pub glob: bool,
    pub grep: bool,
    pub bash: bool,
    pub todo: bool,
    pub search_tools: bool,
    pub agent: bool,
    pub skill: bool,
}

impl BuiltinToolSet {
    /// SDK-safe defaults: read/glob/grep/search-tools only.
    pub fn read_only_defaults() -> Self {
        Self {
            read: true,
            write: false,
            edit: false,
            glob: true,
            grep: true,
            bash: false,
            todo: false,
            search_tools: true,
            agent: false,
            skill: false,
        }
    }

    /// CLI/TUI-style defaults: full builtin set including write/edit/bash.
    pub fn cli_defaults() -> Self {
        Self {
            read: true,
            write: true,
            edit: true,
            glob: true,
            grep: true,
            bash: true,
            todo: true,
            search_tools: true,
            agent: false,
            skill: false,
        }
    }

    /// Patch-only defaults: read + glob + grep + write/edit (the latter feed
    /// the patch recorder rather than the disk).
    pub fn patch_only_defaults() -> Self {
        Self {
            read: true,
            write: true,
            edit: true,
            glob: true,
            grep: true,
            bash: false,
            todo: false,
            search_tools: true,
            agent: false,
            skill: false,
        }
    }
}

impl Default for BuiltinToolSet {
    fn default() -> Self {
        Self::read_only_defaults()
    }
}

/// How the runtime should load the tool catalog. `Eager` registers every
/// allowed tool; `Lazy` defers everything except the SearchTools tool which
/// the model invokes to discover others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadingMode {
    #[default]
    Eager,
    Lazy,
}

impl ToolLoadingMode {
    pub fn to_core(self) -> crom_core::ToolLoadingMode {
        match self {
            Self::Eager => crom_core::ToolLoadingMode::Eager,
            Self::Lazy => crom_core::ToolLoadingMode::Search,
        }
    }
}

/// Application-controlled tool policy.
#[derive(Clone, Default)]
pub struct ToolPolicy {
    pub allowed_tools: Vec<ToolRule>,
    pub disallowed_tools: Vec<ToolRule>,
    pub tool_loading_mode: ToolLoadingMode,
    pub custom_tools: Vec<Arc<dyn Tool>>,
    pub enable_builtin_tools: BuiltinToolSet,
    pub side_effect_policy: SideEffectPolicy,
    /// OS sandbox for built-in Bash child processes (M17.3). Defaults to
    /// `off`; `required` refuses a command the host cannot sandbox.
    pub sandbox: crom_core::SandboxConfig,
    /// Crom Guard as the run's only MCP server (`docs/sdk/guard.md`).
    pub guard: Option<GuardConnection>,
}

/// MCP server name Guard is registered under; its tools are `mcp__guard__*`.
pub const GUARD_SERVER_NAME: &str = crom_core::guard::GUARD_SERVER_NAME;

/// A Crom Guard agent endpoint and the session handle the trusted
/// application obtained from Guard's admin API. The handle is sent only as
/// the `Authorization` header and is redacted from `Debug`.
#[derive(Clone)]
pub struct GuardConnection {
    /// Streamable HTTP endpoint, e.g. `http://127.0.0.1:8470/mcp`.
    pub url: String,
    handle: SecretString,
    /// Per-request timeout; `None` uses the MCP default (60 s).
    pub request_timeout: Option<std::time::Duration>,
    /// The trusted application's source of Guard approvals; `None` leaves
    /// every `approval_required` result as a tool error.
    pub approver: Option<Arc<dyn crom_core::guard::GuardApprover>>,
}

impl GuardConnection {
    pub fn new(url: impl Into<String>, handle: SecretString) -> Self {
        Self {
            url: url.into(),
            handle,
            request_timeout: None,
            approver: None,
        }
    }

    pub fn with_request_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.request_timeout = Some(timeout);
        self
    }

    /// On `approval_required`, ask `approver` and, given an approval id,
    /// retry the call once with `_meta.approval_id` (`docs/sdk/guard.md`).
    pub fn with_approver(mut self, approver: Arc<dyn crom_core::guard::GuardApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if !(self.url.starts_with("http://") || self.url.starts_with("https://")) {
            return Err("guard url must be http(s)".into());
        }
        let h = self.handle.expose();
        if h.is_empty() || h.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err("guard session handle is empty or not a single token".into());
        }
        Ok(())
    }
}

impl std::fmt::Debug for GuardConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GuardConnection")
            .field("url", &self.url)
            .field("handle", &"[REDACTED]")
            .field("request_timeout", &self.request_timeout)
            .field("approver", &self.approver.is_some())
            .finish()
    }
}

impl ToolPolicy {
    pub fn read_only() -> Self {
        Self {
            enable_builtin_tools: BuiltinToolSet::read_only_defaults(),
            side_effect_policy: SideEffectPolicy::read_only(),
            ..Default::default()
        }
    }

    pub fn patch_only() -> Self {
        Self {
            enable_builtin_tools: BuiltinToolSet::patch_only_defaults(),
            side_effect_policy: SideEffectPolicy::workspace_write(),
            ..Default::default()
        }
    }

    pub fn with_custom_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.custom_tools.push(tool);
        self
    }

    /// Read-only builtins, sandbox `required`, and Guard as the only MCP
    /// server with an explicit allow rule for `mcp__guard__*`. The allow rule
    /// satisfies only Crom's gate; Guard still decides every call.
    pub fn guard(connection: GuardConnection) -> Self {
        Self {
            allowed_tools: vec![ToolRule::McpServerGlob {
                server: GUARD_SERVER_NAME.into(),
                tool_glob: "*".into(),
            }],
            sandbox: crom_core::SandboxConfig {
                mode: crom_core::SandboxMode::Required,
                ..Default::default()
            },
            guard: Some(connection),
            ..Self::read_only()
        }
    }

    /// The core allowlist: the registry's tools plus, with Guard, the
    /// `mcp__guard__*` tools the core loop discovers at run start.
    pub(crate) fn allowed_tool_names(&self, tools: &crom_core::ToolRegistry) -> Vec<String> {
        let mut names = tools.names();
        if self.guard.is_some() {
            names.push(format!("mcp__{GUARD_SERVER_NAME}__*"));
        }
        names
    }

    pub(crate) fn guard_approver(&self) -> Option<Arc<dyn crom_core::guard::GuardApprover>> {
        self.guard.as_ref().and_then(|g| g.approver.clone())
    }

    /// The MCP options a run uses: only the Guard server, or MCP disabled.
    pub(crate) fn mcp_options(&self) -> crom_mcp::McpOptions {
        let Some(g) = &self.guard else {
            return crom_mcp::McpOptions::default();
        };
        let server = crom_mcp::StreamableHttpMcpServerConfig {
            url: g.url.clone(),
            headers: [(
                "Authorization".to_string(),
                format!("Bearer {}", g.handle.expose()),
            )]
            .into(),
            startup_timeout: None,
            request_timeout: g.request_timeout,
            guard: true,
        };
        crom_mcp::McpOptions {
            enabled: true,
            // A Guard that cannot be reached or lists nothing for this
            // handle fails the run instead of running without its tools.
            strict: true,
            servers: [(
                GUARD_SERVER_NAME.to_string(),
                crom_mcp::McpServerConfig::StreamableHttp(server),
            )]
            .into(),
            ..Default::default()
        }
    }
}

impl std::fmt::Debug for ToolPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolPolicy")
            .field("allowed_tools", &self.allowed_tools)
            .field("disallowed_tools", &self.disallowed_tools)
            .field("tool_loading_mode", &self.tool_loading_mode)
            .field("custom_tool_count", &self.custom_tools.len())
            .field("enable_builtin_tools", &self.enable_builtin_tools)
            .field("side_effect_policy", &self.side_effect_policy)
            .field("guard", &self.guard)
            .finish()
    }
}

/// How the SDK handles approval prompts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractivityMode {
    /// May use approval callback. Still does not use a terminal — only the
    /// host's callback.
    Interactive,
    /// Uses callback if provided; denies otherwise.
    Headless,
    /// Denies all approvals without consulting the callback (still emits
    /// approval-required events for observability).
    #[default]
    HeadlessDeny,
    /// No side effects: plan-only, returns plan/review.
    HeadlessPlanOnly,
}

impl InteractivityMode {
    pub fn allows_callback(self) -> bool {
        matches!(self, Self::Interactive | Self::Headless)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SideEffectPolicy::read_only denies workspace writes and command execution.
    #[test]
    fn read_only_denies_writes_and_bash() {
        let p = SideEffectPolicy::read_only();
        assert!(p.allows(SideEffectLevel::WorkspaceRead));
        assert!(!p.allows(SideEffectLevel::WorkspaceWrite));
        assert!(!p.allows(SideEffectLevel::CommandExecution));
        assert!(!p.allows(SideEffectLevel::ExternalNetwork));
        assert!(!p.allows(SideEffectLevel::ExternalMutation));
    }

    /// SideEffectPolicy::workspace_write allows writes but still denies command execution.
    #[test]
    fn workspace_write_allows_writes_only() {
        let p = SideEffectPolicy::workspace_write();
        assert!(p.allows(SideEffectLevel::WorkspaceRead));
        assert!(p.allows(SideEffectLevel::WorkspaceWrite));
        assert!(!p.allows(SideEffectLevel::CommandExecution));
    }

    /// SideEffectPolicy::no_external_effects denies external network and mutation.
    #[test]
    fn no_external_effects_denies_external() {
        let p = SideEffectPolicy::no_external_effects();
        assert!(!p.allows(SideEffectLevel::ExternalNetwork));
        assert!(!p.allows(SideEffectLevel::ExternalMutation));
        assert!(p.allows(SideEffectLevel::CommandExecution));
    }

    /// BuiltinToolSet default disables Bash, Write, Edit.
    #[test]
    fn default_builtin_set_is_read_shaped() {
        let s = BuiltinToolSet::default();
        assert!(!s.bash);
        assert!(!s.write);
        assert!(!s.edit);
        assert!(s.read);
        assert!(s.glob);
        assert!(s.grep);
    }

    const HANDLE: &str = "gsh_CANARY_0123456789abcdef";

    fn guard_policy() -> ToolPolicy {
        ToolPolicy::guard(GuardConnection::new(
            "http://127.0.0.1:8470/mcp",
            SecretString::new(HANDLE.into()),
        ))
    }

    /// `ToolPolicy::guard` allows only Guard's MCP tools, keeps read-only builtins, and requires the sandbox.
    #[test]
    fn guard_policy_allows_guard_tools_and_requires_sandbox() {
        let p = guard_policy();
        assert!(matches!(
            p.allowed_tools.as_slice(),
            [ToolRule::McpServerGlob { server, tool_glob }]
                if server == GUARD_SERVER_NAME && tool_glob == "*"
        ));
        assert_eq!(p.sandbox.mode, crom_core::SandboxMode::Required);
        assert!(!p.enable_builtin_tools.bash);
        assert!(!p.side_effect_policy.allow_workspace_write);
        assert!(p.guard.is_some());
    }

    /// The Guard connection becomes the only MCP server, marked `guard`, with the handle as a bearer header.
    #[test]
    fn guard_policy_mcp_options_hold_one_guard_server() {
        let opts = guard_policy().mcp_options();
        assert!(opts.enabled);
        assert!(opts.strict);
        assert_eq!(opts.servers.len(), 1);
        let crom_mcp::McpServerConfig::StreamableHttp(c) = &opts.servers[GUARD_SERVER_NAME] else {
            panic!("not http");
        };
        assert!(c.guard);
        assert_eq!(c.url, "http://127.0.0.1:8470/mcp");
        assert_eq!(c.headers["Authorization"], format!("Bearer {HANDLE}"));
        assert!(opts.secret_values().contains(&HANDLE.to_string()));
        assert!(!ToolPolicy::default().mcp_options().enabled);
    }

    /// Debug output of the connection and the policy never shows the handle.
    #[test]
    fn guard_connection_debug_redacts_handle() {
        let p = guard_policy();
        let dbg = format!("{p:?} {:?}", p.guard);
        assert!(!dbg.contains(HANDLE), "{dbg}");
        assert!(dbg.contains("127.0.0.1:8470"), "{dbg}");
    }

    /// InteractivityMode::HeadlessDeny is the SDK default.
    #[test]
    fn default_interactivity_is_headless_deny() {
        let m = InteractivityMode::default();
        assert_eq!(m, InteractivityMode::HeadlessDeny);
        assert!(!m.allows_callback());
    }
}
