use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Status of a single subagent invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
}

/// What the parent UI sees from a running subagent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubagentEventVisibility {
    /// Parent sees only Start/Stop/Failed/Cancelled/TimedOut events.
    #[default]
    SummaryOnly,
    /// Parent stream interleaves nested child events.
    StreamChildEvents,
}

impl SubagentEventVisibility {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "summary_only" | "summary-only" | "summaryonly" | "summary" => Some(Self::SummaryOnly),
            "stream_child_events"
            | "stream-child-events"
            | "streamchildevents"
            | "stream"
            | "stream_events" => Some(Self::StreamChildEvents),
            _ => None,
        }
    }
}

/// Checkpointing posture for subagents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SubagentCheckpointPolicy {
    /// Inherit the parent's checkpoint configuration verbatim.
    InheritParent,
    /// Disable checkpointing in the subagent (read-only review work).
    Disabled,
    /// Require checkpointing for any mutating tool call inside the subagent
    /// (default).
    #[default]
    RequiredForMutatingTools,
}

impl SubagentCheckpointPolicy {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "inherit" | "inherit_parent" | "inherit-parent" => Some(Self::InheritParent),
            "disabled" | "disable" | "off" => Some(Self::Disabled),
            "required" | "required_for_mutating_tools" | "required-for-mutating-tools" => {
                Some(Self::RequiredForMutatingTools)
            }
            _ => None,
        }
    }
}

/// What state the subagent inherits from the parent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentContextPolicy {
    pub inherit_project_instructions: bool,
    pub inherit_settings: bool,
    pub inherit_hooks: bool,
    pub inherit_permissions: bool,
    pub inherit_mcp_servers: bool,
    pub inherit_tool_search_state: bool,
    pub inherit_approval_memory: bool,
}

impl Default for SubagentContextPolicy {
    fn default() -> Self {
        Self {
            inherit_project_instructions: true,
            inherit_settings: true,
            inherit_hooks: true,
            inherit_permissions: true,
            // Conservative: do not let subagents speak to MCP servers or
            // reuse parent search/approval state by default.
            inherit_mcp_servers: false,
            inherit_tool_search_state: false,
            inherit_approval_memory: false,
        }
    }
}

/// Runtime knobs for the subagent orchestrator.
#[derive(Debug, Clone)]
pub struct AgentRuntimeOptions {
    /// Master switch. If `false`, the `Agent` tool is unavailable even when
    /// agents are registered.
    pub enabled: bool,
    /// Hard cap on concurrent subagent runs spawned by the parent.
    pub max_concurrent_subagents: usize,
    /// Per-subagent timeout. Subagents exceeding this return `TimedOut`.
    pub subagent_timeout: Duration,
    /// Persist the full subagent transcript as a linked child session.
    pub persist_subagent_transcripts: bool,
    /// What the parent stream sees while a child runs.
    pub event_visibility: SubagentEventVisibility,
    /// Inheritance policy applied when the subagent definition does not
    /// override fields itself.
    pub context_policy: SubagentContextPolicy,
    /// Checkpoint posture inside subagents.
    pub checkpoint_policy: SubagentCheckpointPolicy,
    /// Cancel running children when the parent cancels.
    pub cancel_children_on_parent_cancel: bool,
    /// Maximum bytes of the *result preview* surfaced to the parent. The
    /// full transcript is always persisted; this only bounds what's shown
    /// to the parent via the `Agent` tool result.
    pub max_result_preview_bytes: usize,
}

impl Default for AgentRuntimeOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_subagents: 3,
            subagent_timeout: Duration::from_secs(300),
            persist_subagent_transcripts: true,
            event_visibility: SubagentEventVisibility::SummaryOnly,
            context_policy: SubagentContextPolicy::default(),
            checkpoint_policy: SubagentCheckpointPolicy::RequiredForMutatingTools,
            cancel_children_on_parent_cancel: true,
            max_result_preview_bytes: 16 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Default visibility is SummaryOnly so child intermediate events do not
    // pollute the parent stream by default.
    fn default_visibility_is_summary_only() {
        assert_eq!(
            SubagentEventVisibility::default(),
            SubagentEventVisibility::SummaryOnly
        );
    }

    #[test]
    // Default checkpoint policy requires checkpointing for mutating tools so
    // mutating subagents cannot bypass parent rewind safety.
    fn default_checkpoint_policy_requires_mutating_capture() {
        assert_eq!(
            SubagentCheckpointPolicy::default(),
            SubagentCheckpointPolicy::RequiredForMutatingTools
        );
    }

    #[test]
    // Default context policy refuses to leak MCP, tool-search, or approval
    // memory into the child unless explicitly enabled.
    fn default_context_policy_does_not_leak_mcp_or_memory() {
        let p = SubagentContextPolicy::default();
        assert!(!p.inherit_mcp_servers);
        assert!(!p.inherit_tool_search_state);
        assert!(!p.inherit_approval_memory);
        // But project instructions, hooks, and permissions are inherited so
        // safety-relevant rules apply to children.
        assert!(p.inherit_project_instructions);
        assert!(p.inherit_hooks);
        assert!(p.inherit_permissions);
    }

    #[test]
    // Default runtime options match the documented M6 defaults.
    fn default_runtime_options_match_spec() {
        let r = AgentRuntimeOptions::default();
        assert!(r.enabled);
        assert_eq!(r.max_concurrent_subagents, 3);
        assert_eq!(r.subagent_timeout, Duration::from_secs(300));
        assert!(r.persist_subagent_transcripts);
        assert!(r.cancel_children_on_parent_cancel);
    }

    #[test]
    // Lenient parsing accepts both kebab-case and snake_case spellings.
    fn visibility_from_str_lenient() {
        assert_eq!(
            SubagentEventVisibility::from_str_lenient("summary_only"),
            Some(SubagentEventVisibility::SummaryOnly)
        );
        assert_eq!(
            SubagentEventVisibility::from_str_lenient("stream-child-events"),
            Some(SubagentEventVisibility::StreamChildEvents)
        );
        assert_eq!(SubagentEventVisibility::from_str_lenient("bogus"), None);
    }

    #[test]
    // Checkpoint-policy parser accepts inherit/disabled/required spellings.
    fn checkpoint_from_str_lenient() {
        assert_eq!(
            SubagentCheckpointPolicy::from_str_lenient("inherit_parent"),
            Some(SubagentCheckpointPolicy::InheritParent)
        );
        assert_eq!(
            SubagentCheckpointPolicy::from_str_lenient("disabled"),
            Some(SubagentCheckpointPolicy::Disabled)
        );
        assert_eq!(
            SubagentCheckpointPolicy::from_str_lenient("required_for_mutating_tools"),
            Some(SubagentCheckpointPolicy::RequiredForMutatingTools)
        );
    }
}
