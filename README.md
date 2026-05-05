# code-agent

A headless coding-agent library and minimal CLI for Rust, with an Anthropic
Messages API provider. Targets approximate architectural parity with the
public surface of Anthropic's Agent SDK while staying small enough to read in
one sitting.

This is **Milestone 2: Control Plane + Interactive Runtime Alpha**. M2 turns
the M1 one-shot agent loop into a controllable runtime: a long-lived
`AgentClient`, a hook system, a formal permission policy engine, project
and user settings, richer CLI approval UX with diffs and risk scoring,
session continue/fork/show/export, structured telemetry, and a `TodoWrite`
progress tool. M1's narrow surface (one-shot `query()`, six built-in tools,
JSONL sessions, a thin CLI) still works unchanged.

## Workspace layout

```
crates/
  code-agent-core/        types, agent loop, AgentClient, hooks, policy, settings, telemetry
  code-agent-anthropic/   Messages API streaming provider
  code-agent-tools/       Read, Write, Edit, Glob, Grep, Bash, TodoWrite
  code-agent-session/     JSONL store + continue/fork/show/export helpers
  code-agent-cli/         `code-agent` binary (run, resume, continue, fork, chat, sessions)
examples/
  bugfix-demo/            tiny crate with a deliberately failing test (M1 demo)
  policy-demo/            tiny crate plus .agent/settings.json policy + hooks (M2 demo)
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

# Continue the latest session for the current cwd.
cargo run -p code-agent-cli -- continue "and now run the tests"

# Fork a session into a new id and steer it.
cargo run -p code-agent-cli -- fork <SESSION_ID> "try a different fix"

# Live interactive chat (multiple prompts in one process).
cargo run -p code-agent-cli -- chat --show-usage

# List, summarize, and export sessions.
cargo run -p code-agent-cli -- sessions list --cwd .
cargo run -p code-agent-cli -- sessions show <SESSION_ID>
cargo run -p code-agent-cli -- sessions export <SESSION_ID> --format json
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

| Tool      | Permission | Notes                                                  |
|-----------|-----------|---------------------------------------------------------|
| Read      | ReadOnly  | UTF-8 files inside cwd; max 1 MiB by default            |
| Write     | Mutating  | Creates parents; sandboxed to cwd                       |
| Edit      | Mutating  | Single exact-string replacement; errors on 0 or >1 hits |
| Glob      | ReadOnly  | Honors `.gitignore`; max 500 results by default         |
| Grep      | ReadOnly  | Regex over UTF-8 files; max 500 hits                    |
| Bash      | Execution | 30s timeout, 64 KiB stdout/stderr cap, stdin closed     |
| TodoWrite | ReadOnly  | Replaces the agent's task list and emits `TodoUpdate`   |

All filesystem tools resolve paths relative to the workspace root, refuse
parent-traversal escapes, and reject symlinks whose target is outside the
root.

## Hooks, policy, and settings (M2)

Hooks observe and influence the loop at well-defined events:

| Event                | Fires                                  | Useful outputs           |
|----------------------|----------------------------------------|--------------------------|
| `UserPromptSubmit`   | After each user message                | `AddContext`, `Deny`     |
| `PreToolUse`         | Before each tool call                  | `Allow{updated_input}`, `Deny`, `Ask` |
| `PostToolUse`        | After a successful tool call           | `Continue`               |
| `PostToolUseFailure` | After a failed tool call               | `Continue`               |
| `PostToolBatch`      | After all tools in a turn              | `Continue`               |
| `Stop`               | At completion / max-turns / cancel     | `Continue`               |

Decisions merge with priority `Deny > Ask > Allow > AddContext > Continue`.
Built-in hooks live in `code_agent_core::builtin_hooks`:
`BlockEnvFileWritesHook`, `BlockDangerousBashHook`, `MaxPatchSizeHook`,
`AuditLogHook`, `SecretRedactionHook`.

The permission policy engine evaluates each tool call as:

```
hooks → disallow rules → permission mode → allow rules → callback → default deny
```

Allow / disallow rules are typed (`ToolRule`):

* `"Read"` — exact tool by name
* `"Edit*"` — glob over tool names
* `{"tool": "Bash", "command_regex": "^cargo (test|check|fmt|clippy)"}`
* `{"tool": "Edit", "path_glob": "**/.env*"}`

Settings come from `<cwd>/.agent/settings.json` (project) and
`$CODE_AGENT_CONFIG_HOME/settings.json` (or `~/.config/code-agent/`,
user). Project overrides user on scalars; list-typed fields concatenate.

Sample `.agent/settings.json`:

```json
{
  "model": "claude-sonnet-4-6",
  "permission_mode": "default",
  "allowed_tools": [
    "Read", "Glob", "Grep", "Edit", "Write", "TodoWrite",
    {"tool": "Bash", "command_regex": "^cargo (test|check|fmt|clippy)"}
  ],
  "disallowed_tools": [
    {"tool": "Edit", "path_glob": "**/.env*"},
    {"tool": "Bash", "command_regex": "rm\\s+-rf|sudo|curl\\s+.*\\|\\s*sh"}
  ],
  "hooks": {
    "block_env_file_writes": true,
    "block_dangerous_bash": true,
    "max_patch_size_bytes": 200000
  }
}
```

CLI flags:

* `--settings none|project|user|user-and-project`
* `--config PATH` — load a single explicit settings file
* `--yes-to "Bash:^cargo test$"` (repeatable)
* `--deny "Edit:**/.env*"` (repeatable)
* `--log-format pretty|json`, `--show-usage`, `--quiet`, `--verbose`

## Approval UX (M2)

The approval prompt offers a richer menu:

```
[approval] mode=Default
  Tool: Bash
  Command: cargo test
  Working directory: /repo
  Timeout: 30s
  Risk: Low
  Reason: ...
    [y] allow once
    [s] allow for session
    [a] allow for project
    [n] deny once
    [d] deny for session
    [v] view full input
