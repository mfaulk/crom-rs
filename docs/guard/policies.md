# Guard policies and the Cedar PDP

`CedarPdp` in `crates/crom-guard/src/pdp/` (#349) is Guard's policy decision point. It implements the sealed `PolicyDecisionPoint` trait with `cedar-policy = "=4.15.0"`, default features only. The design is [ADR 0003](../security/decisions/0003-guard-policy-engine.md) §4.4–§4.5. The reference schema and policies are in `examples/guard-support/policies/`.

## Loading

```rust
let pdp = crom_guard::CedarPdp::from_files(
    "Support",                                   // Cedar namespace of the schema
    Path::new("policies/support.cedarschema"),
    Path::new("policies/support.cedar"),
)?;                                              // Err(PdpLoadError) => do not start
```

`from_sources(namespace, schema_src, policy_src)` takes the text directly. The policy set and schema are loaded once. There is no API that changes them afterwards, and no hot reload.

Loading fails with `PdpLoadError` if any of these is true:

- A source is larger than `MAX_POLICY_SOURCE_BYTES` (1 MiB), or a file can't be read.
- The namespace is not a `::` path, or the schema does not declare `<ns>::Agent`, `<ns>::User`, and `<ns>::Tenant`.
- The schema doesn't parse, or parsing it produces warnings.
- The policy text doesn't parse, or contains templates.
- A policy has no `@id`, or its `@id` is not a valid `PolicyId` (`[A-Za-z0-9._:@-]`, 1–128 bytes, no `__guard` prefix), or two policies share an `@id`. Policies are re-keyed by `@id`, so `determining_policies` and audit records use these names.
- Strict validation (`ValidationMode::Strict`) reports any error or warning. This is the same check `scripts/validate-guard-policies.sh` runs in CI.
- An `@approval("<role>")` annotation:
  - is on a `forbid`;
  - has an empty role, or one outside `[A-Za-z0-9_.-]`;
  - is on a permit whose text does not contain both `context has approval` and `context.approval.approver.role == "<role>"`. Whitespace is normalized before this check.

## Writing policies

- Give every policy a unique `@id`.
- Use the v1 entity model (ADR 0003 §4.2). Guard builds these entities for each request:

  | Entity | Attributes | Source |
  |---|---|---|
  | `Agent::"<agent>"` (principal) | `tenant`, `delegator` | Grant record |
  | `User::"<delegator>"` | `tenant`, `role`, plus the delegator's `attrs`. Member of its `Tenant` if the schema allows `User in [Tenant]` | Grant record |
  | `Tenant::"<tenant>"` | none | Grant record |
  | The resource | Resolver attributes and direct parents | `ResourceResolver` |
  | `User::"<approver>"` (approved requests only) | `tenant` (the principal's), `role`, `attrs` | Stored approval |

  Entities the resource only *references* are not in the store. That covers its parents and values such as `resource.customer`. A policy can compare them (`context.grant.customers.contains(resource.customer)`), but it can't dereference them (`resource.customer.tenant`). If it does, evaluation errors and the request is denied.
- `context` has these fields:
  - the operation's fields (e.g. `amount_cents`);
  - `grant`, the `GrantCtx` record built from the grant's `Constraints`: sets become `Set<Entity>`, limits become `Long`;
  - `approval`, present only when a stored approval matched. It holds `approver` plus the approval's fields.

  The action's declared context must match this shape exactly, or the request is denied.
- For an approvable permit, annotate it `@approval("<role>")`. Inside it, test `context has approval`, `context.approval.approver.role == "<role>"`, and every field the approval must match. Also test `context.approval.approver.tenant == principal.tenant`.

## Evaluation

`evaluate_chain(&AuthorizationRequest) -> Evaluation` returns the decision, the main-evaluation result for each grant, and any error messages. `authorize` returns only the decision. The steps run in this order:

1. **Rust checks.** If any grant in the chain lacks the action → `Deny(ActionNotGranted)`. If the schema does not declare the action → `Deny(UnknownAction)`.
2. **Main evaluation.** The full policy set is evaluated once per grant, leaf first, each time with that grant's `context.grant`. If every grant allows → `Allow`, with the sorted union of the satisfied permits.
3. **Deny classification.** The first grant that denies decides. The reason is `PolicyDeny` if that grant is the leaf, or `ChainDeny { grant_id }` if it is an ancestor. If a `forbid` determined the deny, or the request carried an approval, that deny is the result.
4. **Approval probe.** The probe set is every `forbid` plus every `@approval` permit. For each `@approval` role, in policy order, it is evaluated against every grant in the chain. The probe adds a placeholder approver, `User::"__guard_approval_placeholder__"`: role = the annotation value, tenant = the principal's, and the delegator's other attributes zeroed. `context.approval` is set to `{approver: placeholder}` plus the request's operation fields.
   - If every grant allows → `ApprovalRequired { role, action_digest }`.
   - If the action's context does not accept that approval record, or no role succeeds, the deny from step 3 stands.
   - The probe never returns `Allow`.

### Fail-closed rules

| Rule | Result | Test (`crates/crom-guard/src/pdp/tests.rs`) |
|---|---|---|
| Any Cedar diagnostic error, even when Cedar decided Allow | `Deny(PolicyError)`, with the erroring policy ids | `erroring_forbid_denies_even_when_cedar_allows`, `erroring_permit_denies` |
| An error during the approval probe | `Deny(PolicyError)` | `erroring_probe_denies` |
| A policy dereferences an entity missing from the store | `Deny(PolicyError)` | `missing_entity_denies` |
| Context, grant record, or resource fails schema validation | `Deny(PolicyError)` | `schema_invalid_context_denies`, `schema_invalid_grant_record_denies`, table rows 43–45 |
| Approver and delegator share an id but differ | `Deny(PolicyError)` | `conflicting_approver_entity_denies` |
| An input uses the reserved `__guard` id prefix | `Deny(PolicyError)` | table row 46 |
| An action missing from any grant, or undeclared in the schema | `Deny(ActionNotGranted)` / `Deny(UnknownAction)` | table rows 04, 39, 42 |
| A schema or policy set that fails to parse or validate (warnings included), or has bad `@id`/`@approval` | `PdpLoadError`, so Guard does not start | `refuses_*` |

## Hashes and audit

`policy_set_hash()` and `schema_hash()` are the lowercase hex SHA-256 of the exact source text loaded. They go into `AuditRecord.policy_set_hash` and `schema_hash`, so each decision can be tied to the policy text that made it. `Evaluation.errors` can contain request values. It is meant for the audit log only, never for the agent.

## Action digest

`ActionDigest::compute(grant_id, action, resource_uid, args)` is the value an approval is bound to (MILESTONE_18 §9.4). The PDP uses the leaf grant's id. The approval store (#354) must call this same function. The byte encoding is documented on the function: a domain tag, then length-prefixed fields, with canonical JSON for the arguments (object keys sorted).

## Latency

To measure it:

```sh
cargo test --release -p crom-guard pdp_latency -- --ignored --nocapture
```

Each figure is full `authorize` time, entity construction included. Built in release mode, run on one thread, n = 20 000, on the ADR 0003 host:

| Case | p50 | p95 | p99 |
|---|---|---|---|
| Allow (refund within limits) | 35 µs | 41 µs | 44 µs |
| ApprovalRequired (main evaluation + probe) | 66 µs | 74 µs | 82 µs |
| Allow with an approval | 43 µs | 51 µs | 55 µs |
| Deny by `tenant-wall` | 29 µs | 33 µs | 38 µs |

Loading the support set takes about 0.6 ms.
