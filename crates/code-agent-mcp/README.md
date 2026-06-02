# code-agent-mcp

Model Context Protocol (MCP) client for code-agent.

Connects to external MCP servers, discovers their tools, prompts, and
resources, and exposes them so the agent can call them alongside built-in
tools.

## What's here

- `McpClient` — JSON-RPC client that initializes a server and lists/calls its
  tools, prompts, and resources (with pagination up to `MCP_MAX_LIST_PAGES`).
- **Transports** (`McpTransportKind`) — stdio (child process) and streamable
  HTTP, plus server log streaming.
- **Config** — `McpOptions`, `McpServerConfig`, and `McpConfigFile` loading
  and merging, with secret redaction helpers for logs and events.
- **Registry** — namespaced tool naming (`agent_tool_name`,
  `parse_agent_tool_name`) and identifier safety checks.
- **Lifecycle events** — connection status, tool discovery, and per-call
  started/completed/failed events.
- `protocol` — typed JSON-RPC and MCP initialize/capability messages.

## Usage

Used by `code-agent-core`'s tool catalog and the `code-agent` CLI to surface
configured MCP servers as agent tools.
