# ADR 0003: Guard policy engine, Cedar entity model, and approval semantics

- **Status:** Proposed — requires owner sign-off and security review (`needs-security-review`)
- **Date:** 2026-11-19
- **Issue:** #346 · consumed by #347 (core types), #349 (PDP), #350 (resolution), #353 (attenuation), #354 (approvals), #362 (reference app)
- **Spec:** [MILESTONE_18](../../milestones/MILESTONE_18.md) §9.1–§9.4; architecture in [ADR 0002](0002-guard-architecture.md)
- **Artifacts:** [`examples/guard-support/policies/support.cedarschema`](../../../examples/guard-support/policies/support.cedarschema), [`support.cedar`](../../../examples/guard-support/policies/support.cedar), [`scripts/validate-guard-policies.sh`](../../../scripts/validate-guard-policies.sh)

## 1. Context

Crom's only rule language is `ToolRule` (`crates/crom-core/src/tool_rule.rs:10`):
tool name and argument pattern matching. It cannot express "this refund of
this amount on this tenant's invoice". MILESTONE_18 §9.1 recommends Cedar
(`cedar-policy` 4.x) for Guard's policy decision point. Three questions change
the API that #347 freezes and so had to be settled by a spike first:

1. How grant constraints reach Cedar: a context record, a `Grant` entity, or a
   Rust pre-check.
2. How `ApprovalRequired` is derived without conflating "an approval would
   help" with "authorized".
3. Cost in this workspace: compile time, decision latency, dependency count.

