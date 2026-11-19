# CLI guide

Usage notes for the `crom` binary: installing, authenticating, running the
agent, MCP servers, workspace trust, and development tasks. Run
`crom --help` for every subcommand and `crom <subcommand> --help` for its
flags.

## Install

`crom` is not published to crates.io yet. Install it from this repository
(Rust ≥ 1.89):

```sh
git clone https://github.com/mfaulk/crom-rs.git
cd crom-rs
cargo install --path crates/crom-cli
```

`cargo install` puts the `crom` binary on your `PATH` (usually
`~/.cargo/bin/crom`). While developing, `cargo run -p crom-cli -- <args>`
runs the same CLI without installing.

### Upgrading from `code-agent`

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

## Authenticate

The simplest option is an environment variable:

```sh
export ANTHROPIC_API_KEY=sk-ant-...   # or OPENAI_API_KEY, GEMINI_API_KEY, DEEPSEEK_API_KEY
```

`crom login` stores a key instead. There is no OS keychain backend yet
(#205), so storing a key requires the opt-in file store (a `0600` file under
the crom config directory), enabled per command with `--allow-file-store`
or permanently with `auth.allow_file_credential_store = true` in settings:

```sh
crom login anthropic --api-key-stdin --allow-file-store < ./anthropic.key
crom auth status
```

Without the file store, `crom login` fails with `CredentialStoreUnavailable`.

OpenAI-compatible endpoints such as DeepSeek and local Ollama/vLLM servers
are selected by provider profile:

```sh
DEEPSEEK_API_KEY=sk-... crom run --provider deepseek "..."
```

Built-in profiles, default models, the `providers` settings key, and
capability flags are documented in [`providers.md`](providers.md).

## Run the agent

```sh
# One-shot run.
crom run "Find the bug in src/lib.rs, fix it, and re-run cargo test" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits \
    --max-turns 8

# Interactive chat (multiple prompts in one process).
crom chat --show-usage

# Full-screen terminal UI. `--fake-provider` is offline and deterministic.
crom tui --fake-provider
crom tui --renderer plain     # plain-text fallback
crom tui --snapshot           # single preview frame (no TTY needed)

# Resume the latest session in this directory.
crom continue "and now add a regression test"
```

To run agent `Bash` commands inside the Linux OS sandbox, set
`sandbox.mode = "required"`; see [`security/sandbox.md`](security/sandbox.md).

## MCP servers

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
not covered by workspace rewind — approve them deliberately. MCP stdio
servers are not sandboxed.

For larger catalogs, enable search-mode tool loading so MCP schemas are not
all sent to the model on turn one:

```sh
crom run "Find the parser bug issue and inspect the likely source" \
    --tool-loading search \
    --allowed-tools "Read,Grep,Glob,SearchTools,mcp__github__*"
```

A fully offline MCP walkthrough lives in
[`examples/mcp-demo/`](../examples/mcp-demo/) (a fake stdio server bundled
in-tree, no external services required).

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
  `agents.inherit_permissions: false`, `agents.path`, `plugins.allow_*: true`,
  `plugins.marketplaces`, and `providers`. Everything else (model, disallow
  rules, UI, limits, a tighter sandbox) still applies.
- `.agent/mcp.json` (or any MCP config file under the workspace that was not
  passed with `--mcp-config`) is not read, so no repo server starts.
- Subagent definitions in `.agent/agents/*.md` are not loaded; each file is
  reported as `.agent/agents/<file>#subagent_definition`. `--agents <dir>` and
  a user-level `agents.path` are operator choices and still load.

A subagent never gets more authority than its parent, trusted or not: a
definition's `tools` list only narrows which of the parent's tools the child
can see (it never pre-approves them), the child inherits the parent's allow
and disallow rules, and its mode is capped at the parent's. With
`agents.inherit_permissions: false` the child starts in `default` mode but
still keeps the parent's disallow rules.

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

## Developing

```sh
cargo build --workspace
cargo test --workspace                              # no API key required
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

The OS sandbox suites include fork-bomb and memory containment tests that are
`#[ignore]`d by default because a broken sandbox would exhaust the host. Run
them only through the script, which wraps them in a capped cgroup scope
(Linux, systemd user session):

```sh
scripts/sandbox-tests.sh
```

Live provider tests are kept out of the default suite; an API key is only
needed when running the agent against a real model.

## CI

`.github/workflows/ci.yml` runs formatting, clippy, build, and tests on
stable, beta, MSRV, and macOS; rustdoc; example checks; the offline eval
suites; coverage; `cargo-deny`; and the `security / OS sandbox (linux)` job
(sandbox enforcement tests, sandboxed eval suites, and the adversarial
security suite); and `lint / workflows + spend-cap gate`. Live-provider evals never run on push or PR: `eval-live.yml`
and `eval-benchmark.yml` are manual dispatches that require a
`max_run_cost_usd` input. See
[CI lanes and spend controls](eval.md#ci-lanes-and-spend-controls-295).
