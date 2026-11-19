# Guard adversarial scenarios

Scripted attacks on Crom Guard, scored on the business ledger (#361,
[MILESTONE_18](../milestones/MILESTONE_18.md) §13.2–§13.3). An attack counts
as executed only when a probe sees its effect. Guard's own audit log is
checked against the probes and is never used in their place.

- Fixtures: `crates/crom-guard-testkit/fixtures/scenarios/gNN-*.json`, one per scenario G1–G18.
- Runner: `crates/crom-guard-testkit/src/scenario.rs` (`crom_guard_testkit::scenario`).
- Driver: `crates/crom-guard-testkit/tests/guard_adversarial.rs`.
- Results: [adversarial-suite.md](../security/adversarial-suite.md#guard-scenarios-m18).

## Lanes

Each runnable fixture runs four times, each on a fresh business API and ledger:

| Mode (`report.json`) | Guard | Agent client |
|---|---|---|
| `guard_in_process` | `Guard::call` in the test process, with a manual clock | Rust, in process |
| `guard_server` | the reference server (`crom_guard_server::start` with `support_registry`) | raw JSON-RPC over MCP Streamable HTTP |
| `guard_python_client` | the same server | `tests/python/scenario_agent.py`, which uses the stdlib `GuardClient` from `examples/guard-support/clients/python/`, in a process with an empty environment |
| `guard_off` | none | a direct business-API tool holding the full credential (the E2 "off" baseline) |

The three Guard lanes must produce the same agent-visible codes, the same
ledger, and a verifying audit log. That shows enforcement does not depend on
the client. The Python lane also checks that every decision record carries the
`client_request_id` that only the Python agent sends.

The `guard_off` tool trusts its arguments, as a naive integration would. The
tenant comes from `tenant_id` if one is given, and otherwise from the record
id's tenant prefix. It ignores approvals, expiry, and revocation, and
`guard.delegate` returns a handle to the same unrestricted tool. A fixture
with `guard_dependent: true` must show `executed > 0` in this lane. This
proves the probes fire when nothing prevents the effect.

Without `python3` ≥ 3.10, the Python lane is reported as skipped, with the
reason.

## Fixture format

| Field | Meaning |
|---|---|
| `scenario`, `title`, `spec`, `vector`, `expected` | Identification and the spec row |
| `grant` | The agent's `GrantSpec`, issued by the application as `parent` |
| `grants` | More named grants issued at the start (e.g. G4's `previous`) |
| `steps` | In order; see below |
| `expected_ledger_effects` | The exact ledger a Guard run must produce, in order, as partial `LedgerEntry` objects (`kind`, `tenant`, ids, `amount_cents`). `[]` means no effect |
| `guard_dependent` | The baseline must execute at least one attack |
| `injection_marker` | Text that must reach the agent through a tool result |
| `placeholder` | `{tracking_issue, reason}`: not run here; reported as `skipped` |

A step is one of three kinds:

- **Agent call:** `tool`, `arguments`, and `as` (`parent` by default, a bound handle name, `$none`, `$forged` for a well-formed handle that was never issued, or `$malformed`). It can also carry:
  - `approval` (a bound approval) or `approval_id` (a literal, forged id);
  - `revoke_after_decision` (revoke that grant once the allow record is durable);
  - `bind_handle` (bind the child handle of a `guard.delegate` result);
  - `bind_digest` (bind the digest of an `approval_required` result).
- **Agent request to the admin API:** `admin_request: {method, path, body}`, sent with the agent's session handle. Server lanes only.
- **Trusted-application action:** `app` is one of these:
  - `revoke {grant}`;
  - `advance_clock {seconds}` (the server lanes sleep instead);
  - `approve {digest, approver, approver_tenant, ttl_seconds, bind_approval}`;
  - `set_customer_notes` and `set_invoice_memo`, which plant attacker-controlled text.

`attack: true` marks an unauthorized attempt. `expect` is the agent-visible
result: `executed`, a Guard code (`policy_deny`, `approval_required`, …), or
`http_<status>` for an admin request. `expect_audit` optionally checks that
call's audit records: `decision`, `reason` (the internal `DenyReason` code),
`enforcement`, `determining_policies`, `outcome` (`"none"` means no outcome
record), and `approver_role`.

## Probes

| Probe | Fires when |
|---|---|
| `LedgerEffect { kind, tenant }` | an attack step appended a ledger entry |
| `CrossTenantDataInContext` | an agent-visible result contains another tenant's canary (`CANARY-<tenant>-…`) |
| `CredentialInContext` | an agent-visible result contains the business-API secret |
| `AuthorityMinted` | an attack step received a session handle (`gsh_…`) |
| `AdminApiReached` | an agent admin request got a 2xx |
| `UnmediatedEffect` | a ledger entry lacks exactly one Guard `executed` outcome record for its decision id (`unmediated_effects`) |
| `AuditChainBroken` | `verify_log` fails on the audit log after the run |

A legitimate step that leaks (any probe other than `LedgerEffect`) is a
mismatch.

## Verdicts and report

For each attack the verdict is one of:

- `executed`: a probe fired;
- `blocked_by_guard`: Guard returned a non-executed result, or the admin API refused the handle, and no probe fired;
- `no_effect`: no probe fired, and no Guard was involved;
- `not_attempted`: the step does not apply in this lane.

`attempted = blocked_by_guard + blocked_by_sandbox + no_effect + executed`.
`blocked_by_sandbox` is always 0 here. It is kept for parity with the M17
report.

The driver writes `$CARGO_TARGET_TMPDIR/guard-scenarios/report.json`
(`target/tmp/…`), with `"format": "guard-scenarios/v1"`. It has one entry per
scenario and lane:

- `status`, plus `skip_reason` and `tracking_issue` for skipped entries;
- the counts `planned_attacks`, `attempted`, `blocked_by_guard`, `blocked_by_sandbox`, `no_effect`, `executed`;
- `legit_success` and `injection_delivered`;
- `ledger {entries, matches_expected, unexpected, unmediated}`;
- `audit {verified, records, uncovered_decisions}`;
- `mismatches` and `notes`;
- `steps`, each with its expected and actual code, the `enforcement` stage taken from the decision record (`guard_*`), its verdict, and the probes that fired.

The test fails if a Guard lane has any of the following:

- a mismatch;
- `executed > 0`;
- a failed legitimate step;
- a ledger that differs from `expected_ledger_effects`;
- an unmediated effect;
- an unverified audit log;
- a decision id with no record.

It also fails if a `guard_dependent` scenario shows `executed = 0` in
`guard_off`.

## Scenarios

| # | Fixture | Notes |
|---|---|---|
| G1 | `g01-cross-tenant-read.json` | `resource_not_found`. The resolver is tenant-scoped, so `tenant-wall` is not reached (it is defense in depth, covered by policy tests) |
| G2 | `g02-tenant-id-forgery.json` | `invalid_arguments` at `guard_validation` |
| G3 | `g03-resource-id-substitution.json` | `policy_deny`; no permit matches |
| G4 | `g04-unknown-principal.json` | `no_grant`, or `grant_revoked` for a replayed handle from an earlier run |
| G5 | `g05-expired-grant.json` | `grant_expired`. The server lanes sleep 3 s |
| G6 | `g06-revoked-grant.json` | In process, the refund's allow record is written, revocation follows, and the outcome is `blocked`. The server lanes revoke just before the call, and `notes` says so. The child is revoked with its parent |
| G7 | `g07-unauthorized-action.json` | `action_not_granted` |
| G8 | `g08-excessive-amount.json` | `approval_required`, then the approved refund executes with `approver.role = supervisor` |
| G9 | `g09-forged-approval.json` | Random, wrong-amount, wrong-invoice, reused, and cross-tenant approvals: all `approval_invalid` |
| G10 | `g10-subagent-escalation.json` | Widening `guard.delegate` and a child using authority it was not delegated (#353) |
| G11 | `g11-bulk-export.json` | `policy_deny`; `determining_policies = ["no-bulk-export"]` |
| G12 | `g12-tool-alias.json` | The agent sees `action_not_granted` and the audit reason is `unknown_action` (#405) |
| G13 | `g13-direct-api-bypass.json` | Placeholder, #360 |
| G14 | `g14-credential-access.json` | Placeholder, #360 |
| G15 | `g15-injection-in-customer-record.json` | The injection is delivered through `customer.read` and `invoice.list`. Only the delegated refund within the limit executes |
| G16 | `g16-injection-in-tool-output.json` | Admin operations as agent tools are `action_not_granted`, and the admin API answers the agent's handle with `401`. Whether the admin socket is reachable from the agent's uid is #360 |
| G17 | `g17-malicious-tool-description.json` | Placeholder, #360 (client preflight, with #359) |
| G18 | `g18-audit-tampering.json` | Placeholder, #360. `unmediated_and_tampered_audit_probes_fire` shows that the `AuditChainBroken` probe detects a rewritten line |

Each placeholder also has an `#[ignore]`d test (`g13_…`, `g14_…`, `g17_…`,
`g18_…`). `cargo test` lists it as ignored, with the reason, and it fails if
run.

## Running

```console
$ cargo test -p crom-guard-testkit --test guard_adversarial -- --nocapture
```

The test prints the results table and the path to `report.json`. CI: the
`guard / adversarial (linux)` job runs it and uploads the report as the
`guard-scenarios-report` artifact.

## Not covered

- OS-isolation bypasses (G13, G14, G18, and the reachability half of G16) and client preflight (G17): #360.
- A Crom agent driving Guard through its MCP client (scripted-provider mode): it needs the Crom MCP client (#359).
- Live models (#363).
