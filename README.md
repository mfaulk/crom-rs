# code-agent

A headless coding agent for Rust. Ships as the `code-agent` CLI binary and a
set of embeddable crates (provider adapters, tool registry, MCP client, SDK
runtime). The library targets approximate architectural parity with the
public surface of Anthropic's Agent SDK while staying small enough to read
in one sitting.

## Quickstart

### Install

`code-agent` is not published to crates.io yet. Install the CLI from this
repository:

```sh
git clone https://github.com/mfaulk/code-agent.git
cd code-agent
cargo install --path crates/code-agent-cli
```

`cargo install` puts the `code-agent` binary on your `PATH` (usually
`~/.cargo/bin/code-agent`).

### Authenticate

Log in once and the credential is stored in the OS keychain (or a clearly
opt-in file backend if no keychain is available). Pick whichever provider
you want to use:

```sh
code-agent login anthropic --api-key=sk-ant-...
code-agent login openai    --api-key-stdin < ./openai.key
code-agent login gemini    --api-key=AI-...

code-agent auth status
```

You can also export `ANTHROPIC_API_KEY` (or the provider equivalent) in your
shell and skip `code-agent login` entirely.

### Run the agent

```sh
# One-shot run.
code-agent run "Find the bug in src/lib.rs, fix it, and re-run cargo test" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits \
    --max-turns 8

# Interactive chat (multiple prompts in one process).
code-agent chat --show-usage

# Full-screen terminal cockpit driven by the SDK runtime (M13 Phase A).
# `--fake-provider` is offline and deterministic; remove it once an
# `ANTHROPIC_API_KEY` is in your environment.
code-agent tui --fake-provider
code-agent tui --renderer plain     # plain-text fallback
code-agent tui --snapshot           # single preview frame (CI-friendly)

# Resume the latest session in this directory.
code-agent continue "and now add a regression test"
```

`code-agent --help` lists every subcommand; `code-agent <subcommand> --help`
shows flags for each one.

## Adding an MCP server

`code-agent` is an MCP client. To expose an MCP server's tools to the agent,
drop an `mcp.json` file at `.agent/mcp.json` in your project root:

```json
{
  "mcpServers": {
    "github": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_TOKEN": "ghp_..." },
      "startup_timeout_seconds": 10,
      "request_timeout_seconds": 60
    }
  }
}
```

HTTP transports are also supported (`"type": "http"` with a `"url"` field).

Allow the MCP tools on the run:

```sh
code-agent run "Triage the three oldest open issues" \
    --allowed-tools "Read,Grep,mcp__github__list_issues,mcp__github__get_issue"
```

Inspect what each server exposes before granting access:

```sh
code-agent mcp list          # show configured servers
code-agent mcp status        # connect and report each server's health
code-agent mcp tools github  # list the tools the github server exposes
code-agent mcp call github list_issues --args-json '{}' --yes
```

MCP tools use `PermissionKind::External`. `acceptEdits` does *not* approve
them, `dontAsk` denies any unresolved MCP tool, and the default
external-effect policy requires an explicit allow rule. Live MCP effects are
not covered by workspace rewind — approve them deliberately.

For larger catalogs, enable search-mode tool loading so MCP schemas are not
all sent to the model on turn one:

```sh
code-agent run "Find the parser bug issue and inspect the likely source" \
    --tool-loading search \
    --allowed-tools "Read,Grep,Glob,SearchTools,mcp__github__*"
```

A fully offline MCP walkthrough lives in `examples/mcp-demo/` (a fake stdio
server bundled in-tree, no external services required).

## Examples

The `examples/` directory contains runnable demos for every major feature —
bugfix loop, policy + hooks, workspace checkpointing, MCP, tool search,
subagents, skills, plugins, auth, planning, and the headless SDK. See
[examples/README.md](examples/README.md) for what each one does and how to
run it.

## Developing

```sh
# Build everything.
cargo build --workspace

# Run unit and integration tests for all crates. No API key required.
cargo test --workspace

# Lint + format.
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check

# Run the CLI without installing.
cargo run -p code-agent-cli -- run "What files are here?" \
    --allowed-tools Read,Glob \
    --permission-mode default
```

The `cargo run -p code-agent-cli -- <args>` form is the easiest way to
iterate on the CLI while developing — every CLI subcommand documented above
works after `--`.

Live provider tests are kept out of the default suite. `ANTHROPIC_API_KEY`
(or the equivalent for OpenAI / Gemini) is only needed when actually running
the agent against a real model.

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo build --workspace`, and
`cargo test --workspace --all-targets` on each push and pull request.

## License

Apache-2.0 OR MIT
