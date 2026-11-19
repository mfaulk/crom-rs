# crom-guard-server

The local Crom Guard service (M18,
[#357](https://github.com/mfaulk/crom-rs/issues/357)): binary `crom-guard` and
library `crom_guard_server`. Part of the Guard TCB. It depends on no `crom-*`
crate other than `crom-guard` (`scripts/check-guard-deps.sh`). Not published.

- **Agent endpoint.** MCP Streamable HTTP (`POST /mcp`, via `rmcp`), authenticated
  with `Authorization: Bearer <session handle>`. `tools/list` shows only the
  operations the caller's grant chain allows. `tools/call` runs
  `crom_guard::Guard::call` to completion even if the client disconnects.
- **Admin API.** A separate listener (0600 unix socket or loopback TCP) with
  its own token: issue and revoke grants, reload downstream credentials, audit
  head, health. Session handles are never accepted there.

```sh
crom-guard serve --config guard.toml
crom-guard policy validate --namespace Support --schema s.cedarschema --policies p.cedar
crom-guard audit verify audit.jsonl
```

The stock binary registers no protected operations. A deployment registers
its own and calls `crom_guard_server::cli::main_with`.

Configuration, endpoints, error codes, limits, shutdown and deployment:
[docs/guard/server.md](../../docs/guard/server.md).

```sh
cargo test -p crom-guard-server
```
