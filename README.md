# crom-rs

A headless coding agent for Rust. Ships as the `crom` CLI binary and a
set of embeddable crates (provider adapters, tool registry, MCP client, SDK
runtime). The library targets approximate architectural parity with the
public surface of Anthropic's Agent SDK while staying small enough to read
in one sitting.

## Quickstart

### Install

`crom` is not published to crates.io yet. Install the CLI from this
repository:

```sh
git clone https://github.com/mfaulk/crom-rs.git
cd crom-rs
cargo install --path crates/crom-cli
```

`cargo install` puts the `crom` binary on your `PATH` (usually
`~/.cargo/bin/crom`).

#### Upgrading from `code-agent`

The project was renamed from `code-agent` to `crom`. User settings now live
in `${CROM_CONFIG_HOME:-${XDG_CONFIG_HOME:-~/.config}/crom}/settings.json`,
and every `CODE_AGENT_*` environment variable is now `CROM_*` (for example
`CROM_CONFIG_HOME`, `CROM_EVAL_LIVE`, `CROM_EVAL_MODEL`).

Until you migrate, `crom` falls back read-only to the old names and prints a
one-time `warning:` on stderr:

- If `<config>/crom` does not exist but `<config>/code-agent` does, user
  settings are read from `<config>/code-agent`. Nothing is ever written there;
  once `<config>/crom` exists it always wins.
- If a `CROM_*` variable is unset but its `CODE_AGENT_*` spelling is set, the
  old value is used. The new name always wins when both are set.
- Plugin manifests using `${CODE_AGENT_PLUGIN_ROOT}` /
  `${CODE_AGENT_PLUGIN_DATA}` still resolve.

To migrate: `mv ~/.config/code-agent ~/.config/crom` and rename the
environment variables. The project-level `.agent/` directory is unchanged.

### Authenticate

Log in once and the credential is stored in the OS keychain (or a clearly
opt-in file backend if no keychain is available). Pick whichever provider
you want to use:

```sh
crom login anthropic --api-key=sk-ant-...
crom login openai    --api-key-stdin < ./openai.key
crom login gemini    --api-key=AI-...

crom auth status
```

You can also export `ANTHROPIC_API_KEY` (or the provider equivalent) in your
shell and skip `crom login` entirely.

OpenAI-compatible endpoints such as DeepSeek and local Ollama/vLLM servers
are selected by provider profile:

```sh
DEEPSEEK_API_KEY=sk-... crom run --provider deepseek --model deepseek-chat "..."
```

Built-in profiles, the `providers` settings key, and capability flags are
documented in [`docs/providers.md`](docs/providers.md).

### Run the agent

```sh
# One-shot run.
crom run "Find the bug in src/lib.rs, fix it, and re-run cargo test" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits \
    --max-turns 8

# Interactive chat (multiple prompts in one process).
crom chat --show-usage

# Full-screen terminal cockpit driven by the SDK runtime (M13 Phase A).
# `--fake-provider` is offline and deterministic; remove it once an
# `ANTHROPIC_API_KEY` is in your environment.
crom tui --fake-provider
crom tui --renderer plain     # plain-text fallback
crom tui --snapshot           # single preview frame (CI-friendly)

# Resume the latest session in this directory.
crom continue "and now add a regression test"
```

`crom --help` lists every subcommand; `crom <subcommand> --help`
shows flags for each one.

## Adding an MCP server

`crom` is an MCP client. To expose an MCP server's tools to the agent,
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
crom run "Triage the three oldest open issues" \
    --allowed-tools "Read,Grep,mcp__github__list_issues,mcp__github__get_issue"
```

Inspect what each server exposes before granting access:

```sh
crom mcp list          # show configured servers
crom mcp status        # connect and report each server's health
crom mcp tools github  # list the tools the github server exposes
crom mcp call github list_issues --args-json '{}' --yes
```

MCP tools use `PermissionKind::External`. `acceptEdits` does *not* approve
them, `dontAsk` denies any unresolved MCP tool, and the default
external-effect policy requires an explicit allow rule. Live MCP effects are
not covered by workspace rewind — approve them deliberately.

For larger catalogs, enable search-mode tool loading so MCP schemas are not
all sent to the model on turn one:

```sh
crom run "Find the parser bug issue and inspect the likely source" \
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
cargo run -p crom-cli -- run "What files are here?" \
    --allowed-tools Read,Glob \
    --permission-mode default
```

The `cargo run -p crom-cli -- <args>` form is the easiest way to
iterate on the CLI while developing — every CLI subcommand documented above
works after `--`.

Live provider tests are kept out of the default suite. `ANTHROPIC_API_KEY`
(or the equivalent for OpenAI / Gemini) is only needed when actually running
the agent against a real model.

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo build --workspace`, and
`cargo test --workspace --all-targets` on each push and pull request.

## Roadmap

Milestone specs live in [`docs/milestones/`](docs/milestones/). The current
plan is [M17 — Measurable Agent Capability and Secure Execution](docs/milestones/MILESTONE_17.md).
The security threat model is in [`docs/security/threat-model.md`](docs/security/threat-model.md).

## License

Apache-2.0 OR MIT
