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
- `sandbox.mode = required` (it constrains Bash, which `read_only` does not enable).
- MCP `strict`: if Guard is unreachable at run start, or rejects the handle on `tools/list` (`no_grant`, `grant_expired`, ...), the run ends with an error instead of running without Guard's tools.

Fields stay public, so a host can widen the policy, for example `ToolPolicy { enable_builtin_tools: ..., ..ToolPolicy::guard(conn) }`. `GuardConnection::with_request_timeout` overrides the 60 s MCP request timeout.

`build()` rejects (`policy.invalid_guard`):

- a non-`http(s)` URL, or an empty or multi-token handle;
- any custom tool named `mcp__guard__*`, which would shadow a Guard tool and inherit its allow rule;
- Bash (`enable_builtin_tools.bash` with `allow_command_execution`) unless `sandbox.mode` is `required`;
- the subagent tool (`enable_builtin_tools.agent`): child runs are not shown to inherit the Guard-only MCP set and sandbox.

Guard names operations `customer.read`. Model APIs reject `.` in tool names, so Crom exposes them as `mcp__guard__customer_read`; the call still goes to `customer.read`. A tool rule that names an MCP tool with a `.` (`mcp__guard__refund.issue`, `McpServerGlob { tool_glob: "refund.*" }`) can never match, so it is rejected: by `build()` (`policy.invalid_tool_rule`), by settings parsing, and by `--allowed-tools` / `--disallowed-tools`. Write rules with `_`. `SecurityDecision.tool_name` records the mapped name; the approval prompt (`approval_summary`) shows the real remote name (`Remote tool: customer.read`).

`run` connects to Guard. `act` connects too, but the plan driver restricts tools to those the plan names, so only planned Guard tools are callable. Plan generation (`plan`) does not connect to Guard.

### Delegation is the trusted application's job

Guard's own `guard.*` tools are never registered as Crom tools, from any `guard: true` server. `guard.delegate` returns a child session handle in its result, which would otherwise reach the model, events, the session file and provider requests. To give a subagent narrower authority, the trusted application calls `guard.delegate` itself (or issues a separate grant through the admin API) and starts the child run with the child handle.

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

`guard` is honoured only from operator config: user settings or a `--mcp-config` file. A repository's `.agent/mcp.json` or `.agent/settings.json` that sets it fails to load, trusted workspace or not. `guard` on a stdio server is a config error: Guard has no stdio transport. Allow the tools on the run, e.g. `--allowed-tools "mcp__guard__*"`.

If the server named `guard` is not `guard: true`, the run refuses to start when any allow rule targets it (`McpServerGlob { server: "guard" }`, or an `mcp__guard__…` name or glob), so an ordinary server of that name cannot inherit Guard's allow rules (`crom_core::guard::check_guard_allow_rules`).

## Composition

Crom's gate runs first, Guard second (§9.5). Neither can turn the other's deny into an allow:

```
model tool_use ─▶ Crom policy engine (rules, mode, hooks, approvals)
                    │ deny → stop (SecurityDecision, layer policy/hook)
                    ▼ allow
                 Crom MCP client ─HTTP + session handle─▶ Guard
                    │ denied / approval_required / unreachable → tool error
                    │   (SecurityDecision, layer guard, outcome blocked)
                    │ timeout → tool error, outcome unknown (layer guard, outcome failed)
                    ▼ executed → tool result (layer guard, outcome executed)
```

- **Crom approvals do not satisfy Guard approvals.** An approval callback or allow rule authorizes only Crom's gate. A Guard `approval_required` comes back as a tool error. A human approves it through Guard's admin API ([approvals.md](../guard/approvals.md)), and the retry must carry `_meta.approval_id`, which Crom's MCP client does not send yet.
- **A Guard deny is never success.** For a `guard: true` server, a result with `isError: true` is always a tool error, read by its `structuredContent.status` (`denied`, `approval_required`, `failed`; anything else is `malformed_response`). A result with `isError: false` is always `executed`: `structuredContent` then holds the operation's own output, whose `status`/`code` fields are business data and are not read. Post-tool hooks can only withhold results, never flip them.
- **No verdict means not authorized.** A connection error or JSON-RPC error (for example `rate_limited`) becomes the tool error `Guard request failed (<code>); the call was not authorized.`, recorded `blocked`.
- **A timeout means the outcome is unknown.** The request may have reached Guard and run. The model gets `Guard request failed (guard_timeout); outcome unknown — the action may have run; check before retrying.`, and the decision is recorded `failed` (not `blocked`). Guard's audit log is authoritative.
- Crom does not retry. If the model calls again, the call goes through both gates again.

