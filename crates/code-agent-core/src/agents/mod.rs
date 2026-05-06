//! Subagent orchestration (M6).
//!
//! A *subagent* is a named, scoped persona the parent agent can delegate
//! focused work to through the built-in `Agent` tool. Each subagent runs
//! in a fresh isolated conversation with its own tool catalog, MCP scope,
//! permission policy, hooks, and checkpointing — separate from the parent.
//!
//! This module defines:
//! - [`AgentDefinition`] and [`AgentDefinitionSummary`] — the typed model.
//! - [`AgentRegistry`] — programmatic + filesystem registration.
//! - [`AgentRuntimeOptions`] / [`SubagentContextPolicy`] — runtime knobs.
//! - [`AgentDefinitionError`] — validation/load errors.
//! - The filesystem (`.agent/agents/*.md`) loader.
//! - Subagent execution events and helper types.

pub mod definition;
pub mod errors;
pub mod loader;
pub mod registry;
pub mod runtime;
pub mod tool;
pub mod yaml;

pub use definition::{
    AgentConcurrencyHint, AgentDefinition, AgentDefinitionSummary, AgentOutputMode,
};
pub use errors::AgentDefinitionError;
pub use loader::{
    filesystem_agents_dir, load_filesystem_agents, load_filesystem_agents_strict,
    parse_definition_file, MAX_AGENT_FILE_BYTES,
};
pub use registry::AgentRegistry;
pub use runtime::{
    AgentRuntimeOptions, SubagentCheckpointPolicy, SubagentContextPolicy, SubagentEventVisibility,
    SubagentStatus,
};
pub use tool::{
    builtin_starter_definitions, AgentTool, AgentToolInput, AgentToolResult, ChildSessionMetadata,
    ChildSessionSinkFactory, SubagentRunHandle, SubagentRuntime, SUBAGENT_TOOL_NAME,
};
