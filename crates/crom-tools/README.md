# crom-tools

Built-in local coding tools for crom.

Provides the filesystem and shell tools the agent uses to inspect and modify a
workspace, each implementing `crom_core::Tool`.

## Tools

- `ReadTool`, `WriteTool`, `EditTool` — read and mutate files.
- `GlobTool`, `GrepTool` — find files by pattern and search contents.
- `BashTool` — run shell commands.
- `TodoWriteTool` — manage the agent's task list.

## Safety

All filesystem tools sandbox access to a workspace root and reject path
traversal and symlink escape (`path_sandbox`). Bash runs with a timeout and
output caps. Tool result text is truncated past configured `ToolLimits`.

## Usage

```rust
use crom_core::ToolRegistry;
use crom_tools::register_builtin;

let mut registry = ToolRegistry::default();
register_builtin(&mut registry);
```

Use `register_builtin_with_limits` to supply custom `ToolLimits`.
