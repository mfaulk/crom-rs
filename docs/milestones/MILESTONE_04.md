# Milestone 4 — MCP Client Alpha

**Theme:** Add safe Model Context Protocol client support on top of the M3
runtime. Configured stdio or basic Streamable HTTP MCP servers can be
initialized, queried with `tools/list`, and exposed as normal agent tools
named `mcp__<server>__<tool>`. MCP calls flow through the existing hooks,
permission policy, approvals, sessions, telemetry, and logs. Workspace
checkpointing still only covers local files — MCP external effects are
explicitly marked as not rewindable.

## New crate: `code-agent-mcp`

- Config types and loading from `.agent/mcp.json` (or under
  `.agent/settings.json -> mcp`).
- MCP protocol layer — `initialize`, `tools/list`, `tools/call`,
  `resources/list`, `resources/read`, `prompts/list`.
- Transports:
  - `stdio` — long-lived child process, request/response correlation.
  - `streamable_http` — basic Streamable HTTP transport.
- Tool registry / adapter exposing MCP tools to the agent under
  `mcp__<server>__<tool>` names.
- Bundled offline test fixture (`fake-mcp-server`) for stdio + HTTP lifecycle
  tests.

## Permission integration

MCP tools use a new `PermissionKind::External`:

- `acceptEdits` does **not** approve them.
- `dontAsk` denies unresolved MCP tools.
- Disallow rules override allow rules.
- The default external-effect policy requires explicit allow rules
  (`require_explicit_allow_rule`).
- The approval prompt warns that MCP effects are external and **not** covered
  by workspace rewind; mutating-looking tool names get a stronger warning.

## New `ToolRule` variants

```jsonc
"mcp__github__list_issues"           // exact MCP tool by name
"mcp__github__*"                     // glob over MCP tools
{ "server": "github", "tool_glob": "list_*" }   // server + tool glob
```

## Built-in MCP-specific hooks

- `BlockMcpServerHook`
- `BlockMcpToolNameHook`
- `BlockMcpWriteLikeToolHook`
- `McpAuditLogHook`
- `McpResultSizeLimitHook`
- `McpSecretRedactionHook`

## Configuration

`.agent/mcp.json`:

```json
{
  "mcpServers": {
    "fake": {
      "type": "stdio",
      "command": "cargo",
      "args": ["run", "-p", "fake-mcp-server", "--quiet"],
      "startup_timeout_seconds": 10,
      "request_timeout_seconds": 60
    }
  }
}
```

`.agent/settings.json`:

```json
{
  "mcp": {
    "enabled": true,
    "config_file": ".agent/mcp.json",
    "max_servers": 10,
    "max_tools_per_server": 200,
    "max_tool_result_bytes": 262144,
    "external_effect_policy": "require_explicit_allow_rule"
  },
  "allowed_tools": ["Read", "Grep", "mcp__fake__list_issues"]
}
```

MCP is disabled by default in the library API. The CLI loads MCP when
`.agent/mcp.json` exists, when `.agent/settings.json` enables it, or when
`--mcp-config PATH` is passed.

## CLI surface

```sh
code-agent mcp list --cwd .
code-agent mcp status --cwd . --mcp-config .agent/mcp.json
code-agent mcp tools <server>
code-agent mcp call <server> <tool> --args-json '{}' --yes
code-agent mcp resources <server>
code-agent mcp resource read <server> <uri>
code-agent mcp prompts <server>
```

Run flags: `--mcp-config PATH`, `--mcp-enabled true|false`, `--mcp-strict`,
`--mcp-timeout SECONDS`.

Agent runs expose MCP tools through the existing tool allowlist:

```sh
code-agent run "Summarize open fake issues" \
  --mcp-config .agent/mcp.json \
  --allowed-tools "Read,Grep,mcp__fake__list_issues,mcp__fake__get_issue"
```

## Demo

`examples/mcp-demo` — offline fake MCP server and demo project; no Anthropic
key needed for the inspection commands.
