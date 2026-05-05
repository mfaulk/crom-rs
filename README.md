# code-agent

A headless coding-agent library and minimal CLI for Rust, with an Anthropic
Messages API provider. Targets approximate architectural parity with the
public surface of Anthropic's Agent SDK while staying small enough to read in
one sitting.

This is **Milestone 4: MCP Client Alpha**. M4 adds safe Model Context
Protocol client support on top of the M3 runtime: configured stdio or basic
Streamable HTTP MCP servers can be initialized, queried with `tools/list`,
and exposed as normal agent tools named `mcp__<server>__<tool>`. MCP calls
flow through the existing hooks, permission policy, approvals, sessions,
telemetry, and logs. Workspace checkpointing still only covers local files:
MCP external effects are explicitly marked as not rewindable.

## Workspace layout

```
crates/
  code-agent-core/        types, agent loop, AgentClient, hooks, policy, settings, telemetry
  code-agent-anthropic/   Messages API streaming provider
  code-agent-tools/       Read, Write, Edit, Glob, Grep, Bash, TodoWrite
  code-agent-session/     JSONL store + continue/fork/show/export helpers
  code-agent-checkpoint/  M3: LocalCheckpointStore, CheckpointHandle, validation
  code-agent-mcp/         M4: MCP client config, protocol, stdio/http transports
  code-agent-cli/         `code-agent` binary (run, resume, continue, fork, chat,
                          sessions, checkpoint, checkpoints, rewind, mcp)
examples/
  bugfix-demo/            tiny crate with a deliberately failing test (M1 demo)
  policy-demo/            tiny crate plus .agent/settings.json policy + hooks (M2 demo)
  checkpoint-demo/        tiny crate exercising checkpoint + rewind + validation (M3 demo)
  mcp-demo/               offline fake MCP server and demo project (M4 demo)
```

`examples/bugfix-demo`, `examples/policy-demo`, `examples/checkpoint-demo`,
and `examples/mcp-demo` are **excluded** from the workspace. Run them from
their own directories.

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

# Checkpointing (M3): on by default for the CLI when mutating tools are
# enabled. Pass --validate to add post-run gates and
# --rewind-on-validation-failure to rewind automatically.
cargo run -p code-agent-cli -- run "Refactor parser and add a test" \
    --checkpointing on \
    --permission-mode acceptEdits \
    --validate "cargo test"

# Inspect, diff, and rewind checkpoints.
cargo run -p code-agent-cli -- checkpoints list
cargo run -p code-agent-cli -- checkpoints diff <CHECKPOINT_ID>
cargo run -p code-agent-cli -- rewind <CHECKPOINT_ID> --dry-run
cargo run -p code-agent-cli -- rewind <CHECKPOINT_ID> --yes

# MCP inspection and direct calls (no Anthropic key needed with the demo).
cd examples/mcp-demo
cargo run -p code-agent-cli --manifest-path ../../Cargo.toml -- mcp status
cargo run -p code-agent-cli --manifest-path ../../Cargo.toml -- mcp tools fake
cargo run -p code-agent-cli --manifest-path ../../Cargo.toml -- \
    mcp call fake list_issues --args-json '{}' --yes
```

Session ids supplied to `resume`, `fork`, `sessions show`, and
`sessions export` must match `[A-Za-z0-9_-]+` (UUIDs minted by
`code-agent` satisfy this). Other inputs are rejected so a crafted id
cannot escape `<cwd>/.agent/sessions/`.

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
`AuditLogHook`, `SecretRedactionHook`, plus MCP-specific hooks such as
`BlockMcpServerHook`, `BlockMcpToolNameHook`, `BlockMcpWriteLikeToolHook`,
`McpAuditLogHook`, `McpResultSizeLimitHook`, and `McpSecretRedactionHook`.

The permission policy engine evaluates each tool call as:

```
hooks → disallow rules → permission mode → allow rules → callback → default deny
```

Allow / disallow rules are typed (`ToolRule`):

* `"Read"` — exact tool by name
* `"Edit*"` — glob over tool names
* `"mcp__github__list_issues"` or `"mcp__github__*"` — MCP tools by exact name or glob
* `{"tool": "Bash", "command_regex": "^cargo (test|check|fmt|clippy)"}`
* `{"tool": "Edit", "path_glob": "**/.env*"}`
* `{"server": "github", "tool_glob": "list_*"}` — MCP server/tool glob

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
* `--mcp-config PATH`, `--mcp-enabled true|false`, `--mcp-strict`,
  `--mcp-timeout SECONDS`
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

## MCP Client (M4)

MCP is disabled by default in the library API. The CLI loads MCP when
`.agent/mcp.json` exists, when `.agent/settings.json` enables it, or when
`--mcp-config PATH` is passed.

Example `.agent/mcp.json`:

```json
{
  "mcpServers": {
    "fake": {
      "type": "stdio",
      "command": "cargo",
      "args": ["run", "-p", "fake-mcp-server", "--quiet"],
      "startup_timeout_seconds": 10,
      "request_timeout_seconds": 60
    }
  }
}
```

MCP settings can also live under `.agent/settings.json`:

```json
{
  "mcp": {
    "enabled": true,
    "config_file": ".agent/mcp.json",
    "max_servers": 10,
    "max_tools_per_server": 200,
    "max_tool_result_bytes": 262144,
    "external_effect_policy": "require_explicit_allow_rule"
  },
  "allowed_tools": ["Read", "Grep", "mcp__fake__list_issues"]
}
```

Supported commands:

```sh
code-agent mcp list --cwd .
code-agent mcp status --cwd . --mcp-config .agent/mcp.json
code-agent mcp tools fake --cwd . --mcp-config .agent/mcp.json
code-agent mcp call fake list_issues --args-json '{}' --yes
code-agent mcp resources fake
code-agent mcp resource read fake fake://docs/parser
code-agent mcp prompts fake
```

Agent runs can expose MCP tools through the existing tool allowlist:

```sh
code-agent run "Summarize open fake issues" \
  --mcp-config .agent/mcp.json \
  --allowed-tools "Read,Grep,mcp__fake__list_issues,mcp__fake__get_issue"
