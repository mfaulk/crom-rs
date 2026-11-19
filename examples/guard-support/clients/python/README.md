# Python reference client for Crom Guard

A client that links no Rust or Crom code ([#358](https://github.com/mfaulk/crom-rs/issues/358), MILESTONE_18 §10.4). Python 3.10 or newer, standard library only (`urllib.request`, `http.client`, `json`). CI runs it in the `guard / python client (linux)` job.

| File | Role | Holds |
|---|---|---|
| `guard_client.py` | MCP over Streamable HTTP: `initialize` (+ `notifications/initialized`), `tools/list`, `tools/call` against `POST /mcp`, with the session handle as `Authorization: Bearer` | the session handle |
| `scripted_agent.py` | Runs the MILESTONE_18 §14.2 matrix and checks each step against the tool result **and** the business-API ledger | its session handle; reads the ledger as a test oracle |
| `trusted_app.py` | The trusted application: Guard's admin API over the unix socket (`issue`, `revoke`, `health`, `approve-pending`) | the admin token |
| `test_guard_client.py` | Unit tests: JSON-RPC framing (JSON and SSE bodies), request headers, and a check that every script imports only the standard library | — |

## Run

```sh
examples/guard-support/run-python-client.sh       # from the repo root
cd examples/guard-support/clients/python && python3 -m unittest -v
```

The driver builds `crom-guard-support` (see [docs/guard/reference-deployment.md](../../../../docs/guard/reference-deployment.md)), then:

1. creates a private state directory with `guard.toml`, the policies, a random admin token and a random business-API credential, and starts Guard on free loopback ports;
2. as the trusted app, issues a grant from `grants/acme-agent-operator.json` for an earlier run and revokes it (the replayed handle), then issues the agent's grant. Handles go to 0600 files in the agent's own directory;
3. starts `trusted_app.py approve-pending`, the scripted human approver;
4. runs `scripted_agent.py` under `env -i`, in its own directory, with no path into Guard's state;
5. fails if the business-API credential appears in the agent's environment (`/proc/<pid>/environ`) or in any file it holds, if any secret appears in the agent, Guard or approver logs, or if `crom-guard audit verify` rejects the audit log.

Artifacts (`report.json`, `ledger.jsonl`, `audit.jsonl`, `agent.log`) land in `target/guard-python-client/` (`GUARD_CLIENT_OUT` overrides). `PYTHON` picks the interpreter; `CROM_GUARD_SUPPORT_BIN` skips the cargo build.

## Matrix

| §14.2 row | Steps | Expected |
|---|---|---|
| Read an authorized customer's invoices | `invoice.list acme-cus-000` | `executed`, one `invoice_list` ledger effect carrying the call's `decision_id` |
| Read another tenant's invoices | `invoice.list globex-cus-000`; `acme-cus-001` (same tenant, not in the grant) | `resource_not_found`; `policy_deny`; no effect |
| Refund within delegated limits | `refund.issue acme-inv-000 1000` | `executed`, one `refund` effect |
| Refund above the limit | `refund.issue acme-inv-001 7500`, admin approval as `acme-supervisor`, retry with `_meta.approval_id`, replay | `approval_required` (no effect); `executed` (one effect); `approval_invalid` (no effect) |
| Export all customer records | `customer.export acme-cus-000` | `policy_deny` (`no-bulk-export`), no effect |
| Forge identity | extra `tenant_id` / `user_id` argument; the revoked handle of an earlier run (`tools/list` and `tools/call`); a fabricated handle | `invalid_arguments`; `grant_revoked`; `no_grant`; no effect |
| Delegate broader authority | `guard.delegate` with a higher refund cap, or another customer; then a narrow read-only delegation, a child read, a child `invoice.list` | `policy_deny` ×2; `executed`; `executed` (one effect); `action_not_granted` |
| Bypass via direct API | business API with no credential, or with the session handle as bearer (GET and refund POST) | HTTP 401, no effect |

Finally: every ledger effect carries a Guard decision id, and the run produced exactly four effects.

**Not covered here.** Bypass through Bash or by reading the credential file is an OS-sandbox property (#360). In this job the agent runs as the same uid as Guard, so the driver checks only that the agent is never handed the credential, not that it could not read Guard's 0600 file.

## Secrets

The client sends the session handle only in the `Authorization` header. `repr()` of `GuardClient` and `Admin` redacts it, HTTP errors keep only the status, and `scripted_agent.py` never prints or reports a handle, including a delegated child's. `trusted_app.py` writes handles and approval ids only to new 0600 files and prints only grant ids. The driver greps every log for the handles, the admin token and the business-API secret.

## Using the official MCP Python SDK (optional, not in CI)

Any MCP client works, as long as it sends the handle as a bearer token. With the [`mcp`](https://pypi.org/project/mcp/) package (untested sketch):

```python
from mcp import ClientSession
from mcp.client.streamable_http import streamablehttp_client

async def read_invoices(url, handle):
    headers = {"Authorization": f"Bearer {handle}"}
    async with streamablehttp_client(url, headers=headers) as (read, write, _):
        async with ClientSession(read, write) as session:
            await session.initialize()
            return await session.call_tool("invoice.list", {"customer_id": "acme-cus-000"})
```
