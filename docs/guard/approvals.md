# Guard approvals

`MemoryApprovalStore` (`crates/crom-guard/src/approval.rs`, #354) records human approvals for `ApprovalRequired` decisions and is the `ApprovalVerifier` the pipeline consults. Spec: MILESTONE_18 §9.4. Scenarios G8 (approval required, then allowed once) and G9 (approval misuse denied).

"Approval required" is not "authorized". An approval makes one *new* request, for exactly the digested action, eligible for Cedar to allow. Cedar still evaluates every grant, every `forbid`, and the `@approval` permit's own conditions.

## Flow

1. The agent calls a protected tool. Cedar's approval probe returns `CallResult::ApprovalRequired { role, action_digest }`. Nothing runs.
2. A human approves through the trusted application's admin path (the HTTP admin route is #357). The application calls

   ```rust
   store.approve(&issuer_app_id, ApprovalSpec {
       action_digest,               // from step 1
       resource_version,            // the version the approver saw
       approver,                    // Delegator from the admin session
       approver_tenant,             // the approver's tenant
       fields,                      // e.g. {"amount_cents": Long(7000)}
       ttl_seconds,                 // 1..=max_ttl_seconds (default 900)
   }, now)
   ```

   and gets an `IssuedApproval { approval_id, expires_at }`. `approval_id` is `ap_` plus 32 hex digits from the OS RNG.
3. The agent retries the same call with `CallMeta { approval_id: Some(..) }` (MCP `_meta`).
4. After resolving the resource, the pipeline recomputes `ActionDigest::compute(leaf grant, action, resource uid, validated args)` and calls `verify`. Under one lock, the store checks and consumes the approval. It then builds the `ApprovalContext`, which Cedar sees as `context.approval` (`approver` plus `fields`).
5. Cedar evaluates. On `Allow` the call executes, and the audit decision record carries `approval_id`.

## What `verify` checks

Any failure is `approval_invalid` (`enforcement: guard_approval`). A failed check does not consume the approval.

| Check | Rejects |
|---|---|
| The id exists | random or forged ids; ids swept after use or expiry |
| Not used | replay (G9) |
| `now < expires_at` on Guard's clock | expired approvals |
| `issuer == principal.issuer` | an approval made by another application |
| `approver_tenant == principal.tenant` | an approver from another tenant |
| `action_digest` equals the recomputed digest | a different leaf grant, action, resource, or argument (e.g. amount) |
| `resource_version` equals the resolved version | an approval of a resource that has since changed |

**Approver tenant.** `ApprovalContext` carries the approver as a `Delegator`, which has no tenant. The PDP therefore sets the approver entity's tenant to the principal's, so the Cedar test `context.approval.approver.tenant == principal.tenant` is always true (see [policies.md](policies.md)). The tenant check above is the real enforcement. It runs before any `ApprovalContext` exists.

## Single use and consumption point (decided)

The approval is consumed in `verify`, **before** Cedar, the decision record, and the recheck. A later deny (a `forbid`, an `@approval` permit whose fields do not match, a grant revoked before execution, an audit failure) therefore burns the approval, and the human must approve again. This is kept deliberately:

- It fails closed: a burned approval can only cause an extra denial, never an extra execution.
- Check-and-consume is one step under the store's lock, so of concurrent uses exactly one passes `verify` (test `concurrent_uses_execute_once`). Consuming later would need a reserve/commit protocol across the PDP, audit, and recheck steps.
- The approval binds one attempt: it covers the request as evaluated at that moment, not a standing permission to retry.

## Trust boundaries

- **No agent-facing operation creates or changes approvals.** `approve` is a library call for the trusted application; it is not a registered operation. The agent supplies only the approval id. The approver, tenant, fields, and version come from the store.
- **The approval id is an identifier, not a credential.** On its own it authorizes nothing: it works only with the leaf grant's session handle, for the exact digested action. So it is written to audit (`approval_id`) and may be shown to the approver. A presented id that fails `verify` is written nowhere: not to the outcome, the audit record (whose `approval_id` stays empty), or `tracing`.
- **Nothing secret is stored.** The store's `Debug` shows only its config. `ApprovalError` messages contain no approval data.

## Store limits

| Setting | Default | Behaviour |
|---|---|---|
| `ApprovalStoreConfig::max_ttl_seconds` | `DEFAULT_APPROVAL_MAX_TTL_SECONDS` = 900 | `ttl_seconds` of 0 or above this is `InvalidSpec`, not shortened |
| `ApprovalStoreConfig::max_approvals` | `DEFAULT_MAX_APPROVALS` = 10 000 | When full, `approve` drops used and expired approvals; still full ⇒ `Unavailable` |

`fields` may not contain `approver` (`InvalidSpec`). The store is in memory only: a restart drops every approval, which fails closed.

`status(issuer, approval_id, now)` returns `Pending`, `Used`, or `Expired`, or `None` if `issuer` has no approval with that id. It is for the admin path, e.g. to show whether an approval was used.

## Not in scope

The approval UI and admin HTTP route (#357), multi-party approvals, and Crom's `ApprovalCallback`, which is not a Guard approval source. The audit record carries `approval_id` but not the approver: join on `approval_id` with the application's own record of the admin action. Adding an `approver` audit field would change the `guard-audit/v1` record format.

## Tests

`crates/crom-guard/tests/approvals.rs`: a valid approval executes once and is recorded; replay, random id, different amount, different invoice, changed resource version, different grant, expired (and the boundary millisecond), cross-tenant approver, and other-issuer approval are each `approval_invalid` with no execution; Cedar sees the stored approver role and approved fields, and a Cedar deny burns the approval; 16 concurrent uses execute exactly once; a rejected id is in no outcome, audit record, or `tracing` output. Unit tests in `approval.rs` cover id format and capacity.

```sh
cargo test -p crom-guard --test approvals
```
