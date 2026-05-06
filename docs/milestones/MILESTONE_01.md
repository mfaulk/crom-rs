# Milestone 1 — Headless Coding Agent

**Theme:** Scaffold a headless coding-agent library and minimal CLI for Rust,
backed by Anthropic's Messages API, with approximate architectural parity to
the public surface of the Anthropic Agent SDK.

## Crates introduced

| Crate | Purpose |
|-------|---------|
| `code-agent-core` | Agent loop, `ModelProvider` / `Tool` / `PermissionCallback` traits, `AgentEvent` stream, options, errors. |
| `code-agent-anthropic` | Streaming Messages API provider: SSE parsing, tool-use buffering, retries on 408/429/5xx, separate connect/read timeouts. |
| `code-agent-tools` | Built-in tools: `Read`, `Write`, `Edit`, `Glob`, `Grep`, `Bash`. |
| `code-agent-session` | JSONL session store under `<cwd>/.agent/sessions/<id>.jsonl`; create/append/load/list/latest, `SessionSinkAdapter`, `replay_conversation` for resume. |
| `code-agent-cli` | `code-agent` binary with `run`, `resume`, `sessions list`. |

## Agent loop semantics

Drives `prompt → model → tool calls → local execution → tool results → completion`
until the assistant returns a final response or `max_turns` is reached.

## Sandboxing and safety

- All filesystem tools resolve paths relative to the workspace root, reject
  parent-traversal escapes, and refuse symlinks whose target is outside the
  root — including dangling symlinks (path sandbox walks ancestors with
  `symlink_metadata`, not `exists`).
- `Bash` runs in its own process group with stdin closed, bounded stdout/stderr,
  configurable timeout that kills the whole group (so grandchildren are reaped).
  Caller-supplied `timeout_secs` is capped at the configured ceiling.
  Uses `-c` (not `-lc`) so user login profiles are not sourced.
- Stdin approval prompts default-deny in non-interactive sessions.
- Anthropic provider filters `MessageRole::System` out of the body messages
  (instead of silently downgrading to user) and surfaces invalid tool-input
  JSON via a structured envelope.
- Session id is assigned before spawning so success and error `Result` events
  report the same id; the store reuses an issued id so each run produces
  exactly one session file.

## Project memory

If `<cwd>/AGENTS.md` exists at session start, its contents are injected as
project instructions in the system prompt. (`CLAUDE.md` is **not** loaded in M1.)

## Demo

`examples/bugfix-demo` — small excluded crate with an off-by-one bug in
`parse_token`; `examples/e2e-demo.sh` exercises every M1 surface in one run
(`run`, `sessions list`, `resume`, JSONL inspection, `AGENTS.md` scoping).

## CI

`.github/workflows/ci.yml` runs `cargo fmt --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, build, and test on every push and
pull request.

## Tests

31 unit and integration tests across the workspace, including regressions
for the symlink escape, the timeout cap, the system-role filter, resume
history replay, and `Bash` process-group reaping.
