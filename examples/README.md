# Examples

Runnable demos for `crom`. Each demo lives in its own directory under
`examples/`. Crate-based demos are **excluded from the root workspace** —
run `cargo` commands from inside the demo directory, not from the repo
root. A few demos (`auth-demo/`, `plugin-demo/`) are not crates and ship
only scripts and config fixtures.

Most demos work offline (no provider API key). Demos that drive the agent
against a real model need `ANTHROPIC_API_KEY` (or the equivalent for the
provider you logged into).

## Demos

| Directory | What it shows | Needs API key |
|-----------|---------------|---------------|
| [`bugfix-demo/`](bugfix-demo/) | Smallest end-to-end loop: agent runs `cargo test`, finds an off-by-one bug, edits `src/lib.rs`, re-runs tests | yes |
| [`policy-demo/`](policy-demo/) | Hook + policy surface — `.env` write blocked by both a policy rule and a built-in hook, `Bash` constrained to `cargo test/check/fmt/clippy` | yes |
| [`checkpoint-demo/`](checkpoint-demo/) | Workspace checkpointing, `--validate "cargo test"`, manual checkpoints, `crom rewind` preview and apply | yes |
| [`mcp-demo/`](mcp-demo/) | MCP client against a deterministic in-tree fake stdio server (`fake-mcp-server/`); exercises `mcp tools`, `mcp call`, resources, prompts | no |
| [`tool-search-demo/`](tool-search-demo/) | Deferred tool loading and `SearchTools` over a 150+ tool fake MCP catalog; exercises `tools list/search/show/stats/explain-loading` | no |
| [`subagent-demo/`](subagent-demo/) | Parent agent delegating to scoped subagents defined under `.agent/agents/` | yes |
| [`skills-demo/`](skills-demo/) | Skills + slash commands — agent loads skill manifests from `.agent/skills/` | yes |
| [`plugin-demo/`](plugin-demo/) *(not a crate)* | Plugin system: a tiny marketplace + plugins contributing tools, agents, hooks, MCP servers; driven by `plugin-demo.sh` | mixed |
| [`beta-demo/`](beta-demo/) | TUI / terminal UX fixture for `crom tui` | yes |
| [`auth-demo/`](auth-demo/) *(not a crate)* | Provider login + credential store walkthrough against a fake provider; driven by `auth-demo/run.sh` | no |
| [`planning-demo/`](planning-demo/) | Plan-then-act flow with a planted bug | yes |
| [`headless-sdk-demo/`](headless-sdk-demo/) | Embedding `crom-sdk`: in-memory workspace, return-only patches, plan/act, cancellation, approval callbacks, custom tools | no (uses `fake-provider`) |

## End-to-end scripts

A handful of non-interactive scripts at the top of this directory exercise
several CLI surfaces in one run. They are idempotent — they reset their
target demo's state on each invocation.

| Script | Drives | Needs API key |
|--------|--------|---------------|
| `e2e-demo.sh` | `bugfix-demo`: run → fix → sessions list → resume → write `SUMMARY.md` | yes |
| `m2-e2e-demo.sh` | `policy-demo`: settings, dontAsk fix, sessions, fork, JSON telemetry, live `.env` denial | yes |
| `m3-checkpoint-demo.sh` | `checkpoint-demo`: checkpoint subcommands + rewind preview, no model call | no |
| `m3-e2e-demo.sh` | `checkpoint-demo`: full checkpoint + `--validate` + auto-rewind walkthrough | yes |
| `m6-subagent-demo.sh` | `subagent-demo`: subagent transcript persistence, child session inspection | no |
| `plugin-demo.sh` | `plugin-demo`: plugin discovery, install, registration warnings | no |

## Running a demo

Typical pattern:

```sh
cd examples/bugfix-demo
cargo test          # demo's own test suite; fails by design until the agent edits the code

# Either install the CLI once (`cargo install --path crates/crom-cli` from the repo root)
# and run the binary directly:
crom run "Run the tests, find the bug, fix it, and run the tests again" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits \
    --max-turns 8

# …or run the CLI through cargo from the repo root with the demo as cwd:
cargo run -p crom-cli --manifest-path ../../Cargo.toml -- \
    run "Run the tests, find the bug, fix it, and run the tests again" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits

cargo test          # passes after the agent's edit
```

Each demo's own `README.md` (where present) and `AGENTS.md` describe the
intent, the planted bug or scenario, and any demo-specific flags.