```

For `Edit` and `Write` the prompt shows the file path, whether the file
is being created or modified, a unified-diff preview (via `similar`),
and the byte size of the change.

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

## Demo: policy-demo (M2)

A second demo at `examples/policy-demo` exercises the full policy + hooks
surface. It ships with a failing test, a `.env` file the agent must not
modify, an `AGENTS.md`, and a `.agent/settings.json` configuring policy
and hooks.

```sh
cd examples/policy-demo
cargo test                     # one test fails by design
ANTHROPIC_API_KEY=… code-agent chat \
    --cwd . --permission-mode default --show-usage
```

The settings file disallows reading or writing any `.env*` file and
limits Bash to `cargo test|check|fmt|clippy`. Both the `.agent` policy
and the `BlockEnvFileWritesHook` block any attempt to modify `.env`.

### Full M2 end-to-end walkthrough

`examples/m2-e2e-demo.sh` is a single non-interactive script that drives
`examples/policy-demo` through every M2 surface in one run:

* loads `.agent/settings.json` (typed allow/disallow `ToolRule`s + built-in
  hook flags),
* fixes the failing test under `--permission-mode dontAsk` with
  `Read,Glob,Grep,Edit,Bash,TodoWrite`,
* lists / shows / exports the resulting session,
* appends a follow-up turn via `code-agent continue`,
* branches the run via `code-agent fork` with `--log-format json` and
  prints the AgentEvent type histogram from the JSON telemetry,
* drives a live policy denial — the agent attempts to read `.env`, the
  `Read:**/.env*` disallow rule fires, and the script asserts the file
  hash never changed.

```sh
export ANTHROPIC_API_KEY=...
./examples/m2-e2e-demo.sh
```

The script is idempotent — Step 1 restores `examples/policy-demo/src/lib.rs`
to its committed (buggy) state and clears `.agent/sessions/` on every run.

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

### Long-lived `AgentClient` (M2)

```rust
use std::sync::Arc;
use code_agent_core::{AgentClient, AgentEvent, PermissionMode};

let mut client = AgentClient::builder()
    .cwd(std::env::current_dir()?)
    .model("claude-sonnet-4-6")
    .permission_mode(PermissionMode::Default)
    .allowed_tools(["Read", "Glob", "Grep", "Edit", "Write", "Bash", "TodoWrite"])
    .provider(provider)
    .tools(tools)
    .build()
    .await?;

client.send_user_message("Fix the failing tests").await?;
while let Some(ev) = client.next_event().await? {
    if matches!(ev, AgentEvent::Result(_)) { break; }
    println!("{ev:?}");
}
client.send_user_message("Now run them again").await?; // queued or interleaved
```

`AgentClient` supports `interrupt()`, `set_permission_mode(...)`, queued
messages while busy, and clean shutdown.

### Hooks and policy

```rust
use std::sync::Arc;
use code_agent_core::{
    BlockDangerousBashHook, BlockEnvFileWritesHook, Hooks, ToolRule,
};

// Tool-pattern registration methods return `Result<Self, HookError>` so that
// a typo'd pattern fails loudly instead of silently widening to "match
// every tool".
let hooks = Hooks::new()
    .pre_tool_use("Edit|Write", Arc::new(BlockEnvFileWritesHook::default()))?
    .pre_tool_use("Bash", Arc::new(BlockDangerousBashHook::default()))?;

let opts = code_agent_core::AgentOptions {
    allowed_tool_rules: vec![
        ToolRule::BashCommandRegex("^cargo (test|check|fmt|clippy)".into()),
    ],
    disallowed_tool_rules: vec![
        ToolRule::FilePathGlob { tool: "Edit".into(), pattern: "**/.env*".into() },
    ],
    ..code_agent_core::AgentOptions::default()
};
```

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
