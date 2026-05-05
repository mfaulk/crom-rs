# code-agent

A headless coding-agent library and minimal CLI for Rust, with an Anthropic
Messages API provider. Targets approximate architectural parity with the
public surface of Anthropic's Agent SDK while staying small enough to read in
one sitting.

This is **Milestone 1: Headless Rust Coding Agent Alpha**. It is intentionally
narrow: a single autonomous task per invocation, six built-in tools, JSONL
session persistence, and a thin CLI. There is no TUI, MCP, plugins, IDE
integration, or checkpointing in this milestone.

## Workspace layout

```
crates/
  code-agent-core/        public traits, agent loop, events, options
  code-agent-anthropic/   Messages API streaming provider
  code-agent-tools/       Read, Write, Edit, Glob, Grep, Bash
  code-agent-session/     JSONL session store under .agent/sessions
  code-agent-cli/         `code-agent` binary (run, resume, sessions list)
examples/
  bugfix-demo/            tiny crate with a deliberately failing test
```

`examples/bugfix-demo` is **excluded** from the workspace because it ships an
intentionally failing test for the agent to fix. Run it from its own
directory.

## Quick start

```sh
# Build everything.
cargo build --workspace

# Run unit and integration tests for all crates.
cargo test --workspace

# Run the CLI against a local repository. Requires ANTHROPIC_API_KEY.
export ANTHROPIC_API_KEY=...
cargo run -p code-agent-cli -- run "What files are here?" \
    --allowed-tools Read,Glob \
    --permission-mode default

# Resume a session by id.
cargo run -p code-agent-cli -- resume <SESSION_ID> "and now run the tests" \
    --permission-mode acceptEdits

# List sessions for a workspace.
cargo run -p code-agent-cli -- sessions list --cwd .
```

The CLI prints:

```
[init] session 01J...
[assistant] I'll inspect the tests first.
[tool] Bash: cargo test
[tool-result] Bash (1342ms)
...
[result] Fixed token parsing bug. All tests pass. (turns=4, subtype=Success)
```

## Built-in tools

| Tool  | Permission | Notes                                                  |
|-------|-----------|---------------------------------------------------------|
| Read  | ReadOnly  | UTF-8 files inside cwd; max 1 MiB by default            |
| Write | Mutating  | Creates parents; sandboxed to cwd                       |
| Edit  | Mutating  | Single exact-string replacement; errors on 0 or >1 hits |
| Glob  | ReadOnly  | Honors `.gitignore`; max 500 results by default         |
| Grep  | ReadOnly  | Regex over UTF-8 files; max 500 hits                    |
| Bash  | Execution | 30s timeout, 64 KiB stdout/stderr cap, stdin closed     |

All filesystem tools resolve paths relative to the workspace root, refuse
parent-traversal escapes, and reject symlinks whose target is outside the
root.

## Permission modes

| Mode         | Read-only | Mutating       | Execution   |
|--------------|-----------|----------------|-------------|
| `default`    | run       | ask            | ask         |
| `acceptEdits`| run       | run            | ask         |
| `plan`       | (no tool execution; first turn becomes the result)        |
| `dontAsk`    | run only tools that appear in `--allowed-tools`           |

`bypassPermissions` is intentionally not implemented in M1.

## Session persistence

Sessions are stored as JSONL at `<cwd>/.agent/sessions/<id>.jsonl`. Each line
is a `SessionRecord` containing a timestamp, cwd, session id, and either a
user prompt or an `AgentEvent`. `code-agent resume <SESSION_ID>` reconstructs
the prior conversation from this file and continues from where it left off.

## Project memory

If `<cwd>/AGENTS.md` exists at session start, its contents are injected as
project instructions into the system prompt. `CLAUDE.md` is **not** loaded in
M1.

## Demo: bugfix-demo

Quickest path:

```sh
cd examples/bugfix-demo
cargo test          # fails: parse_token has an off-by-one bug
code-agent run "Run the tests, find the bug, fix it, and run the tests again" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode acceptEdits \
    --max-turns 8
cargo test          # passes after the agent's edit
```

The demo's `AGENTS.md` instructs the agent to edit only `src/lib.rs`.

### Full end-to-end walkthrough

`examples/e2e-demo.sh` is a single script that exercises every M1 surface in
one run:

* `code-agent run` with `Read`, `Edit`, `Bash` (under `dontAsk`, so it runs
  unattended) to fix the bug in `examples/bugfix-demo`,
* `code-agent sessions list` and a peek at the JSONL under `.agent/sessions/`,
* `code-agent resume <id>` with `Write`, `Glob`, `Grep` for a follow-up that
  produces `SUMMARY.md`,
* `AGENTS.md` project memory scoping both turns.

```sh
export ANTHROPIC_API_KEY=...
./examples/e2e-demo.sh
```

The script is idempotent — it resets `examples/bugfix-demo/src/lib.rs` to the
buggy state and clears `.agent/sessions/` on every run.

## Library use

```rust
use std::sync::Arc;
use code_agent_core::{query, AgentOptions, PermissionMode, ToolRegistry};
use code_agent_anthropic::{AnthropicConfig, AnthropicProvider};
use code_agent_tools::register_builtin;
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let provider = Arc::new(AnthropicProvider::new(AnthropicConfig::from_env()?)?);
    let mut tools = ToolRegistry::new();
    register_builtin(&mut tools);

    let opts = AgentOptions {
        cwd: std::env::current_dir()?,
        permission_mode: PermissionMode::AcceptEdits,
        max_turns: Some(8),
        ..AgentOptions::default()
    };

    let mut events = query("Find and fix the bug", opts, provider, tools).await?;
    while let Some(ev) = events.next().await {
        println!("{ev:?}");
    }
    Ok(())
}
```

For finer control (custom session sinks, resumed history, custom permission
callbacks) construct an `Agent` via `code_agent_core::AgentBuilder`.

## Testing

```sh
cargo test --workspace        # unit + integration tests, no API key required
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
```

Live Anthropic API tests are kept out of the default suite. `ANTHROPIC_API_KEY`
is only needed when actually running the CLI (or wiring `AnthropicProvider`
into your own integration test).

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo build --workspace`, and
`cargo test --workspace --all-targets` on each push and pull request.

## License

Apache-2.0 OR MIT
