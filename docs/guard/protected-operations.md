# Protected operations: resolver and executor contract

A `ProtectedOperation` is trusted application code (ADR 0002 §2.5). Guard checks what it can, but a resolver that copies attributes from the arguments, or an executor that ignores the version, defeats Guard. This page is the contract those two parts must meet ([#350](https://github.com/mfaulk/crom-rs/issues/350); MILESTONE_18 §10.2 steps 3, 5 and 6). The reference implementation is `crom_guard_testkit::support_ops` ([README](../../crates/crom-guard-testkit/README.md#support_ops)).

## Resolver

`ResourceResolver::resolve(&TenantId, &ValidatedArgs)` (the frozen signature from #347). The tenant is the principal's, taken from the grant record.

- **Identifier only.** Read only the identifier field from the arguments. The uid, the attributes (`tenant`, owner, amounts), the parents and the version all come from trusted application data. Policy therefore never sees a tenant or owner value taken from tool arguments (ADR 0003 §4.2).
- **Tenant-scoped lookup.** Look the id up within `tenant`. Missing and other-tenant both return `ResolveError::NotFound`, so the agent sees `resource_not_found` either way and cannot probe other tenants.
- **Version covers the decision.** `ResolvedResource::version` must change whenever anything the policy reads changes (owner, tenant, amounts). The business API's row version or `ETag` is the usual choice.
- **Bounded.** Wrap the resolver in `TimeoutResolver::new(inner, move || Box::pin(tokio::time::sleep(t)))`. A timeout returns `ResolveError::Unavailable`, which `From<ResolveError> for DenyReason` maps to `engine_unavailable`, a deny. The timer bounds only async work. Do the lookup inside the returned future, not in `resolve` itself, and move blocking work to `spawn_blocking`. The pipeline ([#351](https://github.com/mfaulk/crom-rs/issues/351)) runs resolvers and executors under `catch_unwind`: a panicked resolver is a deny and a panicked executor a failed outcome.
- **Fail closed.** Stored data that cannot be expressed as a Cedar entity returns `Unavailable`.

## Executor

`ProtectedOperation::execute(AuthorizedAction, CredentialLease)`. Build a `ConditionalCall::new(&action, "<Namespace::Type>")`. It fails before any downstream call if the bound resource has the wrong type, if its version is not a valid entity-tag, or if the resource id or tenant is `.` or `..`. URL path builders drop or resolve those segments, which would retarget the call. Then make the downstream call as follows:

| Must | From |
|---|---|
| Target exactly the bound resource | `call.resource_id()`, never an id read again from the arguments |
| Address it under the principal's tenant | `call.tenant()` |
| Condition the effect on the authorized version | `If-Match: call.if_match()` (a strong entity-tag, e.g. `"3"`) |
| Tag it for ledger correlation | `X-Guard-Decision-Id: call.decision_id()` |
| Authenticate with the broker credential | `credential.with_secret(\|s\| req.bearer_auth(s))`. Do not let the secret escape the closure |
| Map the response | `ConditionalCall::check_status(status, resource_missing)` |

`check_status` maps 2xx to success. A `412` means the resource changed after authorization, and maps to `ExecuteError::Precondition` (audit outcome `executor_precondition`). A `404` maps to `Precondition` only when `resource_missing` is set, meaning the downstream marked it as "no such resource in this tenant". The testkit API marks it with `X-Resource-Not-Found: true`. Every other status maps to `ExecuteError::Failed`, including an unmarked `404` (such as a wrong route) and `428 Precondition Required`.

The downstream API must perform no effect when the condition fails. With that, a change in ownership or state between authorize and execute (TOCTOU) produces no effect. The testkit business API meets this requirement, and its tests check that no ledger entry is written.

**Errors carry no secrets or data.** Guard never emits `ExecuteError::Failed` text: agents see `execution_failed`, and `tracing` gets only the text's length and a SHA-256 prefix. Still, do not put credentials, tokens, response bodies, or resource content in error strings. Operator tooling may log them on the executor's side. Errors such as `reqwest::Error` should go through `without_url()` or an equivalent.

**Run to completion.** Executors should bound their own downstream calls (request timeouts). A host that cancels a `Guard::call` after its decision record latches Guard closed (see [pipeline.md](pipeline.md)).

Non-identifier inputs, such as `amount_cents`, come from `call.args()`. They are the same validated arguments the policy evaluated.

**Resource type must match the effect's scope.** An executor may affect only the resource that was authorized. An operation whose downstream effect is wider than its Cedar resource must not execute. The reference `customer.export` is typed on one `Customer`, but the export is tenant-wide, so its executor always fails closed without a downstream call. Making it executable needs a `Tenant`-typed resource, which is a schema change.

## Tests

- `crates/crom-guard/src/executor.rs` and `resolution.rs`: unit tests with an in-memory resolver and downstream. They cover resolve → bind → change → execute, the not-found mapping, timeouts, and header-safe versions.
- `crates/crom-guard-testkit/src/support_ops.rs`: the reference operations against the HTTP business API. A forged `tenant_id` is rejected by the schema. A tenant-B id is not found. A change of ownership, or a move to another tenant, after resolution yields `Precondition` and no ledger effect.

- `crates/crom-guard-testkit/tests/pipeline_e2e.rs`: the same scenarios end to end through `Guard::call` and `SupportOperation::execute` ([#351](https://github.com/mfaulk/crom-rs/issues/351), [pipeline.md](pipeline.md)). An allowed refund leaves exactly one ledger effect carrying the decision id. A change of ownership between authorize and execute yields `precondition_failed` and no effect.

The unit tests in `support_ops.rs` call the resolver lookup and the HTTP layer directly, using the values `ConditionalCall` would derive, because only the pipeline builds a `ValidatedArgs` or an `AuthorizedAction`.