Invariants the design must keep (issue #346): every non-`Allow` path is
non-executing, and no policy text, schema, or entity data originates from the
agent.

## 2. Spike setup

- Throwaway crate **outside the repository** (not a workspace member, not
  merged). Source in Appendix A.
- `cedar-policy = "=4.15.0"` (crates.io `rust-version = 1.89`), built and run
  with rustc **1.89.0**; `cedar-policy-cli` 4.15.0 for the CLI checks.
- Host: AMD Ryzen Threadripper PRO 5995WX (96 logical CPUs), Linux 7.3.3
  (Fedora 44). `RUSTC_WRAPPER` unset (no sccache) for every timing.
- Input: the v1 schema and policy set committed with this ADR.

Public API used and verified to compile against 4.15.0:
`Schema::from_cedarschema_str`, `PolicySet::from_str`, `PolicySet::add`,
`PolicySet::policies`, `Policy::annotation`, `Policy::new_id`, `Policy::effect`,
`Validator::new`, `Validator::validate(&PolicySet, ValidationMode::Strict)`,
`ValidationResult::{validation_passed, validation_errors, validation_warnings}`,
`EntityUid::from_json`, `Entity::from_json_value`, `Entities::from_json_value`,
`Entities::from_entities`, `Context::from_json_value(_, Some((&schema, &action)))`,
`Request::new(_, _, _, _, Some(&schema))`, `Authorizer::is_authorized`,
`Response::{decision, diagnostics}`, `Diagnostics::{reason, errors}`.
`Context::from_pairs` exists for building context without JSON. Partial
evaluation (`Authorizer::is_authorized_partial`) exists only behind the
experimental `partial-eval` feature.

## 3. Measured evidence

### 3.1 Schema and policies

The MILESTONE_18 §9.3 text needed three changes before it would validate or
be safe; the committed v1 includes them:

| Change | Why |
|---|---|
| Declare `action "customer.export"` (principal `Agent`, resource `Customer`) | `no-bulk-export` references it; strict validation rejects an undeclared action |
| `refund-with-supervisor-approval` adds `context.amount_cents > 0` | Without it, a refund of −500 with a matching −500 approval is permitted (decision table case 17) |
| `refund-with-supervisor-approval` adds `context.approval.approver.tenant == principal.tenant` | The tenant wall checks only the resource; a supervisor of another tenant could otherwise approve (case 10) |

Approval fields are named as a schema type (`ApprovalCtx`) rather than inline.
Result: `cedar validate --validation-mode strict --deny-warnings` passes with no
errors or warnings; a single misspelt attribute (`refund_limt_cents`) fails
with exit code 3. `cedar format --check` passes (the file is
formatter-normalized). Load cost (schema parse + policy parse + strict
validation): **0.56 ms** (release, mean of 100).

### 3.2 Grant constraints: context record vs `Grant` entity

Both validate in strict mode **with identical policy text**
(`context.grant.customers.contains(...)`): in the entity variant, `GrantCtx` is
declared as an entity and `context.grant` holds its UID.

| Variant | `is_authorized` p50 | p95 | Entity store build |
|---|---|---|---|
| `context.grant` record | 7.10 µs | 7.26 µs | 13 entities |
| `Grant` entity in context | 7.37 µs | 7.51 µs | 14 entities (+~23 µs via JSON) |

Release build, n = 100 000, allow decision on `refund.issue`, 5-policy set.
Second run within 4 %.

### 3.3 Action-set membership: Rust vs Cedar

Encoding `actions: Set<Action>` in the grant record is accepted by the schema
parser, and `forbid (principal, action, resource) unless {
context.grant.actions.contains(action) };` passes strict validation. Cedar can
therefore express it. The Rust check is still chosen (§4.3).

### 3.4 Error semantics

- **Erroring `forbid` is skipped.** Adding a validating forbid whose condition
  overflows (`context.amount_cents * 4611686018427387904 > 0`) to the v1 set:
  raw Cedar returns `decision = Allow`, `reason = ["refund-within-limits"]`,
  one error in diagnostics. This is the fail-open MILESTONE_18 §9.2 warns about.
  The spike's Guard wrapper returns `Deny(PolicyError(... integer overflow ...))`.
- **Strict validation does not remove runtime errors.** Remaining sources seen
  or possible: integer overflow, an entity referenced but absent from the
  entity store (spike: omitting the delegator `User` gives
  `entity Support::User::"alice" does not exist` → deny), and extension
  function errors.

### 3.5 Approval derivation: decision table

`decide()` in Appendix A implements §4.4. The grant is
`{customers: [c1], max_refund_cents: 10000}` with all four actions; the
delegator's `refund_limit_cents` is 5000. Invoice `i1`: c1, 200.00, nothing
refunded. `i3`: c1, 100.00, 90.00 refunded. `i2`: c2 (t1, not delegated).
`i9`: tenant t2 but `customer = c1`, so only the tenant wall stops it. The
"naive probe" column re-evaluates with **only** the `@approval` permits and
without the "a forbid determined the deny" shortcut, which is the literal
reading of MILESTONE_18 §9.4 step 1.

| # | Case | Guard decision | Naive probe |
|---|---|---|---|
| 1 | read delegated customer | Allow (`read-delegated-customers`) | same |
| 2 | read undelegated same-tenant customer | Deny (no permit) | same |
| 3 | read other-tenant customer | Deny (`tenant-wall`) | Deny (no permit) |
| 4 | refund on a read-only grant | Deny (action not in grant, Rust) | same |
| 5 | refund 30.00, within all limits | Allow (`refund-within-limits`) | same |
| 6 | refund 80.00 > delegator limit 50.00 | **ApprovalRequired(supervisor)** | same |
| 7 | refund 80.00 + supervisor approval for 80.00 | Allow (`refund-with-supervisor-approval`) | same |
| 8 | refund 80.00 + approval for 70.00 | Deny | same |
| 9 | refund 80.00 approved by a non-supervisor | Deny | same |
| 10 | refund 80.00 approved by another tenant's supervisor | Deny | same |
| 11 | refund 20.00 > remaining balance 10.00 | Deny | same |
| 12 | case 11 with a supervisor approval | Deny | same |
| 13 | refund on an undelegated customer's invoice | Deny | same |
| 14 | refund 10.00 on another tenant's invoice | Deny (`tenant-wall`) | **ApprovalRequired** |
| 15 | refund 80.00 on another tenant's invoice | Deny (`tenant-wall`) | **ApprovalRequired** |
| 16 | negative refund | Deny | same |
| 17 | negative refund + matching approval | Deny | same |
| 18 | chain: child max 20.00, refund 30.00 | ApprovalRequired(supervisor) | same |
| 19 | chain: child max 20.00, refund 15.00 | Allow (both grants) | same |
| 20 | `customer.export` | Deny (`no-bulk-export`) | Deny (no permit) |

The naive probe asks the application to collect a human approval for a
cross-tenant refund (cases 14–15). The approved retry would still be denied by
the tenant wall, so nothing executes. It does, however, send a cross-tenant
resource to an approval UI. It also reports that resource as approvable rather
than denied, which leaks its existence. Including every `forbid` in the probe
set and skipping the probe when a forbid determined the deny closes both
paths.

### 3.6 Latency

Release build, n = 100 000 (20 000 for the `from_entities` rows), one thread:

| Operation | p50 | p95 | p99 |
|---|---|---|---|
| `Authorizer::is_authorized` (5 policies, allow) | 7.10 µs | 7.26 µs | 10.77 µs |
| `Entities::from_json_value`, 13 entities, with schema | 210 µs | 219 µs | 224 µs |
| `Entities::from_json_value`, 13 entities, no schema | 207 µs | 221 µs | 224 µs |
| `Entities::from_entities`, 13 prebuilt `Entity`, with schema | 46 µs | 48 µs | 54 µs |
| `Entities::from_entities`, 13 prebuilt `Entity`, no schema | 21 µs | 22 µs | 27 µs |
| Full `decide()` (JSON entities + context, 1 grant, allow) | 300 µs | 313 µs | 337 µs |
| Full `decide()` incl. approval probe (case 6) | 647 µs | 658 µs | 728 µs |

Cedar evaluation is within the published range (MILESTONE_18 §9.1, 4–11 µs).
The per-request cost is dominated by **entity-store construction from JSON**,
not by evaluation. Building `Entity` values directly cuts it about 4–10×.

### 3.7 Build cost and dependencies

| Measure | Value |
|---|---|
| Transitive packages, `cedar-policy` + `serde_json` (normal + build edges) | 106 (69 normal-only) |
| Of those, crate **names** not already in the workspace `Cargo.lock` | 48 (incl. `lalrpop`, `lalrpop-util`, `logos`, `petgraph`, `miette`, `serde_with`, `chrono`, `stacker`, `psm`, `sha3`) |
| Cold `cargo build` (dev), `-j4` / `-j96` | 35.0 s / 26.8 s |
| Cold `cargo build --release`, `-j4` / `-j96` | 63.0 s / 33.7 s |
| Baseline (`serde_json` only), dev / release, `-j4` | 2.3 s / 2.4 s |
| **Delta, cold, `-j4`** (≈ 4-vCPU GitHub runner) | **≈ +33 s dev, ≈ +61 s release** |
| `cargo install --locked cedar-policy-cli --version 4.15.0`, cold, `-j4` | 79 s |

MILESTONE_18 §9.1 estimated "about 88 transitive crates". The measured figure
with build dependencies is 106. Incremental workspace builds pay nothing once
the Cedar crates are cached.

### 3.8 `cedar-policy-symcc`

`cedar-policy-symcc` 0.9.0 (MSRV 1.89, Apache-2.0) exists. Its API is async
(`CedarSymCompiler::new(LocalSolver::cvc5()?)`, `check_never_errors`,
`check_always_matches`, `check_never_matches`, `check_unsat`, `check_sat`). It
needs a **cvc5 1.3.1** executable, located via the `CVC5` environment variable.
`cedar-policy-cli` exposes it behind its optional `symcc` feature. cvc5 is not
installed here, so it was not run. A follow-up check is feasible: "no permit is
ever satisfied for a cross-tenant resource" and "no policy in the set can
error" (the latter would mechanize §3.4). It needs a pinned cvc5 binary in CI.
It stays out of M18 scope (issue #346 scope exclusions).

## 4. Decision (proposed)

### 4.1 Engine

**Cedar, `cedar-policy = "=4.15.0"`**, default features. Only `crom-guard`
depends on it (ADR 0002 §2.6). No experimental features (`partial-eval`,
`tpe`, `permissive-validate`, ...) are used in the TCB. The CLI used in CI is
pinned to the same version.

### 4.2 Entity model v1 (frozen)

The committed `support.cedarschema` is v1 for the reference deployment. The
**shape** below is frozen for #347; operator schemas for other deployments
follow it:

| Cedar element | Source | Never from |
|---|---|---|
| `principal`: `Agent` with `tenant`, `delegator` | Grant record (§8.2) | Tool arguments, runtime metadata |
| `delegator` `User` and its attributes | Asserted by the trusted app at grant issuance | Agent |
| `resource` and its attributes (`tenant`, `customer`, amounts) | Trusted `ResourceResolver` (#350) | Argument values other than the identifier |
| `action` | Protected-operation registry, one `ActionId` per MCP tool | Tool name aliases |
| `context.grant` (`GrantCtx` record) | The grant being evaluated | Agent |
| Other context fields (e.g. `amount_cents`) | Schema-validated operation arguments (§10.2 step 2) | Unvalidated input |
| `context.approval` (`ApprovalCtx`, optional) | A stored approval matched by digest (§9.4 step 3) | Tool arguments |

Grant constraints are a **context record**, not an entity. Reasons:

- Evaluation per grant in a chain (§8.3) swaps one record per call. The
  entity store holds only application data, which the resolver owns.
- Latency is equal within noise (§3.2), and the policy text is identical, so a
  later move to an entity is a schema change, not a policy rewrite.
- `Context::from_json_value(_, Some((&schema, &action)))` and
  `Request::new(_, _, _, _, Some(&schema))` type-check the record against the
  action's declared context on every request.

### 4.3 Action-set membership checked in Rust

`action ∈ grant.actions` is checked in Rust **for every grant in the chain,
before resource resolution** (moved from §10.2 step 4 to the end of step 2).
Reasons:

- It is a grant invariant, not operator policy. It must hold whether or not
  the operator's policy set contains the equivalent forbid.
- It is a one-line, reviewable set check.
- It runs before the resolver, so an action the grant cannot use never
  touches application data.

Cedar could also express it (§3.3). It is not duplicated there, so that one
source of truth remains.

#### Amendment (2026-11-19, #353): `guard.delegate` is not evaluated by Cedar

`guard.delegate`, the agent-facing attenuation operation, is a Guard
builtin. It skips the resolver and Cedar. It is gated only by:

- the Rust action check above (`guard.delegate` in every grant of the chain);
- the grant's `max_depth`;
- the narrowing rules of MILESTONE_18 §8.3, checked before the decision
  record and again in the grant store;
- store quotas (children per grant, descendants per root, grants per tenant).

Rationale:

- Delegation only creates a subset of the caller's authority. Every later
  request made with a child handle is evaluated by Cedar for every grant in
  its chain (§4.4), so a child can never be allowed what its parent alone
  would be denied.
- Narrowing is a grant invariant, for the same reasons as the action check
  in this section. It must not depend on operator policy text.
- A delegation has no resolved resource and no action in the operator
  schema to evaluate.

Consequence: operator-wide Cedar policy **cannot forbid** delegation. A
`forbid` on `guard.delegate` would never be consulted. Issuers control it
per grant: a grant can delegate only if the trusted application lists
`guard.delegate` in its `actions` and gives it `max_depth ≥ 1`, and a host
that never calls `OperationRegistry::register_delegate` exposes no such
tool. This deviates from the wording of #353, which describes `guard.delegate`
as a protected operation; protected operations are Cedar-evaluated. See
`docs/guard/grants.md`.

### 4.4 Evaluation and approval derivation

For each request, after authentication, normalization, the Rust action check,
and resolution:

1. **Main evaluation.** For each grant in the chain, evaluate the full policy
   set with that grant's `context.grant`. Include `context.approval` only if a
   stored approval matched (§9.4 step 3).
   - Any error in `diagnostics().errors()` → `Deny(PolicyError)`, whatever the
     decision (§3.4).
   - Every grant `Allow` → `Allow { determining_policies }`.
2. **Deny classification.** On the first grant that denies:
   - If `diagnostics().reason()` is non-empty (a `forbid` determined the deny)
     → `Deny(PolicyDeny)`. No probe.
   - If the request carried an approval → `Deny(PolicyDeny)`. No new
     `ApprovalRequired` for an approved request.
   - If the action's context declares no `approval` attribute → `Deny`.
3. **Approval probe.** For each `@approval("<role>")` value, evaluate the
   **probe set**: every `forbid` in the policy set, plus every permit
   annotated `@approval`. Evaluate it against every grant in the chain, with
   a synthetic `context.approval`:
   - `approver`: a reserved `User` UID, `User::"__guard_approval_placeholder__"`.
     The resolver must reject this ID, and Guard adds it only to the probe's
     entity store. Its attributes: `role = <role>`,
     `tenant = principal.tenant`, numeric limits 0.
   - `amount_cents`: the request's validated amount.

   If every grant allows with no errors →
   `ApprovalRequired { requirement: { role, action_digest } }`. Otherwise
   `Deny(PolicyDeny)`. Any probe error → `Deny(PolicyError)`.
4. **Only step 1 ever yields `Allow`.** The probe result is converted to
   `ApprovalRequired`, which is a deny that names a requirement. It is never
   returned as an authorization. A wrong `@approval` annotation (role mismatch,
   missing annotation) can cause a missing or useless approval prompt. It
   cannot cause execution.

Requirements for policy authors (checked in #349 at load time):

- Every policy has a unique `@id`. Guard re-keys the set by `@id` because
  `PolicySet::from_str` assigns `policy0…`. A missing or duplicate `@id`
  refuses startup.
- An `@approval` value names a role, and the annotated permit tests
  `context has approval` and `context.approval.approver.role == "<role>"`.

### 4.5 Fail-closed rules (in addition to MILESTONE_18 §9.2)

- Strict validation (`ValidationMode::Strict`) with **warnings treated as
  errors** at startup, matching `scripts/validate-guard-policies.sh`.
- The resolver must supply every entity a policy can dereference (principal,
  delegator, resource, the resource's referenced entities, approver). A
  missing entity is an evaluation error and therefore a deny (§3.4). The deny
  is correct, but a resolver gap shows up as `PolicyError`, so #350 tests the
  entity slice per operation.
- Entities are built with `Entity`/`Entities::from_entities(_, Some(&schema))`,
  not from JSON (§3.6), and are validated against the schema.

## 5. Alternatives considered

### 5.1 Engines

| Option | Why not chosen |
|---|---|
| Rego / `regorus` | General-purpose language with harder analysis and no strict schema validation against a typed entity model. Fits only if existing OPA policy must be reused (MILESTONE_18 §9.1) |
| OpenFGA | Go server for long-lived relationship graphs. Per-call argument conditions (amount ≤ limit) are a poor fit, and it adds a service to the TCB |
| Biscuit | Offline-attenuable token format, not a central policy language. M18 grants are server-side (ADR 0002 §2.4). Candidate for M19 capability handles |
| Extend Crom `ToolRule` | Name/argument patterns only, with no entity, attribute, or validator model. Lives in `crom-core`, which Guard may not depend on (ADR 0002 §2.6). It would mean writing a new DSL, which MILESTONE_18 rules out |

### 5.2 Design alternatives within Cedar

| Question | Alternative | Why not chosen |
|---|---|---|
| Grant constraints | `Grant` entity | Equal latency, identical policy text. Mixes grant data into the resolver-owned entity store and complicates per-grant chain evaluation (§4.2) |
| Grant constraints | Rust-only constraint checks | Moves resource-level logic out of the validated, analysable policy language. Only the action set is checked in Rust (§4.3) |
| Action set | `context.grant.actions.contains(action)` forbid in Cedar | Feasible (§3.3), but it makes a grant invariant depend on operator policy text |
| Approval | Separate approval-eligibility policy set | Two texts must agree, and validation cannot check that they do. Drift yields useless prompts (approved retry still denied) or missed prompts. Adds a file for the operator. Still never authorizes, but has no advantage over §4.4 |
| Approval | Partial evaluation with `context.approval` unknown | `is_authorized_partial` is experimental (`partial-eval` feature). Not acceptable in the TCB |
| Approval | Rust-coded rule (e.g. "amount over limit ⇒ approval") | Duplicates policy logic in code and diverges from the policy set |
| Approval probe set | `@approval` permits only (MILESTONE_18 §9.4 as written) | Yields `ApprovalRequired` for cross-tenant resources (§3.5 cases 14–15) |

## 6. Consequences

- Grant constraint fields are part of the operator schema (`GrantCtx`). The
  grant record's `constraints` (§8.2) must serialize to exactly that type.
  A mismatch fails `Context` construction, which is a deny.
- Each approval-eligible denial costs a second evaluation per grant in the
  chain (+0.35 ms in the spike, dominated by entity construction).
- `ApprovalRequired` responses reveal that a resource is approvable for this
  grant. This is no more than the agent can already learn from a deny on the
  same, already-delegated resource. Cross-tenant and forbidden resources
  return a plain `Deny`.
- CI pays ≈ 33 s (dev) per cold build of Guard crates, plus a cached
  `cedar-policy-cli` install (79 s cold) in `security / Guard Cedar policies`.
- 48 new crate names enter `Cargo.lock` when #349 adds the dependency. They go
  through `cargo-deny` like every other dependency.

## 7. Open questions

1. **Approval consumption on a denied retry.** If an approval matches the
   digest but the retry is denied (e.g. the invoice balance changed), is the
   approval consumed? Proposed: consumed. A single-use approval that is
   observed by an evaluation is spent, and the app must re-approve (#354).
2. **Multiple `@approval` roles.** The probe tries each role and reports the
   first that would allow. Should the requirement list every satisfying role?
   Proposed: first, in policy-set order; revisit if a deployment needs more.
3. **Placeholder tenant.** `tenant = principal.tenant` matches the v1 policy.
   Should Guard instead let the annotation name the approver's attributes
   (e.g. `@approval("supervisor")` plus `@approval_tenant("principal")`)?
   Proposed: no annotation DSL in M18.
4. **Entity store reuse.** Should Guard cache per-tenant entity slices across
   requests to remove the 21–46 µs build? Proposed: no. Fresh slices avoid
   TOCTOU on cached attributes (#350); revisit only if measured end-to-end
   latency requires it.
5. **symcc in CI** (§3.8): owner to decide whether a pinned cvc5 in a non-gating
   lane is acceptable for M19.

## 8. Requirements handed on

| To | Requirement |
|---|---|
| #347 | `AuthorizationContext` carries a `GrantCtx`-typed record per grant, plus optional `ApprovalCtx`. `Decision::ApprovalRequired` is not constructible from an `Allow` path |
| #349 | Implement §4.4–§4.5. Unit tests for every row of §3.5, the erroring-forbid case, the missing-entity case, and missing or duplicate `@id`. Pin `cedar-policy = "=4.15.0"` |
| #350 | Resolver returns the full entity slice per operation; rejects the reserved placeholder UID |
| #351 | Rust action-set check for every grant in the chain before resolution (§4.3) |
| #353 | Child `GrantCtx` is evaluated independently (§3.5 cases 18–19 are the reference tests) |
| #354 | Approvals inject `ApprovalCtx` only after digest, grant, expiry, and unused checks. Settle open question 1 |
| #362 | Changes to `examples/guard-support/policies/` keep `scripts/validate-guard-policies.sh` green |

## 9. Changes to MILESTONE_18

- §9.1: dependency count corrected to 106 transitive packages (48 new names),
  with measured build delta and latency, citing this ADR.
- §9.3: schema and policies replaced by a pointer to the committed v1, with
  the three changes from §3.1. The action-set question is recorded as decided
  (Rust).
- §9.4 step 1: probe set is all `forbid` policies plus `@approval` permits,
  with the placeholder rules and preconditions from §4.4.
- §10.2: action-set check moved from step 4 to step 2.

## Appendix A: spike source

Not merged; reproduced here so the numbers can be re-run. Build outside the
repository with `rust-toolchain` = `1.89.0`, then run
`cargo run --release -- <path to examples/guard-support/policies> [iterations]`.

`Cargo.toml`:

```toml
[package]
name = "cedar_spike"
version = "0.0.0"
edition = "2021"
rust-version = "1.89"
publish = false

[dependencies]
cedar-policy = "=4.15.0"
serde_json = "1"

[workspace]
```

`src/main.rs`:

```rust
use cedar_policy::{
    Authorizer, Context, Decision, Effect, Entities, EntityUid, PolicyId, PolicySet, Request,
    Schema, ValidationMode, Validator,
};
use serde_json::{json, Value};
use std::str::FromStr;
use std::time::Instant;

const PLACEHOLDER: &str = "__guard_approval_placeholder__";

fn load(schema_src: &str, policy_src: &str) -> (Schema, PolicySet) {
    let (schema, warnings) = Schema::from_cedarschema_str(schema_src).expect("schema parse");
    for w in warnings {
        println!("schema warning: {w}");
    }
    let parsed = PolicySet::from_str(policy_src).expect("policy parse");
    let mut pset = PolicySet::new();
    for p in parsed.policies() {
        let id = p.annotation("id").expect("@id required").to_string();
        pset.add(p.new_id(PolicyId::new(id))).expect("add");
    }
    let v = Validator::new(schema.clone()).validate(&pset, ValidationMode::Strict);
    for e in v.validation_errors() {
        println!("VALIDATION ERROR: {e}");
    }
    for w in v.validation_warnings() {
        println!("validation warning: {w}");
    }
    assert!(v.validation_passed(), "strict validation failed");
    (schema, pset)
}

fn uid(t: &str, id: &str) -> Value {
    json!({"type": format!("Support::{t}"), "id": id})
}

fn ent(t: &str, id: &str, attrs: Value, parents: Vec<Value>) -> Value {
    json!({"uid": uid(t, id), "attrs": attrs, "parents": parents})
}

fn base_entities(include_delegator: bool) -> Vec<Value> {
    let t1 = uid("Tenant", "t1");
    let t2 = uid("Tenant", "t2");
    let mut v = vec![
        ent("Tenant", "t1", json!({}), vec![]),
        ent("Tenant", "t2", json!({}), vec![]),
        ent("User", "sup", json!({"tenant": {"__entity": t1}, "role": "supervisor", "refund_limit_cents": 100000}), vec![t1.clone()]),
        ent("User", "bob", json!({"tenant": {"__entity": t1}, "role": "agent", "refund_limit_cents": 5000}), vec![t1.clone()]),
        ent("User", "sup2", json!({"tenant": {"__entity": t2}, "role": "supervisor", "refund_limit_cents": 100000}), vec![t2.clone()]),
        ent("Agent", "a1", json!({"tenant": {"__entity": t1}, "delegator": {"__entity": uid("User", "alice")}}), vec![]),
        ent("Customer", "c1", json!({"tenant": {"__entity": t1}}), vec![t1.clone()]),
        ent("Customer", "c2", json!({"tenant": {"__entity": t1}}), vec![t1.clone()]),
        ent("Customer", "c9", json!({"tenant": {"__entity": t2}}), vec![t2.clone()]),
        ent("Invoice", "i1", json!({"tenant": {"__entity": t1}, "customer": {"__entity": uid("Customer","c1")}, "amount_cents": 20000, "refunded_cents": 0}), vec![uid("Customer","c1")]),
        ent("Invoice", "i2", json!({"tenant": {"__entity": t1}, "customer": {"__entity": uid("Customer","c2")}, "amount_cents": 20000, "refunded_cents": 0}), vec![uid("Customer","c2")]),
        ent("Invoice", "i3", json!({"tenant": {"__entity": t1}, "customer": {"__entity": uid("Customer","c1")}, "amount_cents": 10000, "refunded_cents": 9000}), vec![uid("Customer","c1")]),
        // Cross-tenant invoice that (wrongly) references a t1 customer: only the tenant wall stops it.
        ent("Invoice", "i9", json!({"tenant": {"__entity": t2}, "customer": {"__entity": uid("Customer","c1")}, "amount_cents": 20000, "refunded_cents": 0}), vec![uid("Customer","c9")]),
    ];
    if include_delegator {
        v.push(ent("User", "alice", json!({"tenant": {"__entity": t1}, "role": "agent", "refund_limit_cents": 5000}), vec![t1]));
    }
    v
}

#[derive(Clone)]
struct Grant {
    actions: Vec<&'static str>,
    customers: Vec<&'static str>,
    max_refund_cents: i64,
}

impl Grant {
    fn ctx(&self) -> Value {
        json!({
            "customers": self.customers.iter().map(|c| json!({"__entity": uid("Customer", c)})).collect::<Vec<_>>(),
            "max_refund_cents": self.max_refund_cents,
        })
    }
}

#[derive(Clone)]
struct Req {
    action: &'static str,
    resource: (&'static str, &'static str),
    amount: Option<i64>,
    approval: Option<(&'static str, i64)>,
    chain: Vec<Grant>,
}

#[derive(Debug, PartialEq)]
enum Outcome {
    Allow(Vec<String>),
    Deny(String),
    ApprovalRequired(String),
}

struct Engine {
    schema: Schema,
    pset: PolicySet,
    probe_set: PolicySet,
    naive_probe_set: PolicySet,
    approval_roles: Vec<String>,
}

impl Engine {
    fn new(schema: Schema, pset: PolicySet) -> Self {
        let mut probe_set = PolicySet::new();
        let mut naive_probe_set = PolicySet::new();
        let mut approval_roles = vec![];
        for p in pset.policies() {
            let is_approval = p.annotation("approval").is_some();
            if p.effect() == Effect::Forbid || is_approval {
                probe_set.add(p.clone()).unwrap();
            }
            if is_approval {
                naive_probe_set.add(p.clone()).unwrap();
                approval_roles.push(p.annotation("approval").unwrap().to_string());
            }
        }
        Engine { schema, pset, probe_set, naive_probe_set, approval_roles }
    }

    fn eval(&self, set: &PolicySet, r: &Req, g: &Grant, approval: Option<Value>, extra: Vec<Value>, include_delegator: bool)
        -> Result<(Decision, Vec<String>), String> {
        let action = EntityUid::from_json(json!({"type": "Support::Action", "id": r.action})).unwrap();
        let mut ctx = json!({"grant": g.ctx()});
        if let Some(a) = r.amount {
            ctx["amount_cents"] = json!(a);
        }
        if let Some(a) = approval {
            ctx["approval"] = a;
        }
        let context = Context::from_json_value(ctx, Some((&self.schema, &action))).map_err(|e| format!("context: {e}"))?;
        let principal = EntityUid::from_json(uid("Agent", "a1")).unwrap();
        let resource = EntityUid::from_json(uid(r.resource.0, r.resource.1)).unwrap();
        let request = Request::new(principal, action, resource, context, Some(&self.schema)).map_err(|e| format!("request: {e}"))?;
        let mut ents = base_entities(include_delegator);
        ents.extend(extra);
        let entities = Entities::from_json_value(Value::Array(ents), Some(&self.schema)).map_err(|e| format!("entities: {e}"))?;
        let resp = Authorizer::new().is_authorized(&request, set, &entities);
        let errs: Vec<String> = resp.diagnostics().errors().map(|e| e.to_string()).collect();
        if !errs.is_empty() {
            return Err(format!("PolicyError({})", errs.join("; ")));
        }
        Ok((resp.decision(), resp.diagnostics().reason().map(|p| p.to_string()).collect()))
    }

    fn approval_value(approver: &str, amount: i64) -> Value {
        json!({"approver": {"__entity": uid("User", approver)}, "amount_cents": amount})
    }

    /// Full Guard decision: Rust action check, per-grant Cedar, fail closed on
    /// any error, then the approval probe.
    fn decide(&self, r: &Req, naive_probe: bool, include_delegator: bool) -> Outcome {
        for (i, g) in r.chain.iter().enumerate() {
            if !g.actions.contains(&r.action) {
                return Outcome::Deny(format!("NotInGrant(grant#{i})"));
            }
        }
        let mut reasons = vec![];
        let mut denied_by: Option<(usize, Vec<String>)> = None;
        for (i, g) in r.chain.iter().enumerate() {
            let appr = r.approval.map(|(who, amt)| Self::approval_value(who, amt));
            match self.eval(&self.pset, r, g, appr, vec![], include_delegator) {
                Err(e) => return Outcome::Deny(e),
                Ok((Decision::Allow, rs)) => reasons.extend(rs),
                Ok((Decision::Deny, rs)) => {
                    denied_by = Some((i, rs));
                    break;
                }
            }
        }
        let Some((i, rs)) = denied_by else { return Outcome::Allow(reasons) };
        if !rs.is_empty() && !naive_probe {
            return Outcome::Deny(format!("PolicyDeny(grant#{i}, {})", rs.join(",")));
        }
        if r.approval.is_some() || r.amount.is_none() {
            return Outcome::Deny(format!("PolicyDeny(grant#{i}, no permit)"));
        }
        // Probe: every grant must allow with a placeholder approval.
        let set = if naive_probe { &self.naive_probe_set } else { &self.probe_set };
        for role in &self.approval_roles {
            let placeholder = ent("User", PLACEHOLDER, json!({"tenant": {"__entity": uid("Tenant", "t1")}, "role": role, "refund_limit_cents": 0}), vec![]);
            let all = r.chain.iter().all(|g| {
                matches!(
                    self.eval(set, r, g, Some(Self::approval_value(PLACEHOLDER, r.amount.unwrap())), vec![placeholder.clone()], include_delegator),
                    Ok((Decision::Allow, _))
                )
            });
            if all {
                return Outcome::ApprovalRequired(role.clone());
            }
        }
        Outcome::Deny(format!("PolicyDeny(grant#{i}, no permit)"))
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = &args[1];
    let schema_src = std::fs::read_to_string(format!("{dir}/support.cedarschema")).unwrap();
    let policy_src = std::fs::read_to_string(format!("{dir}/support.cedar")).unwrap();
    let (schema, pset) = load(&schema_src, &policy_src);
    println!("v1 schema + {} policies: strict validation passed", pset.policies().count());
    let engine = Engine::new(schema.clone(), pset.clone());

    let g = Grant { actions: vec!["customer.read", "invoice.list", "refund.issue", "customer.export"], customers: vec!["c1"], max_refund_cents: 10000 };
    let narrow = Grant { actions: vec!["customer.read", "refund.issue"], customers: vec!["c1"], max_refund_cents: 2000 };
    let read_only = Grant { actions: vec!["customer.read"], customers: vec!["c1"], max_refund_cents: 0 };
    let refund = |res: &'static str, amt: i64, appr: Option<(&'static str, i64)>, chain: Vec<Grant>| Req {
        action: "refund.issue", resource: ("Invoice", res), amount: Some(amt), approval: appr, chain,
    };
    let cases: Vec<(&str, Req)> = vec![
        ("read delegated customer", Req { action: "customer.read", resource: ("Customer", "c1"), amount: None, approval: None, chain: vec![g.clone()] }),
        ("read undelegated same-tenant customer", Req { action: "customer.read", resource: ("Customer", "c2"), amount: None, approval: None, chain: vec![g.clone()] }),
        ("read other-tenant customer", Req { action: "customer.read", resource: ("Customer", "c9"), amount: None, approval: None, chain: vec![g.clone()] }),
        ("action not in grant (refund on read-only grant)", refund("i1", 1000, None, vec![read_only.clone()])),
        ("refund 30.00 within all limits", refund("i1", 3000, None, vec![g.clone()])),
        ("refund 80.00 > delegator limit 50.00, no approval", refund("i1", 8000, None, vec![g.clone()])),
        ("refund 80.00 with supervisor approval for 80.00", refund("i1", 8000, Some(("sup", 8000)), vec![g.clone()])),
        ("refund 80.00 with approval for 70.00 (amount mismatch)", refund("i1", 8000, Some(("sup", 7000)), vec![g.clone()])),
        ("refund 80.00 approved by non-supervisor", refund("i1", 8000, Some(("bob", 8000)), vec![g.clone()])),
        ("refund 80.00 approved by other-tenant supervisor", refund("i1", 8000, Some(("sup2", 8000)), vec![g.clone()])),
        ("refund 20.00 exceeding remaining invoice balance 10.00", refund("i3", 2000, None, vec![g.clone()])),
        ("refund 20.00 over balance, approval present", refund("i3", 2000, Some(("sup", 2000)), vec![g.clone()])),
        ("refund on undelegated customer's invoice", refund("i2", 1000, None, vec![g.clone()])),
        ("refund on other-tenant invoice", refund("i9", 1000, None, vec![g.clone()])),
        ("refund on other-tenant invoice, over limit", refund("i9", 8000, None, vec![g.clone()])),
        ("negative refund", refund("i1", -500, None, vec![g.clone()])),
        ("negative refund with matching approval", refund("i1", -500, Some(("sup", -500)), vec![g.clone()])),
        ("chain: child max 20.00, refund 30.00", refund("i1", 3000, None, vec![g.clone(), narrow.clone()])),
        ("chain: child max 20.00, refund 15.00", refund("i1", 1500, None, vec![g.clone(), narrow.clone()])),
        ("customer.export (forbid)", Req { action: "customer.export", resource: ("Customer", "c1"), amount: None, approval: None, chain: vec![g.clone()] }),
    ];
    println!("\n| # | Case | Guard decision | Naive probe (approval permits only) |");
    println!("|---|---|---|---|");
    for (n, (name, r)) in cases.iter().enumerate() {
        let d = engine.decide(r, false, true);
        let naive = engine.decide(r, true, true);
        println!("| {} | {name} | {d:?} | {} |", n + 1, if naive == d { "same".to_string() } else { format!("{naive:?}") });
    }

    // Missing entity in the slice -> evaluation error -> deny.
    println!("\nmissing delegator entity: {:?}", engine.decide(&cases[4].1, false, false));

    // Erroring forbid is skipped by Cedar.
    let overflow_src = format!("{policy_src}\n@id(\"overflow-forbid\")\nforbid (principal, action == Support::Action::\"refund.issue\", resource) when {{ context.amount_cents * 4611686018427387904 > 0 }};\n");
    let (_, pset2) = load(&schema_src, &overflow_src);
    let e2 = Engine::new(schema.clone(), pset2.clone());
    let r = &cases[4].1;
    {
        let action = EntityUid::from_json(json!({"type":"Support::Action","id":"refund.issue"})).unwrap();
        let ctx = Context::from_json_value(json!({"grant": g.ctx(), "amount_cents": 3000}), Some((&schema, &action))).unwrap();
        let req = Request::new(EntityUid::from_json(uid("Agent","a1")).unwrap(), action, EntityUid::from_json(uid("Invoice","i1")).unwrap(), ctx, Some(&schema)).unwrap();
        let ents = Entities::from_json_value(Value::Array(base_entities(true)), Some(&schema)).unwrap();
        let resp = Authorizer::new().is_authorized(&req, &pset2, &ents);
        println!("erroring forbid, raw Cedar: decision={:?} reason={:?} errors={}", resp.decision(),
            resp.diagnostics().reason().map(|p| p.to_string()).collect::<Vec<_>>(), resp.diagnostics().errors().count());
    }
    println!("erroring forbid, Guard: {:?}", e2.decide(r, false, true));

    // Variant: grant as entity. Same policy text.
    let schema_b_src = schema_src
        .replace("type GrantCtx = {\n    customers: Set<Customer>,\n    max_refund_cents: Long,\n  };", "entity GrantCtx {\n    customers: Set<Customer>,\n    max_refund_cents: Long,\n  };");
    assert_ne!(schema_b_src, schema_src);
    let (schema_b, pset_b) = load(&schema_b_src, &policy_src);
    println!("grant-as-entity variant: strict validation passed with identical policy text");

    // Variant: action set in Cedar.
    let schema_c_src = schema_src.replace("max_refund_cents: Long,\n  };", "max_refund_cents: Long,\n    actions: Set<Action>,\n  };");
    match Schema::from_cedarschema_str(&schema_c_src) {
        Ok(_) => println!("Set<Action> in a record: schema accepted"),
        Err(e) => println!("Set<Action> in a record: schema rejected: {e}"),
    }
    if let Ok((schema_c, _)) = Schema::from_cedarschema_str(&schema_c_src) {
        let src = format!("{policy_src}\n@id(\"action-in-grant\")\nforbid (principal, action, resource) unless {{ context.grant.actions.contains(action) }};\n");
        let ps = PolicySet::from_str(&src).unwrap();
        let v = Validator::new(schema_c).validate(&ps, ValidationMode::Strict);
        println!("action-in-grant forbid under Set<Action>: strict validation passed={}", v.validation_passed());
        for e in v.validation_errors() { println!("  {e}"); }
    }

    // Bench.
    let n: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(50_000);
    let action = EntityUid::from_json(json!({"type":"Support::Action","id":"refund.issue"})).unwrap();
    let ents = Entities::from_json_value(Value::Array(base_entities(true)), Some(&schema)).unwrap();
    let mk_req = |s: &Schema, gctx: Value| {
        let ctx = Context::from_json_value(json!({"grant": gctx, "amount_cents": 3000}), Some((s, &action))).unwrap();
        Request::new(EntityUid::from_json(uid("Agent","a1")).unwrap(), action.clone(), EntityUid::from_json(uid("Invoice","i1")).unwrap(), ctx, Some(s)).unwrap()
    };
    let req_a = mk_req(&schema, g.ctx());
    let mut ents_b_json = base_entities(true);
    ents_b_json.push(json!({"uid": uid("GrantCtx","g1"), "attrs": g.ctx(), "parents": []}));
    let ents_b = Entities::from_json_value(Value::Array(ents_b_json.clone()), Some(&schema_b)).unwrap();
    let req_b = mk_req(&schema_b, json!({"__entity": uid("GrantCtx","g1")}));
    let auth = Authorizer::new();
    assert_eq!(auth.is_authorized(&req_a, &pset, &ents).decision(), Decision::Allow);
    assert_eq!(auth.is_authorized(&req_b, &pset_b, &ents_b).decision(), Decision::Allow);
    let bench = |name: &str, f: &dyn Fn()| {
        for _ in 0..n / 10 { f(); }
        let mut t: Vec<u128> = (0..n).map(|_| { let s = Instant::now(); f(); s.elapsed().as_nanos() }).collect();
        t.sort_unstable();
        println!("{name}: p50={:.2}us p95={:.2}us p99={:.2}us (n={n})", t[n/2] as f64/1e3, t[n*95/100] as f64/1e3, t[n*99/100] as f64/1e3);
    };
    bench("is_authorized, grant in context (allow)", &|| { std::hint::black_box(auth.is_authorized(&req_a, &pset, &ents)); });
    bench("is_authorized, grant as entity (allow)", &|| { std::hint::black_box(auth.is_authorized(&req_b, &pset_b, &ents_b)); });
    let ents_json = Value::Array(base_entities(true));
    bench("Entities::from_json_value (13 entities, schema)", &|| { std::hint::black_box(Entities::from_json_value(ents_json.clone(), Some(&schema)).unwrap()); });
    bench("Entities::from_json_value + 1 grant entity", &|| { std::hint::black_box(Entities::from_json_value(Value::Array(ents_b_json.clone()), Some(&schema_b)).unwrap()); });
    bench("Entities::from_json_value (13 entities, no schema)", &|| { std::hint::black_box(Entities::from_json_value(ents_json.clone(), None).unwrap()); });
    let prebuilt: Vec<cedar_policy::Entity> = base_entities(true).into_iter().map(|v| cedar_policy::Entity::from_json_value(v, Some(&schema)).unwrap()).collect();
    bench("Entities::from_entities (13 prebuilt Entity, schema)", &|| { std::hint::black_box(Entities::from_entities(prebuilt.clone(), Some(&schema)).unwrap()); });
    bench("Entities::from_entities (13 prebuilt Entity, no schema)", &|| { std::hint::black_box(Entities::from_entities(prebuilt.clone(), None).unwrap()); });
    let over = refund("i1", 8000, None, vec![g.clone()]);
    bench("full decide() incl. entity build, allow", &|| { std::hint::black_box(engine.decide(&cases[4].1, false, true)); });
    bench("full decide() incl. approval probe", &|| { std::hint::black_box(engine.decide(&over, false, true)); });
    let t = Instant::now();
    for _ in 0..100 { std::hint::black_box(load_quiet(&schema_src, &policy_src)); }
    println!("schema parse + policy parse + strict validate: {:.2}ms avg", t.elapsed().as_secs_f64() * 10.0);
}

fn load_quiet(schema_src: &str, policy_src: &str) -> bool {
    let (schema, _) = Schema::from_cedarschema_str(schema_src).unwrap();
    let pset = PolicySet::from_str(policy_src).unwrap();
    Validator::new(schema).validate(&pset, ValidationMode::Strict).validation_passed()
}
```
