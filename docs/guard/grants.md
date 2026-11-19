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
- The child's `expires_at` is the earlier of `now + ttl` and the parent's expiry.
- Tenant, delegator, issuer and run are copied from the parent.

A grant is live when `not_before ≤ now < expires_at` and `revoked_at` is unset. Any recorded revocation denies, whatever its timestamp.

## Handles

- **Only the hash is stored.** The store keeps `SHA-256(handle)` and never the plaintext. The plaintext is returned once, in `IssuedGrant::handle`, for the trusted application to pass to the agent. The store's `Debug` output omits its contents.
- **Lookup uses selector and verifier.** The first 8 bytes of the hash select a small set of candidates. The full 32-byte hash is then compared in constant time with `subtle`.
- **Handles stay off the admin API.** A handle authenticates only to its own grant. The admin API is a separate listener with its own credential (ADR 0002 §2.3, #357).

## Concurrency

One `RwLock` orders `revoke` against `authenticate` and `recheck`. Once `revoke` returns, no call that starts afterwards sees the grant or any of its descendants as live (test `revoke_racing_authenticate`). An executor that is already running is not interrupted (§8.2).

## Restart semantics (decided)

M18 provides only the in-memory store. Restarting Guard invalidates every grant and handle, which fails closed (MILESTONE_18 §19 risk 10). The trusted application must issue new grants after a restart. A persistent store (JSON with atomic rename, or SQLite) is a follow-up. If one is added, its `Grant` records must pass `ExecutionContext::from_chain`, which rejects any `schema_version` other than 1.

## Tests

`cargo test -p crom-guard store::` covers:
- unknown, expired, not-yet-valid and revoked handles, and a revoked parent;
- issuer-scoped revocation;
- the max-TTL limit and unknown actions at issuance;
- narrowing and widening delegations;
- that the plaintext handle is never stored;
- the revoke-versus-authenticate race;
- a proptest: every arbitrary `GrantSpec` is either rejected exactly when it is invalid, or authenticates with the same authority.
