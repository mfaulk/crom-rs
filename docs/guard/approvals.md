# Guard approvals

`MemoryApprovalStore` (`crates/crom-guard/src/approval.rs`, #354) records human approvals for `ApprovalRequired` decisions and is the `ApprovalVerifier` the pipeline consults. Spec: MILESTONE_18 §9.4. Scenarios G8 (approval required, then allowed once) and G9 (approval misuse denied).

"Approval required" is not "authorized". An approval makes one *new* request, for exactly the reviewed action, eligible for Cedar to allow. Cedar still evaluates every grant, every `forbid`, and the `@approval` permit's own conditions.

## Flow

1. The agent calls a protected tool. Cedar's approval probe returns `CallResult::ApprovalRequired { role, action_digest }`. Nothing runs.
2. After writing that decision record, the pipeline calls `ApprovalVerifier::record_pending`. The store keeps a **pending request** keyed by (issuer of the grant, action digest): tenant, leaf grant, action, resource uid, resource version, validated args, the operation's Cedar context fields, the required role, and `created_at`/`expires_at` on Guard's clock.
3. The trusted application's admin path (over HTTP: `GET /admin/approvals/{digest}` and `POST /admin/approvals`, [server.md](server.md)) shows a human exactly that request:

   ```rust
   let view: PendingApproval = store.pending(&issuer_app_id, &action_digest, now)?;
   ```

4. On approval it calls

   ```rust
   store.approve(&issuer_app_id, ApprovalSpec {
       action_digest,      // the reviewed pending request
       approver,           // Delegator from the admin session
       approver_tenant,    // the approver's tenant
       ttl_seconds,        // 1..=max_ttl_seconds (default 900)
   }, now)
   ```

   and gets an `IssuedApproval { approval_id, expires_at }`. `approval_id` is `ap_` plus 32 hex digits from the OS RNG. The pending request is consumed. The approval's resource version and approved fields are copied from it; the admin call cannot set them. A digest with no unexpired pending request from that issuer is `ApprovalError::UnknownRequest`.
5. The agent retries the same call with `CallMeta { approval_id: Some(..) }` (MCP `_meta`).
6. After resolving the resource, the pipeline recomputes `ActionDigest::compute(leaf grant, action, resource uid, validated args)`, reads Guard's clock again, and calls `verify`. Under one lock, the store checks and consumes the approval. It then builds the `ApprovalContext`, which Cedar sees as `context.approval` (`approver` plus the approved fields).
7. Cedar evaluates. On `Allow` the call executes, and the audit decision record carries `approval_id`.

**First pending record wins.** While a pending request for a digest is unexpired, a repeated `ApprovalRequired` for the same digest does not replace it, so the resource version a human reviewed cannot be swapped underneath them. If the resource changes before the retry, the approval no longer matches (`approval_invalid`), and the next request records a fresh pending request.

## What `verify` checks

Any failure is `approval_invalid` (`enforcement: guard_approval`). A failed check does not consume the approval.

| Check | Rejects |
|---|---|
| The id exists | random or forged ids; ids swept after use or expiry |
| Not used | replay (G9) |
| `now < expires_at`, with `now` read just before `verify` | expired approvals, including expiry during resolution |
| `issuer == principal.issuer` | an approval made by another application (defense in depth: `approve` already needs that issuer's pending request) |
| `approver_tenant == principal.tenant` | an approver from another tenant |
| `action_digest` equals the recomputed digest | a different leaf grant, action, resource, or argument (e.g. amount) |
| The pending request's resource version equals the resolved version | an approval of a resource that has since changed |

**Approver tenant.** `ApprovalContext` carries the approver as a `Delegator`, which has no tenant. The PDP therefore sets the approver entity's tenant to the principal's, so the Cedar test `context.approval.approver.tenant == principal.tenant` is always true (see [policies.md](policies.md)). The tenant check above is the real enforcement. It runs before any `ApprovalContext` exists.

## Single use and consumption point (decided)

The approval is consumed in `verify`, **before** Cedar, the decision record, and the recheck. A later deny (a `forbid`, an `@approval` permit whose role or conditions do not hold, a grant revoked before execution, an audit failure) therefore burns the approval, and the human must approve again. This is kept deliberately:

- It fails closed: a burned approval can only cause an extra denial, never an extra execution.
- Check-and-consume is one step under the store's lock, so of concurrent uses exactly one passes `verify` (test `concurrent_uses_execute_once`). Consuming later would need a reserve/commit protocol across the PDP, audit, and recheck steps.
- The approval binds one attempt: it covers the request as evaluated at that moment, not a standing permission to retry.

## Trust boundaries

- **No agent-facing operation creates or changes approvals.** `approve` and `pending` are library calls for the trusted application; neither is a registered operation. The agent supplies only the approval id. Pending requests are written only by the pipeline, from Guard's own evaluation.
- **The admin supplies only who approved and for how long.** What is approved (action, resource, version, args, fields) comes from the pending request.
- **The approval id is an identifier, not a credential.** On its own it authorizes nothing: it works only with the leaf grant's session handle, for the exact digested action. So it is written to audit (`approval_id`) and may be shown to the approver. A presented id that fails `verify` is written nowhere: not to the outcome, the audit record (whose `approval_id` stays empty), or `tracing`.
- **Nothing secret is stored.** `PendingApproval` holds the validated args, which the agent supplied. The store's `Debug` shows only its config. `ApprovalError` messages contain no approval data.

## Store limits

| Setting | Default | Behaviour |
|---|---|---|
| `max_ttl_seconds` | `DEFAULT_APPROVAL_MAX_TTL_SECONDS` = 900 | `ttl_seconds` of 0 or above this is `InvalidSpec`, not shortened |
| `pending_ttl_seconds` | `DEFAULT_PENDING_TTL_SECONDS` = 900 | How long a pending request can be approved |
| `max_approvals` | `DEFAULT_MAX_APPROVALS` = 10 000 | Total cap, applied separately to approvals and to pending requests |
| `max_approvals_per_issuer` | `DEFAULT_MAX_PER_ISSUER` = 1 000 | When the issuer (or the store) is full, `approve` drops used and expired approvals; still full ⇒ `Unavailable`, and the pending request is kept |
| `max_pending_per_issuer` | `DEFAULT_MAX_PER_ISSUER` = 1 000 | When full, recording drops expired pending requests; still full ⇒ the request is not recorded (a `tracing` warning with no request data), and a later `approve` for it is `UnknownRequest`. The agent still gets `approval_required` |

All fields are on `ApprovalStoreConfig`. The store is in memory only: a restart drops every approval and pending request, which fails closed.

`status(issuer, approval_id, now)` returns `Pending`, `Used`, or `Expired`, or `None` if `issuer` has no approval with that id. It is for the admin path, e.g. to show whether an approval was used.

## Not in scope

An approval UI, multi-party approvals, and Crom's `ApprovalCallback`, which is not a Guard approval source. The audit record carries `approval_id` but not the approver; adding the approver to audit (a `guard-audit/v1` format change) is tracked in a follow-up issue. Until then, join on `approval_id` with the application's own record of the admin action.

## Tests

`crates/crom-guard/tests/approvals.rs`:

- a valid approval executes once and is recorded in audit;
- the pending request shows exactly the evaluated action, resource, version, args, fields, and role; another issuer can neither view nor approve it; `approve` fails for an unknown, already-approved, or expired digest;
- replay, random id, different amount, different invoice, changed resource version (including a newer version that does not replace the pending record), different grant, expired (at the boundary millisecond and during resolution), and cross-tenant approver are each `approval_invalid` with no execution;
- Cedar sees the stored approver role, and a Cedar deny burns the approval;
- 16 concurrent uses execute exactly once;
- per-issuer quotas for pending requests and approvals, with sweeping;
- a rejected id is in no outcome, audit record, or `tracing` output.

```sh
cargo test -p crom-guard --test approvals
```
