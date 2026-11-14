# crom-session

JSONL session persistence for crom.

Records an agent run as an append-only log so sessions can be resumed,
replayed, forked, and exported. Implements `crom_core`'s `SessionSink`
so the runtime writes records as it streams events.

## Storage

Sessions live under `<cwd>/.agent/sessions/<session-id>.jsonl`. Each line is
one `SessionRecord` JSON object with a timestamp, making context
reconstruction deterministic.

## What's here

- `FsSessionStore` / `SessionStore` — read and write session logs.
- `SessionSinkAdapter` — bridges the store to the runtime's `SessionSink`.
- Operations: `replay_conversation`, `fork_session`, `export_session`
  (`ExportFormat`), `summarize_session`, and prompt/settings appenders.
- `recovery` — lock files (`RunLock`), state detection, and resilient record
  loading for interrupted or concurrently-accessed sessions.

## Usage

Wired up by the `crom` CLI; embeddable applications can use it directly
or rely on the SDK's storage configuration.
