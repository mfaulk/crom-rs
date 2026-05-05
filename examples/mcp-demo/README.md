# mcp-demo

Offline MCP client demo for `code-agent`.

The demo includes a deterministic stdio MCP server in `fake-mcp-server/`.
It exposes `list_issues`, `get_issue`, and `search_docs`, plus resource and
prompt inspection endpoints. No external API keys or services are required.

```bash
cd examples/mcp-demo

cargo test

code-agent mcp status
code-agent mcp tools fake
code-agent mcp call fake list_issues --args-json '{}' --yes
code-agent mcp resources fake
code-agent mcp resource read fake fake://docs/parser
code-agent mcp prompts fake
```

Agent run example:

```bash
code-agent run "Summarize open fake issues and suggest which source file to inspect" \
  --mcp-config .agent/mcp.json \
  --allowed-tools "Read,Grep,mcp__fake__list_issues,mcp__fake__get_issue"
```

MCP tool effects are external and are not covered by workspace checkpointing.