## Audit correlation

Every call to a tool registered from a `guard: true` server records one `SecurityDecision` with `enforcement: guard` ([security/audit.md](../security/audit.md)). Only those tools' results are read as Guard verdicts: `guard` metadata on any other tool's result is ignored.

| Guard result | `outcome` | `decision` | `reason` | `guard_decision_id` |
|---|---|---|---|---|
| `isError: false` | `executed` | Crom's (`allow`) | — | `_meta.decision_id` |
| `failed` (`precondition_failed`, `execution_failed`) | `failed` | Crom's | the code | `_meta.decision_id` |
| `denied` | `blocked` | `deny` | Guard's code, e.g. `resource_not_found` | `_meta.decision_id` |
| `approval_required` | `blocked` | `deny` | `approval_required` | `_meta.decision_id` |
| error result without a known status | `blocked` | `deny` | `malformed_response` | if present |
| connection error / JSON-RPC error | `blocked` | `deny` | `guard_unavailable` / Guard's `data.code` | — |
| timeout | `failed` | Crom's | `guard_timeout` | — |

`policy_source` and `rule` still describe Crom's own gate. `guard_decision_id` joins Guard's `guard-audit/v1` records (`decision_id`). It is a correlation aid, not evidence. Codes and decision ids are kept only if they match a safe character set (`[a-z0-9_]` up to 64 bytes, and `[A-Za-z0-9._:-]` up to 128 bytes), both when read from Guard and when read back from tool-result metadata; anything else becomes `unknown` or is dropped. Records written before #359 have no `guard_decision_id` and still deserialize.

## The session handle

The handle is a bearer secret. It goes only into the `Authorization` header of requests to Guard:

- `GuardConnection` and `ToolPolicy` redact it in `Debug`.
- It is not in tool arguments, tool results, model requests, events, the persisted session, or the run result.
- The MCP client's redaction list holds both `Bearer <handle>` and the bare handle, so error text that echoes it is scrubbed.
- Crom's MCP client does not log request headers.
- `guard.delegate`, the only Guard tool that returns a handle, is never exposed (see above).

`crates/crom-sdk/tests/guard_client.rs` checks this with the live handle as a canary across every SDK event, the run result, every model request, the `FileStorage` session directory, and `tracing` output captured at `TRACE` from every crate.

## Tests

```sh
cargo test -p crom-sdk --features fake-provider --test guard_client
```

`tests/guard_client.rs` starts `crom-guard-server` in process (`crom_guard_server::start`) over the `crom-guard-testkit` support operations, `guard.delegate`, and the business API. It issues a grant through the admin API and drives a scripted Crom run:

- an allowed and a denied `customer.read` produce `SecurityDecision`s at layer `guard` whose `guard_decision_id`s match Guard's `decision`/`outcome` audit records;
- a Guard shut down after tool discovery yields a `blocked` tool error with `guard_unavailable`;
- an unknown handle fails the run before any call;
- `mcp__guard__guard_delegate` is not registered, and Guard never sees the call;
- `act` reaches Guard for a planned Guard tool and never for an unplanned one;
- the session-handle canary appears in no Crom output.

Unit tests:

- `crom_core::guard`: result classification (business `status` fields in executed output, timeouts as outcome-unknown), metadata re-filtering, `apply_verdict`, `check_guard_allow_rules`;
- `crom-core` `tests/security_audit.rs`: forged `guard` metadata from a host tool is ignored;
- `crom-core` events (old records deserialize) and tool rules (dotted MCP names rejected);
- `crom-core` settings and `crom-mcp` config: `guard` refused from repository config; also the `guard` flag, bare-credential redaction, and dotted tool names;
- `crom-sdk` policy and builder: `ToolPolicy::guard`, URL/handle/namespace checks, Bash without the sandbox, the subagent tool, dotted rules;
- `crom-cli`: dotted MCP rules on the command line.

## Not implemented

- Sending `_meta.approval_id`, `tool_use_id` or `client_request_id` on `tools/call`.
- An in-process Guard inside the Crom runtime (out of scope, §6).
