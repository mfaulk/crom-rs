# Milestone 3 — Workspace Checkpointing + Rewind Alpha

**Theme:** Layer content-addressed local checkpoints onto the M2 runtime.
Each top-level user request snapshots the workspace before any `Write` /
`Edit` mutation; `crom rewind <id>` restores files (conversation
history is left intact). M1 and M2 surfaces work unchanged with
checkpointing turned off.

## New crate: `crom-checkpoint`

- `LocalCheckpointStore` — SHA-256 blob deduplication, JSONL index,
  per-session storage caps, per-session async mutex serializing index
  updates, lazy-cached blob byte total.
- `AgentCheckpointHandle` + `RotatingCheckpointHandle` — implement the new
  `crom_core::CheckpointHandle` trait. `session_id` and `run_id` flow
  through every call so a misconfigured handle cannot bleed snapshots into a
  forked session's lineage.
- `LocalCheckpointApi` — backs `AgentClient::create_checkpoint`,
  `list_checkpoints`, `diff_checkpoint`, `rewind_files`.
- `validation` — shell-based post-run gates with timeouts and output caps;
  exit-non-zero on failure so `--validate` works in CI.

## On-disk layout

```
<cwd>/.agent/checkpoints/<session-id>/
  checkpoints.jsonl       # index of checkpoints
  manifests/<id>.json     # per-checkpoint metadata + tracked files
  blobs/<sha256>          # content-addressed file contents
```

## Core changes

- New `AgentEvent::CheckpointEvent(CheckpointEventPayload)` envelope so core
  stays decoupled from the checkpoint crate.
- `CheckpointHandle` / `CheckpointApi` traits in `crom_core`.
- Snapshots are captured **after** policy approval and **before** mutation.
- The agent loop reads Bash command from `effective_input` (the policy- and
  hook-rewritten input that will actually execute), not raw `call.input`.

## Bash policy

`Bash` is intentionally **not** checkpointed. The configured `bash_policy`
decides what to do:

| Policy | Behavior |
|--------|----------|
| `allow_with_warning` | Run normally, emit a `bash_warning` event |
| `require_explicit_approval` (default) | Deny unless approval came from the callback or stored memory |
| `deny_likely_mutating_commands` | Deny commands like `sed -i`, `mv`, `rm`, `cargo fmt`, redirects |

## Failure modes

- `failure_mode: strict` (default) — denies the mutating call when capture
  fails (file too large, storage cap, symlink escape) so the workspace is
  never modified without a recovery path.
- `failure_mode: best_effort` — demote to a warning.

## Validation gates

```sh
crom run "Refactor the parser" \
    --checkpointing on \
    --validate "cargo test" \
    --rewind-on-validation-failure
```

If any validation command exits non-zero (or times out) and
`rewind_on_failure` is set, the workspace is rewound to the start-of-run
checkpoint. Conversation history is **not** rewound.

## CLI surface

- `crom run --checkpointing on --validate "cargo test" --rewind-on-validation-failure`
- `crom checkpoint create [DESC]`
- `crom checkpoints list / show / diff`
- `crom rewind <ID> [--yes] [--dry-run] [--force-cwd]`
- `chat` rotates checkpoints per user prompt via `RotatingCheckpointHandle`.
- Validation failure now affects the CLI exit code.
- Rewind appends a session note to the checkpoint's owner session, not
  `latest_for_cwd`.

## Settings

```json
{
  "checkpointing": {
    "enabled": true,
    "failure_mode": "strict",
    "create_on_user_message": true,
    "max_checkpoint_bytes_per_file": 5242880,
    "max_total_checkpoint_bytes_per_session": 104857600,
    "retention": { "max_checkpoints_per_session": 50, "max_age_days": 14 },
    "bash_policy": "require_explicit_approval"
  },
  "validation": {
    "commands": ["cargo test"],
    "timeout_seconds": 60,
    "rewind_on_failure": false
  }
}
```

## Sandboxing improvements

- `resolve_within_cwd` rejects intermediate symlink escapes by canonicalizing
  the parent (or closest existing ancestor), not just the terminal component.
- `atomic_write` writes to a unique sibling temp file
  (`<name>.ckpt-tmp.<uuid>`) so `main.rs` and `main.txt` cannot collide on rename.
- Diff redaction extended to `.pem`, `.key`, `.kdbx`, `id_*`, YAML secrets,
  kubeconfig.

## Demos

- `examples/checkpoint-demo` — small crate exercising checkpoint + rewind +
  validation.
- `examples/m3-checkpoint-demo.sh` — no-API-key smoke test of just the
  checkpoint subcommands and rewind preview.
- `examples/m3-e2e-demo.sh` — full end-to-end script.

## Out of scope (per milestone)

MCP, subagents, slash commands, TUI, IDE, multi-provider routing, container
sandboxing, distributed/Git-backed checkpoint storage, full directory-tree
snapshots, tracking arbitrary Bash filesystem mutations.

## Tests

173 passing total: 40 new, 133 baseline preserved. New tests include
intermediate-symlink-escape and sibling-temp-file regressions, rewind/diff
integration, agent-loop integration, session integration, CLI smoke, and
redaction patterns.
