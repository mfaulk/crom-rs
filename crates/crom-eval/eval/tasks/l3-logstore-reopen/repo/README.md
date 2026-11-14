# logkv

An append-only key/value store.

- `wire` — record framing: LEB128 varint lengths, CRC-32 checksums.
- `logstore` — the store: every `put`/`delete` appends a framed record to a
  log; opening a store replays the log to rebuild the in-memory index;
  `compact` rewrites the log with live keys only.
- `kvtool` — a tiny line-oriented command interpreter over a store.

Logs are plain byte vectors (`Vec<u8>`) so tests need no filesystem.
