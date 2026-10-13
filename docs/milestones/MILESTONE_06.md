# Milestone 6 — Subagent Orchestration

**Theme:** Let the parent agent delegate focused subtasks to *subagents*
through a built-in `Agent` tool. Subagents run in fresh, isolated
conversations with their own scoped tools, permission policy, MCP scope,
hooks, and child-session JSONL — they do not pollute the parent's
conversation. M1–M5 surfaces work unchanged when subagents are disabled.

## Built-in `Agent` tool

A new built-in tool with `PermissionKind::Orchestration` (treated like
`Execution`: not auto-approved under `Default` or `AcceptEdits`, surfaced
to callbacks and audit records as "delegate work to a child agent").

Subagents always run with `Agent` stripped from their own tool registry,
so M6 forbids recursive subagent spawning.

## Agent definitions

`AgentDefinition` + `AgentRegistry` accept programmatic registrations and
filesystem-loaded definitions in `.agent/agents/<name>.md`:

```yaml
---
name: security-reviewer
description: Reviews Rust code for security vulnerabilities.
tools:
  - Read
  - Grep
  - Glob
disallowed_tools:
  - Edit
  - Write
  - Bash
model: claude-sonnet-4-6
max_turns: 8
permission_mode: dontAsk
---

You are a security-focused Rust code reviewer.
...
```

The registry merges built-in / programmatic / filesystem sources and
rejects duplicate names.

## `SubagentRuntime`

- Bounded semaphore enforces `max_concurrent_subagents`.
- Per-run timeout, cancellation propagation from the parent.
- Optional checkpoint inheritance for mutating-tool subagents.
- Child-sink factory routes child events to the appropriate JSONL.
- Telemetry emitted for the full child lifecycle.

## Hooks and policy

- New `HookEventName::SubagentStart` / `SubagentStop`.
- New `ToolRule::AgentNameGlob` for allow/deny on agent names.
- The parent must explicitly allow `Agent` (or be allowed once via the
  approval callback) before any subagent runs — even under `acceptEdits`.

## Child sessions

Subagent transcripts are persisted under
`.agent/sessions/children/<parent>/<child>.jsonl`, each prefixed with a
`SubagentMetadata` header record so transcripts can be reverse-mapped to
their parent offline.

## Settings

```json
{
  "agents": {
    "enabled": true,
    "path": ".agent/agents",
    "allow_filesystem_agents": true,
    "max_concurrent_subagents": 3,
    "subagent_timeout_seconds": 300,
    "persist_subagent_transcripts": true,
    "event_visibility": "summary_only",
    "checkpoint_policy": "required_for_mutating_tools",
    "inherit_mcp_servers": false,
    "inherit_approval_memory": false
  }
}
```

By default the parent sees only `SubagentStart`, `SubagentStop`,
`SubagentFailed`, `SubagentCancelled`, and `SubagentTimedOut` events;
intermediate child events stay in the child JSONL unless
`--show-subagent-events` is set.

## CLI surface

```sh
crom agents list --cwd .
crom agents show security-reviewer --cwd .
crom agents validate --cwd .
crom agents run security-reviewer "Review src/auth.rs" --cwd .

crom run "Review this crate for security, tests, and API design." \
    --allowed-tools "Read,Grep,Glob,Bash,Agent" \
    --agents .agent/agents \
    --show-subagent-events

crom sessions children <PARENT_SESSION_ID> --cwd .
crom sessions show-child <SUBAGENT_SESSION_ID> --cwd .
crom sessions export-child <SUBAGENT_SESSION_ID> --format json --cwd .
```

New flags on `run` / `chat`: `--agents`, `--no-agents`,
`--max-concurrent-subagents`, `--subagent-timeout`,
`--show-subagent-events`.

## Demo

`examples/subagent-demo/` — three specialized definitions
(`api-reviewer`, `security-reviewer`, `test-analyst`) plus
`examples/m6-subagent-demo.sh` driving the demo offline.

## Tests

75+ new tests covering definitions, registry, scoping, hooks, parallel
execution, child sessions, and lifecycle status mapping. Existing M1–M5
tests untouched.
