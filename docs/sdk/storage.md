# Storage

The SDK ships three `AgentStorage` implementations:

| Storage           | Use case                                                                |
| ----------------- | ----------------------------------------------------------------------- |
| `NoopStorage`     | Default. Drops every event. Useful for stateless runs.                   |
| `InMemoryStorage` | Keeps session events and plans in memory. Useful for tests & web reqs.  |
| `FileStorage`     | JSONL session files + JSON plan files under a configurable root.        |

```rust,no_run
use std::sync::Arc;
use crom_sdk::{AgentRuntime, FileStorage, ProviderConfig};
use crom_sdk::fake_provider::EchoFakeProvider;

# async fn run() -> anyhow::Result<()> {
let storage = Arc::new(FileStorage::new("/tmp/agent-storage")?);
let runtime = AgentRuntime::builder()
    .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
    .storage(storage)
    .build()
    .await?;
# Ok(())
# }
```

## Session policy

`SessionPolicy` controls how the runtime mints session ids and whether
storage records the session at all:

- `None` — no session id, no storage writes.
- `InMemory` — a session id is minted but not persisted.
- `Persisted` — store every event through `AgentStorage`.
- `Existing(SessionId)` — resume an existing session id.
- `HostManaged` — host owns the session record; SDK does not write.

The SDK never persists secrets, tenant ids, or user ids that the
`SecretRedactor` has flagged.
