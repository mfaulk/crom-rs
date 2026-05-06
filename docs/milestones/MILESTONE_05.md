# Milestone 5 — Tool Search + Deferred Tool Loading Alpha

**Theme:** Stop sending every tool definition to the model on every turn.
M5 adds a central tool catalog, approximate tool-definition budget
accounting, provider-agnostic lexical tool search, the built-in
`SearchTools` tool, and deferred loading for large local + MCP catalogs.
MCP tools are deferred by default in search mode; local coding tools stay
eager. Tool loading never bypasses hooks, policy, approvals, sessions,
telemetry, or checkpointing.

## Tool catalog

Deterministic catalog built from local tools and discovered MCP tools, with
per-tool metadata (name, description, server, schema property names /
descriptions, tags, aliases, category, risk).

## Loading modes

| Mode | Behavior |
|------|----------|
| `eager` | Send all permitted tool definitions, preserving M1–M4 behavior. |
| `search` | Send only always-loaded tools plus tools discovered by `SearchTools`. |
| `auto` | Use eager for small catalogs, switch to search when tool count or estimated tool-definition tokens exceed the configured threshold. |

## `SearchTools` tool

A new built-in tool (`ReadOnly` permission) for the model to discover and
load deferred tools at runtime.

```json
{
  "query": "github issue search repository bugs",
  "limit": 5,
  "source_filter": "mcp:fake",
  "include_risky": false
}
```

Search ranks over tool name, MCP server name, description, schema property
names / descriptions, tags, aliases, and category. Strategies:

- `regex` — simple regex match
- `bm25` — BM25-like lexical ranking
- `hybrid` — combined ranking

Native provider-side search can also be enabled via `native_provider_mode`
(e.g. `anthropic-regex`, `anthropic-bm25`, `auto`).

Search results are filtered through the same allow / deny rules used by
tool invocation. Loading a tool only makes its definition available on the
**next turn**; the actual call still runs through policy, hooks, approvals,
and MCP external-effect checks.

## Settings

```json
{
  "tool_search": {
    "enabled": true,
    "mode": "auto",
    "strategy": "hybrid",
    "native_provider_mode": "auto",
    "activation_threshold_percent": 10,
    "min_tool_count": 10,
    "max_results": 5,
    "max_loaded_tools": 20,
    "max_loaded_tool_tokens": 12000,
    "always_loaded": ["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite", "SearchTools"],
    "deferred": ["mcp__*"],
    "disabled": [],
    "include_schema_arguments": true,
    "include_tool_category_hints": true,
    "persist_discovered_tools": true
  }
}
```

## CLI flags

- `--tool-loading eager|search|auto`
- `--tool-search-strategy regex|bm25|hybrid`
- `--native-tool-search off|anthropic-regex|anthropic-bm25|auto`
- `--tool-search-threshold N`, `--max-tool-search-results N`
- `--max-loaded-tools N`, `--max-loaded-tool-tokens N`
- `--always-load "Read,Grep,SearchTools"`
- `--defer-tools "mcp__*"`
- `--disable-tools "mcp__github__delete_*"`

## Catalog inspection commands

```sh
code-agent tools list --cwd . --mcp-config .agent/mcp.json --all
code-agent tools list --cwd . --mcp-config .agent/mcp.json --loaded
code-agent tools list --cwd . --mcp-config .agent/mcp.json --deferred
code-agent tools search "github issues" --cwd . --mcp-config .agent/mcp.json --limit 5
code-agent tools show mcp__fake__github_list_issues --cwd . --mcp-config .agent/mcp.json
code-agent tools stats --cwd . --mcp-config .agent/mcp.json
code-agent tools explain-loading --cwd . --mcp-config .agent/mcp.json
```

The `tools` subcommands mirror the agent's runtime semantics:
`allowed_tools` from settings (or `--allowed-tools`) narrows the registry
the same way the agent does, and `--permission-mode` (or `permissionMode` in
settings) is honored when computing search visibility.

## Telemetry

New `AgentEvent` variants emitted to the normal event stream and session
JSONL:

- `ToolCatalogBuilt`
- `ToolLoadingModeResolved`
- `ToolContextBudgetUpdated`
- `ToolSearchStarted`
- `ToolSearchCompleted`
- `ToolLoaded`
- `ToolDeferred`
- `ToolDisabled`
- `ToolSearchFailed`

JSON log mode (`--log-format json`) serializes these as valid JSON lines.

## Example usage

```sh
code-agent run "Find the parser bug issue and inspect the likely source" \
  --mcp-config .agent/mcp.json \
  --tool-loading search \
  --allowed-tools "Read,Grep,Glob,SearchTools,mcp__fake__*"
```

## Demo

`examples/tool-search-demo` — large fake MCP catalog (`fake-large-mcp-server`)
and `.agent/settings.json` configured for deferred loading. Catalog-inspection
commands run without an Anthropic key.
