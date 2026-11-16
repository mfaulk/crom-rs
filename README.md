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

`.agent/mcp.json` is repository config: it is only loaded after you trust the
workspace (`crom trust`, see [Workspace trust](#workspace-trust)), and it may
not use `${VAR}` placeholders. Put servers that need host secrets in user
settings or pass them with `--mcp-config <file>`; those still interpolate
`${VAR}` from your environment.

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

## Workspace trust

A cloned repository can ship `.agent/settings.json` and `.agent/mcp.json`.
Until you trust the workspace, crom treats both as untrusted:

- `.agent/settings.json` may only tighten policy. These keys are ignored:
  `permission_mode` looser than your user setting (order
  `plan < dontAsk < default < acceptEdits`), `allowed_tools`, safety hooks set
  to `false` (`hooks.block_dangerous_bash`, `block_env_file_writes`,
  `audit_log`, `secret_redaction`), a larger `hooks.max_patch_size_bytes`,
  `validation.commands`, `checkpointing.enabled: false`, a looser
  `checkpointing.bash_policy`, `mcp.enabled: true`, `mcp.config_file`,
  `mcp.servers`, `mcp.external_effect_policy`, `mcp.load_resources`/`load_prompts: true`,
  `agents.inherit_permissions: false`, `plugins.allow_*: true`,
  `plugins.marketplaces`, and `providers`. Everything else (model, disallow
  rules, UI, limits, a tighter sandbox) still applies.
- `.agent/mcp.json` (or any MCP config file under the workspace that was not
  passed with `--mcp-config`) is not read, so no repo server starts.

Each ignored key is reported: a `[trust] ignored …` line on stderr, or a
blocked `security_decision` event (`policy_source: workspace_trust`) under
`--log-format json`. `crom config` lists them too.

When stdin and stderr are a terminal, agent commands (`run`, `chat`,
`resume`, `continue`, `fork`, `plan`, `act`, `replan`, `tui`) list the ignored
keys and ask once whether to trust the workspace; a yes is saved. Headless
runs (no TTY, `--log-format json`, `headless-run`, eval) never prompt and stay
restricted.

```sh
crom trust                # trust the current directory
crom trust path/to/repo   # trust another workspace
crom trust --status       # print trusted/untrusted for the current directory
crom trust --list         # list trusted workspaces
crom untrust              # revoke
```

Decisions are stored in
`${CROM_CONFIG_HOME:-${XDG_CONFIG_HOME:-~/.config}/crom}/trusted-workspaces.json`,
keyed by canonical path. Trust is exact: trusting a directory does not trust
its subdirectories. Even in a trusted workspace, repo-sourced MCP config never
gets `${VAR}` interpolation. The SDK never reads `.agent/settings.json` or
`.agent/mcp.json`; embedders pass configuration explicitly.

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

The OS sandbox suites include fork-bomb and memory containment tests that are
`#[ignore]`d by default because a broken sandbox would exhaust the host. Run
them only through the script, which wraps them in a capped cgroup scope
(Linux, systemd user session):

```sh
scripts/sandbox-tests.sh
```

See [`docs/security/sandbox.md`](docs/security/sandbox.md).

Live provider tests are kept out of the default suite. `ANTHROPIC_API_KEY`
(or the equivalent for OpenAI / Gemini) is only needed when actually running
the agent against a real model.

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo build --workspace`, and
`cargo test --workspace --all-targets` on each push and pull request. The
`security / OS sandbox (linux)` job runs `scripts/sandbox-tests.sh` on
`ubuntu-24.04` with `CROM_REQUIRE_SANDBOX_TESTS=1` and the golden/security
eval suites with `--sandbox required`, plus the adversarial security suite
(scripted attacks scored on host effects, sandbox required and off).

## Roadmap

Milestone specs live in [`docs/milestones/`](docs/milestones/). The current
plan is [M17 — Measurable Agent Capability and Secure Execution](docs/milestones/MILESTONE_17.md).
The security threat model is in [`docs/security/threat-model.md`](docs/security/threat-model.md).
Security audit events (`SecurityDecision`) and secret redaction are described in
[`docs/security/audit.md`](docs/security/audit.md). The Linux OS sandbox for
agent `Bash` (`sandbox` settings, `crom eval run --sandbox`) is described in
[`docs/security/sandbox.md`](docs/security/sandbox.md). The adversarial security
suite (ten effect-scored attack scenarios) is described in
[`docs/security/adversarial-suite.md`](docs/security/adversarial-suite.md).

## License

Apache-2.0 OR MIT
