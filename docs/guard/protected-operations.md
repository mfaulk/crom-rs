# Protected operations: resolver and executor contract

A `ProtectedOperation` is trusted application code (ADR 0002 §2.5). Guard checks what it can, but a resolver that copies attributes from the arguments, or an executor that ignores the version, defeats Guard. This page is the contract those two parts must meet ([#350](https://github.com/mfaulk/crom-rs/issues/350); MILESTONE_18 §10.2 steps 3, 5 and 6). The reference implementation is `crom_guard_testkit::support_ops` ([README](../../crates/crom-guard-testkit/README.md#support_ops)).

## Resolver

`ResourceResolver::resolve(&TenantId, &ValidatedArgs)` (the frozen signature from #347). The tenant is the principal's, taken from the grant record.

- **Identifier only.** Read only the identifier field from the arguments. The uid, the attributes (`tenant`, owner, amounts), the parents and the version all come from trusted application data. Policy therefore never sees a tenant or owner value taken from tool arguments (ADR 0003 §4.2).
- **Tenant-scoped lookup.** Look the id up within `tenant`. Missing and other-tenant both return `ResolveError::NotFound`, so the agent sees `resource_not_found` either way and cannot probe other tenants.
- **Version covers the decision.** `ResolvedResource::version` must change whenever anything the policy reads changes (owner, tenant, amounts). The business API's row version or `ETag` is the usual choice.
- **Bounded.** Wrap the resolver in `TimeoutResolver::new(inner, move || Box::pin(tokio::time::sleep(t)))`. A timeout returns `ResolveError::Unavailable`, which `From<ResolveError> for DenyReason` maps to `engine_unavailable`, a deny.
- **Fail closed.** Stored data that cannot be expressed as a Cedar entity returns `Unavailable`.

## Executor

`ProtectedOperation::execute(AuthorizedAction, CredentialLease)`. Build a `ConditionalCall::new(&action, "<Namespace::Type>")`. It fails before any downstream call if the bound resource has the wrong type, or if its version is not a valid entity-tag. Then make the downstream call as follows:

| Must | From |
|---|---|
| Target exactly the bound resource | `call.resource_id()`, never an id read again from the arguments |
| Address it under the principal's tenant | `call.tenant()` |
| Condition the effect on the authorized version | `If-Match: call.if_match()` (a strong entity-tag, e.g. `"3"`) |
| Tag it for ledger correlation | `X-Guard-Decision-Id: call.decision_id()` |
| Authenticate with the broker credential | `credential.with_secret(\|s\| req.bearer_auth(s))`. Do not let the secret escape the closure |
| Map the response | `ConditionalCall::check_status(status)` |

`check_status` maps 2xx to success. `412` and `404` mean the resource changed or left the tenant after authorization, and map to `ExecuteError::Precondition` (audit outcome `executor_precondition`). Every other status, including `428 Precondition Required`, maps to `ExecuteError::Failed`.

The downstream API must perform no effect when the condition fails. With that, a change in ownership or state between authorize and execute (TOCTOU) produces no effect. The testkit business API meets this requirement, and its tests check that no ledger entry is written.

Non-identifier inputs, such as `amount_cents`, come from `call.args()`. They are the same validated arguments the policy evaluated.

## Tests

- `crates/crom-guard/src/executor.rs` and `resolution.rs`: unit tests with an in-memory resolver and downstream. They cover resolve → bind → change → execute, the not-found mapping, timeouts, and header-safe versions.
- `crates/crom-guard-testkit/src/support_ops.rs`: the reference operations against the HTTP business API. A forged `tenant_id` is rejected by the schema. A tenant-B id is not found. A change of ownership, or a move to another tenant, after resolution yields `Precondition` and no ledger effect.

Until the pipeline ([#351](https://github.com/mfaulk/crom-rs/issues/351)) exists, code outside `crom-guard` cannot build a `ValidatedArgs` or an `AuthorizedAction`. The testkit tests therefore call the resolver lookup and the HTTP layer directly, using the values `ConditionalCall` would derive. The `AuthorizedAction` → `ConditionalCall` step is tested inside `crom-guard`.
