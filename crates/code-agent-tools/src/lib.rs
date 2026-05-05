//! Built-in local coding tools for code-agent.
//!
//! All filesystem tools sandbox file access to a workspace root and reject
//! path traversal/symlink escape. Bash is sandboxed with timeout and output
//! caps. Tool result text is truncated when it exceeds configured limits.

use std::sync::Arc;

use code_agent_core::{Tool, ToolRegistry};

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod limits;
pub mod path_sandbox;
pub mod read;
pub mod todo;
pub mod write;

pub use bash::BashTool;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use limits::ToolLimits;
pub use read::ReadTool;
pub use todo::TodoWriteTool;
pub use write::WriteTool;

/// Build a `ToolRegistry` populated with all six built-in tools using default
/// `ToolLimits`. Callers can customize by constructing `ToolLimits` and using
/// `register_builtin_with_limits`.
pub fn register_builtin(registry: &mut ToolRegistry) {
    register_builtin_with_limits(registry, ToolLimits::default());
}

pub fn register_builtin_with_limits(registry: &mut ToolRegistry, limits: ToolLimits) {
    let limits = Arc::new(limits);
    registry.register(Arc::new(ReadTool::new(limits.clone())) as Arc<dyn Tool>);
    registry.register(Arc::new(WriteTool::new(limits.clone())) as Arc<dyn Tool>);
    registry.register(Arc::new(EditTool::new(limits.clone())) as Arc<dyn Tool>);
    registry.register(Arc::new(GlobTool::new(limits.clone())) as Arc<dyn Tool>);
    registry.register(Arc::new(GrepTool::new(limits.clone())) as Arc<dyn Tool>);
    registry.register(Arc::new(BashTool::new(limits)) as Arc<dyn Tool>);
    registry.register(Arc::new(TodoWriteTool::new()) as Arc<dyn Tool>);
}

pub(crate) fn truncate_with_marker(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n... (truncated, {} bytes omitted)",
        &s[..end],
        s.len() - end
    )
}
