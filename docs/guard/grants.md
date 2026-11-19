# Guard grants and session handles

`MemoryGrantStore` (`crates/crom-guard/src/store.rs`, #348) implements the sealed `GrantStore` trait. Record format: MILESTONE_18 §8.2. Types: [README.md](README.md).

## Lifecycle

| Step | Call | Checks |
|---|---|---|
| Issue | `issue(issuer, GrantSpec, now)` | See "Issue" below. |
| Authenticate | `authenticate(handle, now)` | See "Authenticate" below. Returns the chain as an `ExecutionContext`. |
| Re-check | `recheck(ctx, now)` | Reloads the chain from the store and checks it is still live. The pipeline calls this just before executing (§10.2 step 6). |
| Revoke | `revoke(issuer, grant_id, now)` | Sets `revoked_at` on the first call; later calls change nothing. Returns `UnknownGrant` unless `issuer` issued the grant, so one application can't revoke another's grants. Descendants fail on their next request, because the whole chain is checked every time. |
| Delegate | `delegate(parent_ctx, DelegationSpec, now)` | See "Delegate" below. Agents reach it only through `guard.delegate` (see "Agent-facing delegation"). |

**Issue.** These are checked:
- `actions` is not empty, and every action is registered (`GrantStoreConfig::registered_actions`).
- `ttl_seconds` is at least 1 and at most `max_ttl_seconds`, which defaults to 3600. A longer request is rejected, not shortened.

Guard then generates the grant id (`g_…`), the agent id (`a_…`), and a 256-bit session handle (`gsh_…`) from the OS RNG.

**Authenticate.** Fails with:
- `NoGrant` for an unknown handle;
- `GrantNotYetValid`, `GrantExpired` or `GrantRevoked` when any grant in the chain is outside its window or revoked;
- `EngineUnavailable` when the chain is broken.

**Delegate.**
- The parent chain is re-read from the store and must be live.
- The child's actions must be a subset of the parent's, and its constraints must narrow the parent's.
- The parent's `max_depth` must be at least 1, and the child's must be lower.
- The child must not outlive its parent: a `now + ttl` later than the parent's `expires_at` is rejected, not shortened.
- A grant may create at most `max_children_per_grant` children (default 16) over its lifetime.
- Tenant, delegator, issuer and run are copied from the parent.
- Re-reading the parent, the checks and the insert all run under one write lock, so no delegation succeeds after the parent's `revoke` has returned.

MILESTONE_18 §8.3 sets the child's `max_depth` to the parent's minus one. The store accepts any lower value, which is never wider; `guard.delegate` defaults it to 0.

The narrowing rules (actions, constraints, depth, expiry) are one function, `store::check_narrowing`, which `delegate` and the `guard.delegate` pre-decision check both call.

A grant is live when `not_before ≤ now < expires_at` and `revoked_at` is unset. Any recorded revocation denies, whatever its timestamp.

## Agent-facing delegation: `guard.delegate`

[#353](https://github.com/mfaulk/crom-rs/issues/353), MILESTONE_18 §8.3. Code: `crates/crom-guard/src/delegate.rs`.

The host opts in with `OperationRegistry::register_delegate()`, before it builds `GrantStoreConfig` from `registry.action_ids()`. The tool is then listed like any other, under the reserved name `guard.delegate` (`DELEGATE_ACTION`). A grant can call it only if `guard.delegate` is in its `actions`, and in the `actions` of every ancestor (the pipeline's action check, step 2). Holding the action is not enough: the grant also needs `max_depth ≥ 1`.

Arguments (validated against a closed schema that passes the registry allowlist; anything else is `invalid_arguments`):

| Field | Type | Meaning |
|---|---|---|
| `actions` | array of action ids, 1–64, unique | The child's actions. Required |
| `constraints` | array of `{name, set}` or `{name, at_most}`, at most 32 | Each entry replaces the parent's constraint of that name or adds a new one. Parent constraints not listed are inherited unchanged. A `set` holds `{"type", "id"}` entity uids, at most 1024. A repeated name, or an entry with both or neither of `set` and `at_most`, is `invalid_arguments` |
| `ttl_seconds` | integer ≥ 1 | The child expires at `now + ttl_seconds`. Required |
| `max_depth` | integer 0–255 | Default 0: the child cannot delegate |

There is no field for tenant, delegator, run, issuer or agent. They are copied from the parent or generated, and an extra field is `invalid_arguments`.

Order of checks for one call:

1. Pipeline step 1–2: authenticate the parent handle, validate the schema, and require `guard.delegate` in every grant of the chain (`action_not_granted`).
2. **Pre-decision check** (`BuiltinOperation::check`): parse the arguments (`invalid_arguments`), then apply `check_narrowing` against the caller's leaf grant at the call's clock reading. Any widening (an action the parent lacks, a wider or re-kinded constraint, `max_depth` not below the parent's, `max_depth` 0 on the parent, or an expiry after the parent's) is **`policy_deny`**. The decision record says `deny` with reason `policy_deny`, and nothing is written to the store.
3. Decision record `allow` (enforcement `guard_validation`), durable before anything else.
4. Recheck the parent chain on a fresh clock reading.
5. `GrantStore::delegate`, which re-reads the parent under its write lock and applies the same rules again, plus the per-grant child limit and store capacity. A failure here (for example a revoke that raced the call, or the child limit) is outcome `failed` with `execution_failed`; the store's message is never shown to the agent.
6. Outcome record `executed`, with `downstream_ref` set to the child grant id.

Result content:

```json
{"handle": "gsh_…", "grant_id": "g_…", "actions": ["customer.read"],
 "constraints": {"customers": {"set": [ … ]}, "max_refund_cents": {"at_most": 2000}},
 "expires_at": 1700000060000, "max_depth": 0}
```

The child handle is a secret. It appears only in this agent-visible result. The audit log records the child grant id (`downstream_ref`) and the logged arguments (`actions`, `constraints`, `ttl_seconds`, `max_depth`), never the handle. No error and no `tracing` event carries it. Hosts must treat the `executed` content of `guard.delegate` like the parent handle itself (`crom-guard-server`, #357, must not log tool results).

Constraints the operator's schema does not declare (for example a new name in `GrantCtx`) are accepted by narrowing, but every request with such a child then fails schema validation in Cedar (`policy_error`). That fails closed.

### Why `guard.delegate` is not Cedar-authorized

`guard.delegate` skips the resolver and Cedar, and is authorized by the Rust narrowing rules. Reasons:

- **It creates no authority, only a subset of the caller's.** ADR 0002 §2.3 allows it as the single agent-reachable write because it can only narrow. Whatever a child later does goes through Cedar on every call, with every grant in its chain evaluated on its own (MILESTONE_18 §8.3, ADR 0003 §4.4). So the child can never be allowed something its parent alone would be denied. A Cedar check on the delegation itself would add no protection.
- **Narrowing is a grant invariant, not operator policy.** ADR 0003 §4.3 moved the equivalent action-set check out of Cedar for the same reason: it must hold whatever the operator's policy text says. Putting delegation in Cedar would make a missing or permissive policy a way to widen authority.
- **No resource.** Cedar needs a resolved resource and a declared action in the operator's schema. Delegation has neither, so every deployment would have to add a synthetic entity and action to its schema just to permit it.

What does gate delegation: `guard.delegate` must be in every grant of the chain (an application that never lists it disables delegation for that grant), `max_depth ≥ 1`, and the narrowing check before the decision record and again in the store. If an operator wants a policy-level limit on delegation later, the `check` hook is the place to add it.

### Attenuation properties (tested)

- For any request, if the child chain is allowed then the parent alone is allowed. Tested twice: end to end through `Guard` with 10,000 random parent grants, delegations and requests (`tests/attenuation.rs`, `child_authority_implies_parent_authority`), and at the PDP with 10,000 random *unrelated* child grants and no narrowing check at all, so the property holds from chain evaluation alone (`pdp::tests::attenuation`).
- A delegation succeeds exactly when it narrows, per an independent oracle in the same test.
- Depth strictly decreases, and tenant, delegator, issuer and run never change across a chain (`ExecutionContext::from_chain` rejects any other chain).
- Revoking or expiring a parent disables every descendant, and their ability to delegate.

## Handles

- **Only the hash is stored.** The store keeps `SHA-256(handle)` and never the plaintext. The plaintext is returned once, in `IssuedGrant::handle`, for the trusted application to pass to the agent. The store's `Debug` output omits its contents.
- **Lookup uses selector and verifier.** The first 8 bytes of the hash select a small set of candidates. The full 32-byte hash is then compared in constant time with `subtle`.
- **Handles stay off the admin API.** A handle authenticates only to its own grant. The admin API is a separate listener with its own credential (ADR 0002 §2.3, #357).

## Capacity

The store holds at most `max_grants` grants (default 100,000). When it is full, `issue` and `delegate` first remove every expired grant. If it is still full, they fail with `Unavailable`. Agents can drive `delegate`, so memory has to be bounded.

Removing expired grants is safe because a child never expires after its parent: an expired subtree goes all at once, and no live grant loses an ancestor. After removal, a swept handle fails with `NoGrant` instead of `GrantExpired`.

## Concurrency

One `RwLock` orders `revoke` against `authenticate` and `recheck`. Once `revoke` returns, no call that starts afterwards sees the grant or any of its descendants as live. Tests `revoke_racing_authenticate` and `parent_revoke_racing_child_authenticate` check this. Each asserts that some calls really started after the revoke, and each is time-bounded, so a broken store makes it fail instead of hang. An executor that is already running is not interrupted (§8.2).

## Restart semantics (decided)

M18 provides only the in-memory store. Restarting Guard invalidates every grant and handle, which fails closed (MILESTONE_18 §19 risk 10). The trusted application must issue new grants after a restart. A persistent store (JSON with atomic rename, or SQLite) is a follow-up. If one is added, its `Grant` records must pass `ExecutionContext::from_chain`, which rejects any `schema_version` other than 1.

## Requirements on callers

The store doesn't enforce these, so the issues that use it must:

- **#353 (done):** only a grant whose `actions` include the delegate operation may call `delegate`. The store checks only `max_depth`; the pipeline's action check requires `guard.delegate` in every grant of the chain (see "Agent-facing delegation").
- **#351 (done):** `Guard` takes `now` from its injected `Clock`, never from the request ([pipeline.md](pipeline.md)). If the wall clock is set back, expired grants come back to life (revoked ones stay revoked).
- **#349:** evaluate every grant in the chain on its own. A child may add constraints its parent lacks, which narrows authority only if no policy treats a missing constraint as permissive.

Handle secrets are not zeroized in memory; that is a follow-up.

## Tests

`cargo test -p crom-guard store::` covers:
- unknown, expired, not-yet-valid and revoked handles, and a revoked parent;
- issuer-scoped revocation;
- the max-TTL limit and unknown actions at issuance;
- narrowing and widening delegations, including several levels;
- the per-grant child limit and the store capacity, with expired grants removed;
- that the plaintext handle is never stored;
- revoke racing authenticate, for the grant itself and for a revoked parent;
- a proptest: every arbitrary `GrantSpec` is either rejected exactly when it is invalid, or authenticates with the same authority.

Attenuation (#353):

```sh
cargo test -p crom-guard --test attenuation        # widening attempts, multi-hop, revoke, handle secrecy, 10k-case proptest
cargo test -p crom-guard --lib pdp::tests::attenuation   # 10k-case chain-evaluation proptest, no narrowing check
cargo test -p crom-guard-testkit --test scenario_g10 --test canary
```

`crates/crom-guard-testkit/fixtures/scenarios/g10-subagent-escalation.json` is the scenario G10 fixture for #361: the parent grant, the legitimate subtask (delegate, then read and refund within the child's limits), each escalation attempt with its expected code, and the expected ledger effects. `tests/scenario_g10.rs` replays it against the business API with the ledger as the oracle.
