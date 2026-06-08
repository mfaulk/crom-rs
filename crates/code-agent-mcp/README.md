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

## Concurrency

Request concurrency is an explicit per-transport capability. A transport
reports it via `McpTransport::supports_concurrent_requests`:

- **stdio** is single-flight: its request/response cycle needs exclusive
  access to the shared stdout framing reader, so callers serialize requests
  (the `&mut self` `send_request` path).
- **streamable HTTP** advertises concurrency (`true`): each request is an
  independent HTTP round-trip over a cloneable `reqwest::Client`, so multiple
  tool calls to the same server can be in flight at once via the `&self`
  `McpClient::call_tool_concurrent` / `McpTransport::send_request_concurrent`
  path. Unique JSON-RPC ids come from an atomic allocator, and each request
  keeps its own per-request timeout. `code-agent-core` picks the concurrent
  path automatically when the transport supports it, so a slow tool call no
  longer blocks a fast one on the same HTTP server.

## Usage

Used by `code-agent-core`'s tool catalog and the `code-agent` CLI to surface
configured MCP servers as agent tools.
