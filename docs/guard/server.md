# Guard server (`crom-guard`)

[#357](https://github.com/mfaulk/crom-rs/issues/357). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §10.3, [ADR 0002](../security/decisions/0002-guard-architecture.md) §2.2–2.3. Code: `crates/crom-guard-server` (library `crom_guard_server`, binary `crom-guard`).

The server puts the in-process pipeline ([pipeline.md](pipeline.md)) behind two listeners:

| Listener | Consumer | Transport | Credential |
|---|---|---|---|
| Agent endpoint | Any MCP client | MCP Streamable HTTP at `POST /mcp`, HTTP/1.1 | `Authorization: Bearer <session handle>` |
| Admin API | The trusted application | HTTP/JSON over a unix socket (mode 0600) or loopback TCP | `Authorization: Bearer <admin token>` |

The two credentials never cross. A session handle sent to the admin API gets `401`. The admin token sent to the agent endpoint is treated as no handle (`no_grant`). The admin token may not start with `gsh_`, so it can never parse as a session handle. There is no stdio transport (§10.3).

## Commands

```sh
crom-guard serve --config guard.toml [--agent-listen 127.0.0.1:8470] [--admin-listen unix:/run/crom-guard/admin.sock]
crom-guard policy validate --namespace Support --schema support.cedarschema --policies support.cedar
crom-guard audit verify audit.jsonl [--anchor <seq>:<hash>]...
```

Exit codes: 0 success; 1 refused to start, invalid policy, or broken audit chain; 2 usage error. `serve` stops on SIGINT or SIGTERM (see "Shutdown").

The stock binary compiles in **no protected operations**: every `tools/call` is `unknown_action` and no grant can be issued. A deployment builds its own small binary that registers its operations and hands over to the same CLI:

```rust
fn main() -> std::process::ExitCode {
    crom_guard_server::cli::main_with(|| {
        let mut registry = crom_guard::OperationRegistry::new();
        // registry.register(Arc::new(MyOperation::new(...))).unwrap();
        registry
    })
}
```

Tests and embedders can call `crom_guard_server::start(config, registry)` directly. It returns a `ServerHandle` with the bound addresses and `shutdown()`.

## Configuration

`guard.toml`. Unknown keys are rejected. Relative paths resolve against the config file's directory.

```toml
[agent]
listen = "127.0.0.1:8470"          # default
# allowed_hosts = ["guard.internal"] # Host header allowlist; default: loopback names
# allow_non_loopback = false         # required to bind a non-loopback address

[admin]
listen = "unix:/run/crom-guard/admin.sock"   # or a loopback "127.0.0.1:8471"
token_file = "/etc/crom-guard/admin.token"   # or token_env = "CROM_GUARD_ADMIN_TOKEN"
app_id = "support-app"                       # issuer recorded on every grant

[policy]
namespace = "Support"
schema = "policies/support.cedarschema"
policies = "policies/support.cedar"

[credentials]
file = "/etc/crom-guard/credentials.json"    # FileCredentialBroker, see credentials.md
# or: env = [{ id = "business-api", var = "BUSINESS_API_TOKEN", actions = ["refund.issue"] }]

[audit]
path = "/var/lib/crom-guard/audit.jsonl"

[grants]
max_ttl_seconds = 3600

[limits]
max_request_bytes = 262144        # agent request body; larger gets 413
admin_max_request_bytes = 65536
request_read_timeout_ms = 10000   # reading headers and body only
resolve_timeout_ms = 5000         # Guard::with_resolve_timer
```

### Refusing to start

`serve` exits 1, and starts no listener, if any of these fails:

- the config parses and validates: exactly one token source and one credential source, positive limits, and loopback binds unless `allow_non_loopback` is set;
- the Cedar schema and policies load in strict mode (`CedarPdp::from_files`);
- the credential source loads (`FileCredentialBroker::open` refuses group- or world-accessible files, symlinks, and other owners);
- the admin token loads. A `token_file` must be a regular file (no symlinks), mode 0600 or 0400, owned by Guard's uid. The token itself must be at least 32 printable ASCII characters and must not start with `gsh_`;
- the audit log verifies (`FileAuditSink::open`);
- the listeners bind. A unix admin socket is created, set to mode 0600, and checked for mode 0600 and Guard's uid before serving. A stale socket at the path is replaced; any other file there is refused. The socket is removed on shutdown.

Startup errors never contain a secret.

## Agent endpoint

MCP Streamable HTTP, served by `rmcp` (pinned `~3.11`) in stateless mode with JSON responses. No `Mcp-Session-Id` is issued, and `GET /mcp` (server-initiated SSE) is not offered. Protocol versions are negotiated by `rmcp`. The tests use 2025-06-18 clients (a raw JSON-RPC client and `crom-mcp`).

| Method | Behaviour |
|---|---|
| `initialize` | Server info (`crom-guard`, crate version) and the `tools` capability. Needs no handle and grants nothing |
| `tools/list` | Authenticates the handle on Guard's clock. Lists only the operations that **every** grant in the caller's chain lists, the same check as pipeline step 2. Names, descriptions and schemas come only from the operation registry. `cacheScope: private`, `ttlMs: 0` |
| `tools/call` | `Guard::call(bearer, name, arguments, meta)`. Missing `arguments` is `{}` |

**`tools/list` errors.** Without a live handle the response is a JSON-RPC error with code `-32001` (`crom_guard_server::SESSION_ERROR`). Its `message` and `data.code` are the pipeline deny code: `no_grant`, `grant_not_yet_valid`, `grant_expired`, `grant_revoked`, or `engine_unavailable` while Guard is failed closed.

**`tools/call` results.** Every outcome is a `CallToolResult` with `_meta`:

```json
{"_meta": {"decision_id": "dec-…", "audit_head": {"seq": 12, "head_hash": "…64 hex…"}}}
```

| `CallResult` | `isError` | `structuredContent` | `content[0].text` |
|---|---|---|---|
| `executed` | `false` | the operation's output | the output as JSON text |
| `denied` | `true` | `{"status": "denied", "code": "<deny code>"}` | the code |
| `approval_required` | `true` | `{"status": "approval_required", "role": "…", "action_digest": "…"}` | `approval_required` |
| `failed` | `true` | `{"status": "failed", "code": "precondition_failed" \| "execution_failed"}` | the code |

`audit_head` is omitted if nothing could be written. Agents see only these stable codes. Unknown tools, aliases and case variants are `unknown_action`.

**Request `_meta`** (untrusted, recorded apart from trusted audit fields): `approval_id` is passed to the approval verifier, and `tool_use_id` and `client_request_id` are recorded as correlation. `client_info` comes from the MCP client info.

**Cancellation.** Each `tools/call` runs `Guard::call` on its own task, tracked by the server, and the handler awaits that task. If the client disconnects, or `rmcp` cancels the request, only the waiting handler is dropped. The call runs to completion and writes its outcome record, so Guard does not latch closed ([pipeline.md](pipeline.md), "Cancellation"). In stateless mode, `notifications/cancelled` cannot reach another request and is accepted and ignored.

## Admin API

All routes need `Authorization: Bearer <admin token>`, compared in constant time. That includes `/admin/health` and unknown paths. A missing or wrong token gets `401 {"error": "unauthorized"}`, and the presented value is never logged. Any request with an `Origin` header gets `403`. Request bodies are capped at `admin_max_request_bytes`.

| Route | Body | Success | Errors |
|---|---|---|---|
| `POST /admin/grants` | `GrantSpec` ([grants.md](grants.md)) | `201 {"grant": Grant, "handle": "gsh_…"}` | `400 invalid_spec` (store rejected it), `4xx invalid_request` (malformed JSON), `503 store_unavailable` |
| `POST /admin/grants/{id}/revoke` | — | `200 {"revoked": "<id>"}` | `404 unknown_grant`, also for a grant another app issued |
| `POST /admin/credentials/reload` | — | `200 {"status": "reloaded"}` | `422 reload_failed` with the load error, which carries no secret. The previous credentials stay in use |
| `GET /admin/audit/head` | — | `200 {"head": {"seq", "head_hash"} \| null}` | — |
| `GET /admin/health` | — | `200 {"status": "ok", "operations": n}` | `503` with `"status": "failed_closed"` after the latch |
| `POST /admin/approvals` | — | — | `501 not_implemented`. Placeholder until [#354](https://github.com/mfaulk/crom-rs/issues/354) |

The issue response is the only place a session handle leaves Guard. The trusted application passes it to the agent out of band. Grants are issued with `admin.app_id` as issuer, so revoke can reach only grants issued through this listener's credential.

Agent-facing delegation (`guard.delegate`, [#353](https://github.com/mfaulk/crom-rs/issues/353)) is a builtin operation in the registry, not an admin route. When #353 registers it, it appears in `tools/list` for grants that list it, with no server change.

## Limits and timeouts

- **Body size.** The agent body is capped at `max_request_bytes` before `rmcp` sees it (`413`). Tool arguments are capped again at 64 KiB by the pipeline (`invalid_arguments`).
- **Read timeouts.** `request_read_timeout_ms` bounds reading request headers (the connection is closed) and the request body (`408`). It never bounds handling: an in-flight call is not cut short. Bound downstream calls inside the executor, and resolution with `resolve_timeout_ms`.
- **No CORS.** No CORS headers are sent. Requests with an `Origin` header are refused on both listeners. The agent endpoint also checks `Host` against `allowed_hosts` (loopback names by default) against DNS rebinding.
- **No TLS.** Both listeners default to loopback. To expose the agent endpoint beyond the host, set `allow_non_loopback = true` and terminate TLS in a reverse proxy. Keep the admin API on the unix socket.

## Shutdown

On SIGINT or SIGTERM the server:

1. stops accepting connections on both listeners;
2. lets each open connection finish its current request, then closes it;
3. waits for every in-flight `Guard::call`, including calls whose client has gone;
4. removes the unix admin socket and exits 0.

Nothing is cancelled, so shutdown never latches Guard and never leaves a decision record without its outcome. There is no forced-exit deadline. A downstream that never answers keeps the process alive until the executor's own timeout fires.

## Logging

`tracing` to stderr, filtered by `CROM_GUARD_LOG` (default `info,rmcp=warn`). Neither the server nor `rmcp` logs request headers, so session handles and the admin token never appear, even at `TRACE`. Raising `rmcp` to `debug` or `trace` logs JSON-RPC messages, including tool arguments. Deny reasons and executor error text stay in the audit log ([pipeline.md](pipeline.md)).

## Deployment notes

- Run Guard as its own user. The credential file, admin token file, audit log and admin socket should be readable only by that user. Agents must not be able to reach the admin socket (#360 checks this).
- Grants live in memory: restarting Guard invalidates every handle ([grants.md](grants.md), "Restart semantics").
- No connection-count limit and no rate limiting. Loopback-only deployment is assumed (§7.5).

## Tests

```sh
cargo test -p crom-guard-server
```

- `tests/http.rs`, over real HTTP on ephemeral ports:
  - `initialize`, `tools/list` and `tools/call` with a raw JSON-RPC client;
  - grant-scoped `tools/list`, and its session errors;
  - unknown tools, aliases and case variants;
  - stable deny, failure and approval codes, and `_meta` passthrough;
  - oversized bodies (413) and stalled headers or bodies (timeouts);
  - each credential on the other listener, missing or wrong admin credentials, and `Origin` on both listeners;
  - issue, revoke, reload, audit head, health, and the approvals stub;
  - a client disconnect mid-execute: the call completes and Guard stays open;
  - shutdown with a call in flight: the call finishes and answers, and new connections are refused;
  - refusing to start on an invalid policy, readable secret files, or a non-loopback bind;
  - the 0600 unix admin socket.
- `tests/compat.rs`: `crom-mcp`'s Streamable HTTP client, used only as a transport.
- `tests/canary.rs`: the session handle, admin token and downstream secret appear in no response (other than the issue response), no audit line, and no `tracing` output captured at `TRACE` from every crate, `rmcp` included.
- `tests/cli.rs`: `serve` from a config file, with the unix admin socket and exit 0 on SIGTERM; `policy validate`; `audit verify`; usage errors.
- Unit tests: config validation and admin-token loading (permissions, symlinks, weak tokens, redacted `Debug`).
