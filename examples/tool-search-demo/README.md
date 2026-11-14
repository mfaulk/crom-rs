# Tool Search Demo

Offline demo for M5 tool search and deferred tool loading. The fake MCP server exposes 150+ deterministic tools over stdio, so catalog inspection and search do not require external services or a model API key.

Run from this directory:

```sh
crom tools stats --mcp-config .agent/mcp.json
crom tools search "github issue bug" --mcp-config .agent/mcp.json
crom tools explain-loading --mcp-config .agent/mcp.json
```

The settings keep local workspace tools and `SearchTools` eager, defer `mcp__*`, and hide mutating fake tools such as `mcp__fake__github_create_issue`.

