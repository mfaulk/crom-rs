# mcp-demo

Offline demo for Milestone 4 MCP client support.

Rules:
- Use the fake MCP server configured in `.agent/mcp.json`; do not use external services.
- Keep changes inside `examples/mcp-demo`.
- `cargo test` should pass before finishing.
- MCP effects are external to workspace checkpointing, even though this demo server is deterministic.
