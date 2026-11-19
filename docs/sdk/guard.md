# Crom as a Crom Guard client

[#359](https://github.com/mfaulk/crom-rs/issues/359). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §9.5, §12.4.

Crom reaches [Crom Guard](../guard/server.md) as an ordinary MCP Streamable HTTP server. Guard is not linked into Crom and is not part of Crom's trusted computing base: Crom's crates never depend on a Guard crate (Guard crates appear only as `crom-sdk` dev-dependencies for the integration test), and Guard's crates depend on no other `crom-*` crate (`scripts/check-guard-deps.sh`).

## SDK

```rust
use crom_sdk::{AgentRuntime, GuardConnection, SecretString, ToolPolicy};

// The trusted application obtained `handle` from Guard's admin API
// (`POST /admin/grants`) and never shows it to the model.
let guard = GuardConnection::new("http://127.0.0.1:8470/mcp", SecretString::new(handle));
let runtime = AgentRuntime::builder()
    .provider(provider)
    .tool_policy(ToolPolicy::guard(guard))
    .build()
    .await?;
```

`ToolPolicy::guard(connection)` is `ToolPolicy::read_only()` plus:

- Guard as the run's **only** MCP server, named `guard` (`crom_sdk::GUARD_SERVER_NAME`), with `guard: true`. The SDK loads no other MCP server.
- `allowed_tools = [McpServerGlob { server: "guard", tool_glob: "*" }]`: the explicit allow rule that MCP tools need under the default external-effect policy. It satisfies only Crom's gate.
- `sandbox.mode = required` (it constrains Bash, which `read_only` does not enable; add Bash only with the sandbox enforcing).
- MCP `strict`: if Guard is unreachable at run start, or rejects the handle on `tools/list` (`no_grant`, `grant_expired`, ...), the run ends with an error instead of running without Guard's tools.

Fields stay public, so a host can widen the policy, for example `ToolPolicy { enable_builtin_tools: ..., ..ToolPolicy::guard(conn) }`. `GuardConnection::with_request_timeout` overrides the 60 s MCP request timeout.

`build()` rejects (`policy.invalid_guard`) a non-`http(s)` URL, an empty or multi-token handle, and any custom tool named `mcp__guard__*`, which would shadow a Guard tool and inherit its allow rule.

Guard names operations `customer.read`. Model APIs reject `.` in tool names, so Crom exposes them as `mcp__guard__customer_read`; the call still goes to `customer.read`. Write `McpServerGlob` rules against the `_` form.

Plan generation (`plan`) does not connect to Guard. `run` and `act` do.

## CLI and settings

Any `"type": "http"` MCP server entry accepts `"guard": true` ([cli.md](../cli.md#mcp-servers)):

```json
{
  "mcpServers": {
    "guard": {
      "type": "http",
      "url": "http://127.0.0.1:8470/mcp",
      "headers": { "Authorization": "Bearer ${CROM_GUARD_HANDLE}" },
      "guard": true
    }
  }
}
```

`guard` on a stdio server is a config error: Guard has no stdio transport. Put the entry in user settings or a `--mcp-config` file, not in the repository's `.agent/mcp.json`, which may not interpolate `${VAR}`. Allow the tools on the run, e.g. `--allowed-tools "mcp__guard__*"`.

## Composition

Crom's gate runs first, Guard second (§9.5). Neither can turn the other's deny into an allow:

```
model tool_use ─▶ Crom policy engine (rules, mode, hooks, approvals)
                    │ deny → stop (SecurityDecision, layer policy/hook)
                    ▼ allow
                 Crom MCP client ─HTTP + session handle─▶ Guard
                    │ denied / approval_required / unreachable → tool error
                    │   (SecurityDecision, layer guard, outcome blocked)
                    ▼ executed → tool result (layer guard, outcome executed)
```

- **Crom approvals do not satisfy Guard approvals.** An approval callback or allow rule authorizes only Crom's gate. A Guard `approval_required` comes back as a tool error. A human approves it through Guard's admin API ([approvals.md](../guard/approvals.md)), and the retry must carry `_meta.approval_id`, which Crom's MCP client does not send yet.
- **A Guard deny is never success.** For a `guard: true` server, only a result Crom reads as `executed` reaches the model with `is_error: false`. A `structuredContent.status` of `denied`, `approval_required` or `failed` makes the result an error even if `isError` is false. Post-tool hooks can only withhold results, never flip them.
- **Failure is closed.** A transport error, timeout, or JSON-RPC error (for example `rate_limited`) becomes a tool error `Guard request failed (<code>); the call was not authorized.`, recorded `blocked`. Crom does not retry. The model may call again, and the call goes through both gates again. After a timeout the call may still have run in Guard. Guard's audit log is authoritative.

## Audit correlation

Every call to a `guard: true` server records one `SecurityDecision` with `enforcement: guard` ([security/audit.md](../security/audit.md)):

| Guard result | `outcome` | `decision` | `reason` | `guard_decision_id` |
|---|---|---|---|---|
| `executed` | `executed` | Crom's (`allow`) | — | `_meta.decision_id` |
| `failed` (`precondition_failed`, `execution_failed`) | `failed` | Crom's | the code | `_meta.decision_id` |
| `denied` | `blocked` | `deny` | Guard's code, e.g. `resource_not_found` | `_meta.decision_id` |
| `approval_required` | `blocked` | `deny` | `approval_required` | `_meta.decision_id` |
| error result without a status | `blocked` | `deny` | `malformed_response` | if present |
| unreachable / timeout / JSON-RPC error | `blocked` | `deny` | `guard_unavailable` / `guard_timeout` / Guard's `data.code` | — |

`policy_source` and `rule` still describe Crom's own gate. `guard_decision_id` joins Guard's `guard-audit/v1` records (`decision_id`). It is a correlation aid, not evidence. Codes and decision ids are copied only if they match a safe character set (`[a-z0-9_]`, and `[A-Za-z0-9._:-]` up to 128 bytes); anything else becomes `unknown` or is dropped. Records written before #359 have no `guard_decision_id` and still deserialize.

## The session handle

The handle is a bearer secret. It goes only into the `Authorization` header of requests to Guard:

- `GuardConnection` and `ToolPolicy` redact it in `Debug`.
- It is not in tool arguments, tool results, model requests, events, the persisted session, or the run result.
- The MCP client's redaction list holds both `Bearer <handle>` and the bare handle, so error text that echoes it is scrubbed.
- Crom's MCP client does not log request headers.

`crates/crom-sdk/tests/guard_client.rs` checks this with the live handle as a canary across every SDK event, the run result, every model request, the `FileStorage` session directory, and `tracing` output captured at `TRACE` from every crate.

## Tests

```sh
cargo test -p crom-sdk --features fake-provider --test guard_client
```

`tests/guard_client.rs` starts `crom-guard-server` in process (`crom_guard_server::start`) over the `crom-guard-testkit` support operations and business API, issues a grant through the admin API, and drives a scripted Crom run:

- an allowed and a denied `customer.read` produce `SecurityDecision`s at layer `guard` whose `guard_decision_id`s match Guard's `decision`/`outcome` audit records;
- a Guard shut down after tool discovery yields a `blocked` tool error with `guard_unavailable`;
- the session-handle canary appears in no Crom output.

Unit tests: `crom_core::guard` (result classification), `crom-mcp` config (`guard` flag, bare-credential redaction, dotted tool names), `crom-sdk` policy and builder (`ToolPolicy::guard`, validation), `crom-core` events (old records deserialize).

## Not implemented

- Sending `_meta.approval_id`, `tool_use_id` or `client_request_id` on `tools/call`.
- An in-process Guard inside the Crom runtime (out of scope, §6).
