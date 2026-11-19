# Support reference application

[#362](https://github.com/mfaulk/crom-rs/issues/362). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §14. Code and the full matrix: [`examples/guard-support/`](../../examples/guard-support/README.md). It runs on the [reference deployment](reference-deployment.md).

```sh
cargo run --manifest-path examples/guard-support/Cargo.toml                    # scripted Crom agent, offline (CI)
cargo run --manifest-path examples/guard-support/Cargo.toml -- --agent python  # stdlib Python client
ANTHROPIC_API_KEY=... cargo run --manifest-path examples/guard-support/Cargo.toml -- \
  --agent live --provider anthropic --max-run-cost-usd 0.50                    # real model, manual only
```

## Who holds what

| Party | Process | Holds | Does |
|---|---|---|---|
| Guard + business API | `crom-guard-support` (own process, 0700 state dir) | admin token file, business-API credential, audit log | Authorizes every protected call (Cedar policies, grants, approvals); runs the operation with the broker credential; writes `guard-audit/v1` |
| Support app (trusted) | `guard-support` | admin token | Opens a grant per ticket (`POST /admin/grants`), answers supervisor approvals, delegates to subagents (`guard.delegate`), revokes on ticket close |
| Crom runtime (trusted host code) | `guard-support`, via `crom-sdk` | the ticket's session handle, sent only as `Authorization` to Guard | `ToolPolicy::guard`: Guard is the only MCP server; Bash, when enabled, is sandboxed |
| Model (untrusted) | scripted, or a provider | nothing secret | Chooses tool calls; sees tool results only |

The app makes no authorization decision. A human (scripted, or `--supervisor prompt`) decides approvals, from Guard's own record of the pending request.

## One ticket, end to end

0. Before Guard starts, the driver runs `crom-guard check` for this deployment, with the Crom agent's policy as its settings (agent (a) in [deployment.md](deployment.md): Guard's uid, sandbox `required` without network, Guard the only MCP server, untrusted workspace), and stops if it fails. The Python mode skips it: that client runs unsandboxed on Guard's uid, outside the profile ([reference-deployment.md](reference-deployment.md)).
1. The app issues the grant in `grants/acme-agent-operator.json` (acme operator, customer `acme-cus-000`, refunds up to 100.00, operator limit 50.00) and keeps the handle.
2. Crom connects to Guard with the handle and lists only the granted operations as `mcp__guard__*` tools (`guard.delegate` is never exposed to Crom).
3. The model calls tools. Guard allows, denies (`resource_not_found`, `policy_deny`, `invalid_arguments`, ...) or answers `approval_required` for a refund above the operator's limit.
4. On `approval_required`, Crom asks the app's `GuardApprover` ([sdk/guard.md](../sdk/guard.md#guard-approvals-from-the-host)). The app fetches `GET /admin/approvals/{digest}`, shows the supervisor the exact action, and on approval posts `POST /admin/approvals` as `acme-supervisor`. Crom retries the call once with `_meta.approval_id`; Guard verifies the approval (same grant, action, resource version, arguments; single use) and evaluates the policies again.
5. For a subagent, the app calls `guard.delegate` with the ticket's handle; Guard refuses anything broader than the parent grant. The child Crom run gets the child handle.
6. On ticket close the app revokes the grant (`POST /admin/grants/{id}/revoke`); every later request with that handle is `grant_revoked`.

## Read the evidence

Artifacts land in `target/guard-demo/`.

```sh
cat target/guard-demo/matrix.txt                  # one row per attempted action
cat target/guard-demo/guard-check.txt              # the isolation preflight, one line per check
jq '.checks' target/guard-demo/report.json         # preflight, audit verify, ledger attribution, leak scans
jq -c '{kind, guard_decision_id}' target/guard-demo/ledger.jsonl
jq -c 'select(.approver) | {record, decision_id, action, args_logged, approver, outcome}' \
  target/guard-demo/audit.jsonl                    # the approved refund names its approver
<target-dir>/debug/examples/crom-guard-support audit verify target/guard-demo/audit.jsonl
```

Every ledger entry carries a `guard_decision_id`, which joins exactly one Guard `outcome: executed` record, and the same id is the `guard_decision_id` on Crom's `SecurityDecision` for that call (`crom-events.jsonl`).

## Change something

Edit a policy in `examples/guard-support/policies/` or the grant in `grants/`, run `scripts/validate-guard-policies.sh`, and rerun the scripted demo. Rows whose outcome changed fail, and `matrix.txt` differs from `snapshots/scripted-matrix.txt`. Rerun with `--update-snapshot` only for an intended change.

## Limits

- The scripted Bash attempts show only that those calls fail and leave no ledger effect. The bypass suite is #360; the scored adversarial scenarios are #361.
- The SDK agent reads no settings file, so `check` sees the driver's rendering of the agent's `ToolPolicy` (`agent::crom_settings`), not a file Crom loads. A unit test pins that rendering to `agent/crom-settings.json`.
- Guard's session errors at tool discovery (`grant_revoked`, `no_grant`) end the Crom run but are not surfaced by Crom; the driver labels those rows with Guard's code from its own `tools/list` probe.
- `crom run` (CLI) sets no Guard approver: there, `approval_required` stays a tool error.
