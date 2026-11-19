# ADR 0002: Crom Guard architecture, trusted computing base, and crate boundaries

- **Status:** Proposed — requires owner sign-off (`owner-action`)
- **Date:** 2026-11-19
- **Issue:** #344 · consumed by #345 (threat model), #347 (core types), #348 (grant lifecycle), #357 (server), #359 (Crom client)
- **Spec:** [MILESTONE_18](../../milestones/MILESTONE_18.md) §2, §7, §10

## 1. Context

M18 adds an enforcement point for agent-initiated business operations:
trusted principals, task-scoped grants, resource-level authorization, and
credential isolation (MILESTONE_18 §1). Every later M18 issue depends on
where that enforcement point runs, what it trusts, and what it may link.

Constraints from the plan:

- It must work for agents that are not Crom (§10.4: a standard-library
  Python client in CI, #358).
- A bug in the Crom runtime must not be able to widen authority (§7.4).
- It must be extractable to its own repository without untangling Crom
  internals.

Today's crate graph has no Guard code. `crom-core` depends on `crom-mcp`;
`crom-sdk` depends on `crom-core`, `crom-auth`, and `crom-tools`.

## 2. Decision

### 2.1 Two crates, one separate process by default

| Crate | Kind | Role |
|---|---|---|
| `crom-guard` | library | Authority model (execution context, grants, attenuation, revocation), Cedar policy decision point, resource-resolver and executor interfaces, enforcement pipeline, credential-broker interface, audit writer |
| `crom-guard-server` | binary `crom-guard` | Local service: MCP Streamable HTTP agent endpoint and admin API over `crom-guard` |

Both are workspace members with `publish = false` until the API stabilises.

**Deployment modes.**

- **Out-of-process (default, and the only mode M18 claims cover).** Guard runs
  as its own process, ideally as a separate OS user (§7.5). Agents reach it
  only over the agent endpoint.
- **In-process library mode** (`crom_guard::Guard`): allowed only for a
  trusted Rust host that embeds Guard in its own trusted process. The Crom
  runtime does **not** use this mode, so the runtime stays outside the TCB.
  (Resolves MILESTONE_18 §19 risk 14.)

### 2.2 Agent endpoint: MCP Streamable HTTP via `rmcp`

- Transport: MCP Streamable HTTP using `rmcp` (the official Rust MCP SDK;
  3.11.0 at time of writing, Apache-2.0, MSRV 1.88). The minor is pinned in
  #357 because of spec churn (§19 risk 8).
- **No stdio.** With stdio the agent launches Guard, so the agent process
  would own Guard's environment and credential, which breaks the boundary.
- **Not a proxy.** Guard serves only its own registered protected
  operations. Tool names, descriptions, and schemas come from Guard's
  registry (operator code); no downstream MCP server's metadata is forwarded.
  Tool annotations are informational and enforcement never reads them.
- Authentication: `Authorization: Bearer <session handle>`. Guard never
  forwards the agent's token downstream; executors use the credential broker.
- OAuth 2.1 resource-server metadata is out of scope for M18 (§6).

### 2.3 Admin API: separate listener, never reachable with a session handle

- Default: HTTP/JSON over a unix socket with mode `0600`, owned by Guard's
  user.
- Alternative: loopback TCP plus an admin token (for hosts without unix
  sockets or with the app under another user).
- The admin listener is distinct from the agent listener. A session handle
  presented to the admin API is rejected; an admin credential presented to
  the agent endpoint is rejected.
- **Only the admin API writes authority state** (grants, approvals,
  revocation). The single agent-facing exception is `delegate`, which can
  only narrow an existing grant (#353).

### 2.4 Grants: opaque, server-side; handles are random lookup keys

- A grant is a record in Guard's store. The agent holds a 256-bit random
  session handle; Guard stores only its hash (§8.1–8.2).
- Revocation and expiry are checked against the store on every call
  (§10.2 steps 1 and 6), so revocation is immediate within one Guard
  process.
- Restart semantics are decided in #348. The plan's default is to
  invalidate all handles on restart (fail closed), with persistence
  optional (§19 risk 10).

### 2.5 Trusted computing base

In the TCB for a protected operation (MILESTONE_18 §7.4):

1. `crom-guard`.
2. Application-supplied resource resolvers and executors registered as
   protected operations. They are trusted code; Guard cannot make a buggy
   executor safe (§19 risk 6).
3. `crom-guard-server` and its transport stack (`rmcp`, the HTTP server
   chosen in #357).
4. `cedar-policy`, plus the operator's policy set and schema.
5. The OS (process and file-permission separation) and, where a claim relies
   on it, the M17 sandbox.

**Not in the TCB:** the Crom runtime, the model and provider, any MCP client,
the agent's tools, and the agent-side SDK.

### 2.6 Crate boundary (mechanically checked)

- `crom-guard` depends on no `crom-*` crate.
- `crom-guard-server` depends on no `crom-*` crate other than `crom-guard`.
- Both rules cover normal **and** build dependencies (build scripts are code
  that runs at build time). Dev-dependencies are exempt, so tests may use
  Crom crates as clients.
- Enforced by `scripts/check-guard-deps.sh`, run in CI as
  `security / guard dependency boundary`. It walks `cargo tree -e normal,build`
  for both packages and fails on any other `crom-*` crate, direct or
  transitive.

Crom depends on Guard only as a client (#359), never the reverse.

### 2.7 Extraction path

Because of §2.6, the two crates can move to their own repository by copying
`crates/crom-guard*`, the check script, and the CI job, once the API
stabilises. M18 does not split the repository.

## 3. Alternatives considered

| Alternative | Why not |
|---|---|
| Enforcement inside the Crom runtime (extend `PolicyEngine`) | Puts the runtime in the TCB; unusable by non-Crom agents; a runtime bug or a widened tool rule (cf. #341/#342) widens authority. Crom's gate stays as the tool-name gate in front of Guard (§9.5) |
| In-process library as the default | Same as above for any agent runtime that embeds it. Kept only for trusted Rust hosts (§2.1) |
| stdio MCP transport | Agent owns Guard's process, env, and credential (§2.2) |
| Generic MCP proxy in front of third-party servers | Forwards untrusted tool metadata into the trusted path; Guard could not resolve resources or bind executors for operations it does not own |
| agentgateway (as library or sidecar) | Per MILESTONE_18 §10.3: not consumable as a Rust library, and as a sidecar it adds a second policy engine (CEL) to the TCB. A sidecar deployment in front of Guard is a follow-up candidate, not an M18 dependency |
| Self-contained capability tokens (Biscuit, macaroons, JWT) | Offline attenuation is attractive, but revocation then needs distribution, the token format and verifier join the TCB, and M18 has a single Guard process that can check a store directly. Biscuit stays a follow-up (§6, §9.1) |
| Admin operations on the agent endpoint, gated by a role | One listener means one bug away from an agent minting grants. Separate listener and credential instead (§2.3) |

## 4. Consequences

- Every protected operation costs one local HTTP round trip from the agent.
  Acceptable for the reference workload; measured in #363 (E5 latency).
- Crom needs its MCP HTTP client to carry the session handle as a static
  header outside model context (#359; §19 risk 9).
- A Guard restart invalidates outstanding handles by default; the trusted
  app must reissue (#348).
- Two more workspace members; Cedar's compile weight lands only on Guard
  crates (§19 risk 2, measured in #346).
- The dependency rule constrains reuse: Guard cannot borrow Crom's redaction,
  sandbox, or MCP code. Anything it needs is reimplemented or taken from a
  third-party crate, which keeps the TCB auditable on its own.

## 5. Open questions

1. Should the Guard process sandbox itself (landlock on its own process)?
   Recommended follow-up; not required for M18 claims (§19 risk 13).
2. Same-UID deployments: an agent outside the sandbox running as Guard's user
   can read Guard's credential file. M18 recommends a separate user and has
   #360's preflight warn; whether to refuse same-UID by default is open
   (§19 risk 7).
3. Grant persistence across restarts (#348).

## 6. Requirements handed on

| To | Requirement |
|---|---|
| #347 | Core types live in `crom-guard`; no type from a `crom-*` crate in Guard's public API |
| #348 | Handles stored hashed; restart semantics decided and documented |
| #353 | `delegate` is the only agent-reachable write and only narrows |
| #357 | Agent endpoint and admin API on separate listeners; unix socket `0600` default; pin `rmcp` minor; no stdio |
| #345 | Add Guard assets (A7–A10), boundaries, and the TCB of §2.5 to the threat model |
| #359 | Crom connects as an ordinary HTTP MCP client; handle in headers, never in model context |