```

MCP tools use `PermissionKind::External`. `acceptEdits` does not approve
them, `dontAsk` denies unresolved MCP tools, disallow rules override allow
rules, and the default external-effect policy requires explicit allow rules.
The approval prompt warns that MCP effects are external and not covered by
workspace rewind; mutating-looking tool names get a stronger warning.

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

## Checkpointing and rewind (M3)

When checkpointing is on, the CLI snapshots the workspace before each
`Write`/`Edit` mutation and stores the result under
`<cwd>/.agent/checkpoints/<session-id>/`:

```
checkpoints/<session-id>/
  checkpoints.jsonl       # index of checkpoints
  manifests/<id>.json     # per-checkpoint metadata + tracked files
  blobs/<sha256>          # content-addressed file contents
```

The agent loop captures snapshots **after** policy approval and **before**
mutation. The default `failure_mode: strict` denies the mutating call
when capture fails (e.g. file-too-large, storage cap, symlink escape) so
the workspace is never modified without a recovery path. Set
`failure_mode: best_effort` to demote to a warning.

`Bash` is intentionally **not** checkpointed — there's no portable way
to know what files a shell command touched. The configured
`bash_policy` decides what to do:

| Policy                            | Behavior                                                        |
|-----------------------------------|-----------------------------------------------------------------|
| `allow_with_warning`              | Run normally, emit a `bash_warning` event                       |
| `require_explicit_approval` (default) | Deny unless approval came from the callback or stored memory |
| `deny_likely_mutating_commands`   | Deny commands like `sed -i`, `mv`, `rm`, `cargo fmt`, redirects |

Validation gates run after the agent completes:

```sh
code-agent run "Refactor the parser" \
    --checkpointing on \
    --validate "cargo test" \
    --rewind-on-validation-failure
```

If any validation command exits non-zero (or times out), the workspace
is rewound to the start-of-run checkpoint. **Conversation history is
not rewound** — the next turn will continue from the existing chat,
with files restored.

Settings:

```json
{
  "checkpointing": {
    "enabled": true,
    "failure_mode": "strict",
    "create_on_user_message": true,
    "max_checkpoint_bytes_per_file": 5242880,
    "max_total_checkpoint_bytes_per_session": 104857600,
    "retention": {
      "max_checkpoints_per_session": 50,
      "max_age_days": 14
    },
    "bash_policy": "require_explicit_approval"
  },
  "validation": {
    "commands": ["cargo test"],
    "timeout_seconds": 60,
    "rewind_on_failure": false
  }
}
```

CLI flags:

* `--checkpointing on|off` — overrides settings
* `--validate "cargo test"` (repeatable) — post-run validation commands
* `--validation-timeout <SECS>` — per-command timeout
* `--rewind-on-validation-failure` — auto-rewind when validation fails

`code-agent` subcommands:

* `code-agent checkpoint create [DESCRIPTION]` — manual checkpoint
* `code-agent checkpoints list [--session ID]`
* `code-agent checkpoints show <CHECKPOINT_ID>`
* `code-agent checkpoints diff <CHECKPOINT_ID>` — unified diff vs current files
* `code-agent rewind <CHECKPOINT_ID> [--yes] [--dry-run] [--force-cwd]`

`AgentClient` exposes the same operations programmatically:
`create_checkpoint`, `list_checkpoints`, `diff_checkpoint`, `rewind_files`.

The `examples/checkpoint-demo` directory has a one-test crate set up to
walk through the entire flow.

### Full M3 end-to-end walkthrough

`examples/m3-e2e-demo.sh` is a single non-interactive script that drives
`examples/checkpoint-demo` through every M3 surface in one run:

* runs the agent under `--checkpointing on` with a `--validate "cargo test"`
  gate and asserts the start-of-run checkpoint, per-mutation snapshots, and
  `[validation] passed` events all fire,
* lists / shows / diffs checkpoints via `code-agent checkpoints …`,
* creates a manual checkpoint via `code-agent checkpoint …`,
* previews then performs a rewind (`rewind --dry-run`, then `rewind --yes`)
  and verifies `src/lib.rs` is restored byte-for-byte to the buggy starting
  state while conversation history is left intact,
* exercises auto-rewind: `--validate "false" --rewind-on-validation-failure`
  forces the post-run gate to fail and verifies the agent's edit is
  reverted automatically,
* counts `[warning]` events from the `bash_policy: allow_with_warning`
  setting so shell tool calls are visible as not-checkpointed,
* prints the on-disk content-addressed layout under
  `.agent/checkpoints/<session-id>/{checkpoints.jsonl, manifests/, blobs/}`.

```sh
# Either export ANTHROPIC_API_KEY or add ANTHROPIC_API_KEY=... to repo-root .env.
./examples/m3-e2e-demo.sh
```

The script is idempotent — Step 1 runs `git checkout HEAD -- .` and
`git clean -fd -- .` inside `examples/checkpoint-demo` to restore every
tracked file and drop every untracked non-ignored file (`target/` and
`.agent/sessions/`/`.agent/checkpoints/` are gitignored, so they
survive `git clean`; the script wipes the latter two explicitly).

For a smaller, no-API-key smoke test of just the checkpoint subcommands
and rewind preview, see `examples/m3-checkpoint-demo.sh`.

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
