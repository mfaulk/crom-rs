# Crom Guard core types

`crates/crom-guard` (#347) defines the types and traits the rest of M18 builds on. The architecture is in [ADR 0002](../security/decisions/0002-guard-architecture.md), and the policy model is in [ADR 0003](../security/decisions/0003-guard-policy-engine.md). The spec is [MILESTONE_18](../milestones/MILESTONE_18.md) §8–§10 and §12. Run `cargo doc -p crom-guard --open` for the full API.

This PR freezes the public API. Signature changes after it merges need owner approval.

## Where authority lives

Only three kinds of value carry authority, and only `crom-guard` can construct them. A `compile_fail` doctest checks each one: no public constructor, no `Deserialize`, and no `Clone` where reuse would matter.

| Type | Built by | Meaning |
|---|---|---|
| `ExecutionContext` | the grant store (#348), from an authenticated `SessionHandle` | Holds the live grant chain (leaf first) and the `TrustedPrincipal` derived from it |
| `TrustedPrincipal` | `ExecutionContext` | Agent, tenant, delegator, issuer, run, and grant chain. Every field comes from grant records, never from the request |
| `AuthorizedAction` | the pipeline (#351): `bind(decision_id, &Decision, &AuthorizationRequest)` returns `None` unless the decision is `Allow`, and copies the action, resource, version, args and principal from that request | Proof that one request was allowed. Not `Clone`. It is the only input an executor accepts |

All of `ValidatedArgs`, `AuthorizationRequest` and `ApprovalContext` are crate-built too. So resolvers, the policy decision point (PDP) and executors only ever see schema-validated input and approvals that matched.

Everything else is data. Ids, `GrantSpec`, `Grant` and `ResolvedResource` are publicly constructible, and `Grant` can be deserialized so the store can persist it. None of them authorizes anything by itself.

## Types

| Module | Types | Notes |
|---|---|---|
| ids | `GrantId`, `AgentId`, `RunId`, `DecisionId`, `ApprovalId`, `ActionId`, `TenantId`, `UserId`, `AppId`, `PolicyId` | Validated on construction and on deserialization: 1–128 bytes of `[A-Za-z0-9._:@-]`. The `__guard` prefix is reserved for Guard-internal ids such as the approval placeholder user |
| value | `EntityUid`, `AttrValue`, `Attrs` | Cedar-typed values. Externally tagged in JSON, e.g. `{"long": 5}`. `EntityUid` is validated on deserialization too: the type is a `::` path, and the id is 1–256 bytes with no control characters. Build Cedar UIDs from `entity_type()` and `id()`, never from `Display` |
| grant | `GrantSpec`, `DelegationSpec`, `Grant`, `Delegator`, `Constraints`, `Constraint` | `GrantSpec` is what the app submits, and `DelegationSpec` is what `delegate` accepts; both use `deny_unknown_fields`. `Grant` is the §8.2 record. Its fields are private, so it is immutable except for revocation. `Delegator.attrs` cannot set `id`, `tenant` or `role` |
| handle | `SessionHandle` | `gsh_` plus 64 lowercase hex digits. No `Debug`, `Display`, `Serialize` or `Clone` |
| context | `ExecutionContext`, `TrustedPrincipal`, `ChainError` | Chain checks: parent links, shared tenant/delegator/issuer/run, depth strictly decreasing, schema version 1, a non-empty validity window, and no repeated grant ids |
| resource | `ResolvedResource`, `ResourceVersion` | Built by the trusted resolver. The version is the executor's precondition |
| action | `ValidatedArgs`, `AuthorizationContext`, `ApprovalContext`, `AuthorizationRequest`, `AuthorizedAction` | `AuthorizationContext` rejects the Guard-owned keys `grant` and `approval`. `AuthorizationRequest` carries the validated args, so the PDP can compute the approval digest |
| decision | `Decision`, `DenyReason`, `ApprovalRequirement`, `ActionDigest` | Agents see only `DenyReason::code()`. The full value, including ancestor grant ids, goes to the audit log |
| credential | `CredentialLease`, `CredentialBroker` | The lease exposes its secret only inside `with_secret(\|s\| ..)` |
| audit | `AuditRecord`, `AuditSink`, `AuditReceipt`, `FileAuditSink`, `verify_log`, … | The `guard-audit/v1` fields from §12.1. `FileAuditSink` adds `seq`, `prev_hash` and `hash`, and `verify_log` checks the chain (#352). See [audit.md](audit.md) |
| time | `Timestamp`, `Clock`, `SystemClock` | Unix milliseconds on Guard's clock |

## Traits

| Trait | Sealed | Implemented in | Notes |
|---|---|---|---|
| `GrantStore` | yes | `MemoryGrantStore` ([grants.md](grants.md)) | Async: `issue`, `delegate`, `authenticate`, `recheck` (just before execution), and `revoke`, which is scoped to the issuing app |
| `PolicyDecisionPoint` | yes | #349 | Sync, because evaluation is CPU-only. `authorize` returns a `Decision` and cannot return an error. Engine errors become `Deny` |
| `ResourceResolver` | no | app code, #350 | Async (`BoxFuture`). Not-found and other-tenant both return the same `ResolveError::NotFound` |
| `ProtectedOperation` | no | app code, #351, #356 | One `ActionId` per MCP tool. Provides the input schema, resolver, Cedar context, and `execute(AuthorizedAction, CredentialLease)` |
| `CredentialBroker` | no | #355 | Async. Hands out leases. The agent never receives one |
| `AuditSink` | no | #352 (`ChainedAuditSink` / `FileAuditSink`) | Async. `append` must be durable before it resolves. On `Err`, nothing executes |

Sealed traits produce authority, so implementations outside this crate are rejected at compile time.

## Semantics fixed here

- **Liveness.** A grant is usable only from `not_before` (inclusive) to `expires_at` (exclusive). Any recorded `revoked_at` denies, whatever its timestamp.
- **Chains.** `ExecutionContext::check_live` checks every grant in the chain, so revoking a parent disables all its descendants.
- **Narrowing.** `Constraints::narrows` is the early check for `delegate` (#353):
  - every parent constraint must appear in the child, with the same kind;
  - a set in the child must be a subset of the parent's, and a limit must be ≤ the parent's;
  - the child may add constraints the parent doesn't have.

  Enforcement does not rely on this check. Every grant in a chain is evaluated on its own.
- **`DenyReason` additions.** On top of the variants in MILESTONE_18 §9.2, there are four:
  - `GrantNotYetValid`;
  - `ActionNotGranted`, the Rust action-set check from ADR 0003 §4.3;
  - `ApprovalInvalid`;
  - `AuditUnavailable`.

  `DenyReason`, `Decision` and `AttrValue` are `#[non_exhaustive]`.

## Tests

```sh
cargo test -p crom-guard          # unit tests, compile_fail doctests, tests/audit.rs (tamper, durability, schema)
cargo test -p crom-guard-server   # `crom-guard audit verify` CLI
scripts/check-guard-deps.sh       # no other crom-* dependency
```
