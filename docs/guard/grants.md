# Guard grants and session handles

`MemoryGrantStore` (`crates/crom-guard/src/store.rs`, #348) implements the sealed `GrantStore` trait. Record format: MILESTONE_18 §8.2. Types: [README.md](README.md).

## Lifecycle

| Step | Call | Checks |
|---|---|---|
| Issue | `issue(issuer, GrantSpec, now)` | See "Issue" below. |
| Authenticate | `authenticate(handle, now)` | See "Authenticate" below. Returns the chain as an `ExecutionContext`. |
| Re-check | `recheck(ctx, now)` | Reloads the chain from the store and checks it is still live. The pipeline calls this just before executing (§10.2 step 6). |
| Revoke | `revoke(issuer, grant_id, now)` | Sets `revoked_at` on the first call; later calls change nothing. Returns `UnknownGrant` unless `issuer` issued the grant, so one application can't revoke another's grants. Descendants fail on their next request, because the whole chain is checked every time. |
| Delegate | `delegate(parent_ctx, DelegationSpec, now)` | See "Delegate" below. #353 adds the agent-facing `delegate` operation. |

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

A grant is live when `not_before ≤ now < expires_at` and `revoked_at` is unset. Any recorded revocation denies, whatever its timestamp.

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

- **#353:** only a grant whose `actions` include the delegate operation may call `delegate`. The store checks only `max_depth`.
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
