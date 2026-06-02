# code-agent-checkpoint

Workspace checkpointing and rewind for code-agent.

Captures before-mutation snapshots of files the agent changes so a session can
be rewound to an earlier state. Implements `code_agent_core`'s checkpoint
traits.

## Storage

Checkpoints live under `<cwd>/.agent/checkpoints/<session-id>/`. The store is
content-addressed: blobs are deduplicated by SHA-256 hash.

## What's here

- `LocalCheckpointStore` / `CheckpointStore` — capture, create, and rewind.
- `LocalCheckpointApi` — the `CheckpointApi` the runtime drives, capturing a
  snapshot before each `Write`/`Edit`.
- `CheckpointDiff` / `FileDiff` — diff a checkpoint against the working tree.
- `BashCheckpointPolicy` (`classify_bash_command`) — decides which Bash
  commands warrant a checkpoint.
- `CheckpointOptions`, `ValidationOptions`, `CheckpointFailureMode` — tune
  capture behavior and post-rewind validation.
- Typed events (`CheckpointCreated`, `RewindCompleted`, …) and `to_payload`
  for emitting them on the agent event stream.

## Usage

Enabled in `code-agent-sdk` via the `checkpointing` feature and wired up by the
`code-agent` CLI.
