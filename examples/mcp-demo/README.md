# mcp-demo

Offline MCP client demo for `crom`.

The demo includes a deterministic stdio MCP server in `fake-mcp-server/`.
It exposes `list_issues`, `get_issue`, and `search_docs`, plus resource and
prompt inspection endpoints. No external API keys or services are required.

```bash
cd examples/mcp-demo

cargo test

crom mcp status
crom mcp tools fake
crom mcp call fake list_issues --args-json '{}' --yes
crom mcp resources fake
crom mcp resource read fake fake://docs/parser
crom mcp prompts fake
```

Agent run example:

```bash
crom run "Summarize open fake issues and suggest which source file to inspect" \
  --mcp-config .agent/mcp.json \
  --allowed-tools "Read,Grep,mcp__fake__list_issues,mcp__fake__get_issue"
```

MCP tool effects are external and are not covered by workspace checkpointing.
