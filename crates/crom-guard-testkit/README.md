# crom-guard-testkit

Controlled business API and effect ledger for testing Crom Guard (M18,
[#356](https://github.com/mfaulk/crom-rs/issues/356)). It is a test fixture,
not part of the Guard TCB, and is not published.

M18 security results are computed from this ledger, independently of Guard's
own audit log ([MILESTONE_18 §13.2](../../docs/milestones/MILESTONE_18.md)).

## Data

`seed()` is deterministic. It builds two tenants (`acme`, `globex`). Each has:

- three users: `<tenant>-agent-operator`, `<tenant>-supervisor` and `<tenant>-viewer`;
- ten customers, `<tenant>-cus-000` to `-009`;
- two invoices per customer, `<tenant>-inv-000` to `-019`.

Every customer note and invoice memo carries a canary, `canary(tenant, id)`. Every third invoice memo (`n % 3 == 0`) also carries `data::INJECTED_INSTRUCTION`, for injection scenarios.

## HTTP API

Every request needs `Authorization: Bearer <secret>` for one of the configured `ApiCredential`s. Without one, the API returns `401`. An id that belongs to another tenant returns the same `404` as a missing id. Both carry `X-Resource-Not-Found: true` (`RESOURCE_NOT_FOUND_HEADER`); a `404` for an unknown route does not.

| Method | Path | Ledger effect |
|---|---|---|
| GET | `/tenants/{t}/customers/{id}` | `customer_read` |
| GET | `/tenants/{t}/customers/{id}/invoices` | `invoice_list` (all returned ids) |
| GET | `/tenants/{t}/invoices/{id}` | `invoice_read` (response has an `ETag`) |
| POST | `/tenants/{t}/invoices/{id}/refunds` `{"amount_cents": n}` | `refund` |
| POST | `/tenants/{t}/customers/export` | `customer_export` (all returned ids) |

Customers and invoices each carry a `version`, starting at 1. Customer reads, invoice lists (both use the customer version) and invoice reads return it as `ETag`. They also accept an optional `If-Match` and return `412` if it is stale.

Refunds need `If-Match: "<version>"`:

| Condition | Status |
|---|---|
| `If-Match` header missing | `428` |
| Stale version | `412` |
| Amount is 0 or more than the remaining balance | `422` |
| Success | `201`, and the version is bumped |

## Ledger

`Ledger::in_memory()` or `Ledger::with_file(path)` (JSONL, truncated at start).

- Each successful request appends exactly one `LedgerEntry`:
  - `seq`, `tenant`, `credential_id` (never the secret);
  - `guard_decision_id`, taken from `X-Guard-Decision-Id` (`DECISION_ID_HEADER`);
  - the effect.
- Rejected requests append nothing.
- The entry is written before the effect is committed. If the write fails, the request returns `503` and nothing changes.

`Ledger::read_file` reads a ledger back, and `BusinessApi::reset()` restores the seed and empties the ledger.

## Trusted access (not ledgered)

These methods model the application's own data access. They are not reachable over HTTP and append nothing to the ledger:

- `lookup_customer(tenant, id)` and `lookup_invoice(tenant, id)` are tenant-scoped reads, used by the `support_ops` resolvers. An id from another tenant returns `None`, the same as a missing id.
- `update_customer(tenant, id, f)` and `update_invoice(tenant, id, f)` change a record out of band and bump its version. Tests use them to change ownership or state between authorize and execute.

## Usage

```rust
let api = BusinessApi::new(
    vec![ApiCredential { id: "guard".into(), secret: guard_secret }],
    Ledger::with_file(&dir.join("ledger.jsonl"))?,
);
let running = api.serve("127.0.0.1:0".parse()?).await?;
// ... point Guard's executors at running.base_url() ...
let effects = api.ledger().entries();
```

For the E2 "Guard off" baseline, configure a credential for the trusted test driver and call the API directly. Its entries have `guard_decision_id: None`.

## `support_ops`

`ProtectedOperation` implementations for the support reference deployment ([#350](https://github.com/mfaulk/crom-rs/issues/350)). `operations(&api, &SupportConfig::new(running.base_url()))` builds all four:

| Action | Arguments | Cedar resource | Downstream call |
|---|---|---|---|
| `customer.read` | `customer_id` | `Support::Customer` | `GET /tenants/{t}/customers/{id}` |
| `invoice.list` | `customer_id` | `Support::Customer` | `GET /tenants/{t}/customers/{id}/invoices` |
| `refund.issue` | `invoice_id`, `amount_cents` (≥ 1) | `Support::Invoice` | `POST /tenants/{t}/invoices/{id}/refunds` |
| `customer.export` | `customer_id` | `Support::Customer` | none: always fails (see below) |

- **Schemas.** Every schema sets `additionalProperties: false` and has no tenant field, so a forged `tenant_id` fails validation. Ids must match `^[A-Za-z0-9._:@-]+$`.
- **Resolvers.** `SupportResolver` looks the id up with `lookup_*` in the principal's tenant. Attributes follow `examples/guard-support/policies/support.cedarschema`:
  - `Customer`: `tenant`, with the `Tenant` as its parent;
  - `Invoice`: `tenant`, `customer`, `amount_cents` and `refunded_cents`, with the `Customer` as its parent.

  The version is the record's `version`. Not-found and other-tenant both return `ResolveError::NotFound`. The lookup runs inside the returned future, on Tokio's blocking pool when a runtime is present. Each resolver is wrapped in `crom_guard::TimeoutResolver` (`SupportConfig::resolve_timeout`, 2 s by default). A timeout, or a panicked lookup, returns `Unavailable`, which is a deny.
- **Context.** `refund.issue` adds `amount_cents` as a Cedar `Long`. The other actions add nothing.
- **Executors.** They follow the `crom_guard::ConditionalCall` contract:
  - the tenant comes from the principal, and the id from `action.resource()`, never from the arguments;
  - `If-Match` carries `action.resource_version()`;
  - `X-Guard-Decision-Id` carries `action.decision_id()`;
  - the bearer is set inside `CredentialLease::with_secret`, and redirects are disabled.

  A `412`, or a `404` marked with `X-Resource-Not-Found`, returns `ExecuteError::Precondition`, and the API performed no effect. An unmarked `404` returns `ExecuteError::Failed`. `refund.issue` reads only `amount_cents` from the arguments.
- **Export never executes.** `customer.export` resolves the named customer, because the schema declares a `Customer` resource. The export endpoint is tenant-wide, though, so a policy that permitted export on one customer would leak every customer in the tenant. The executor therefore always returns `ExecuteError::Failed` without a downstream call or ledger entry. A real export needs a `Tenant`-typed resource, which is a schema change and out of scope for v1. The operation exists so that `no-bulk-export` can forbid it (G11).

`support_registry(&api, &config)` returns an `OperationRegistry` with all four plus `guard.delegate`. `SupportOperation::with_description` advertises a different tool description (G17 serves a hostile one; `scenario::scenario_registry` applies a fixture's `tool_descriptions`). `ApiCredential::from_broker_json` reads a `FileCredentialBroker` file, so the API can accept exactly the credentials Guard's broker hands out.

## `crom-guard-support` example binary

`examples/crom-guard-support.rs` is the stock `crom-guard` CLI with `support_registry` compiled in, plus the business API in the same process. It is the reference deployment's Guard ([docs/guard/reference-deployment.md](../../docs/guard/reference-deployment.md)):

```sh
cargo build --locked -p crom-guard-testkit --example crom-guard-support
CROM_GUARD_SUPPORT_CREDENTIALS=credentials.json CROM_GUARD_SUPPORT_LEDGER=ledger.jsonl \
  <target-dir>/debug/examples/crom-guard-support serve --config guard.toml
```

`crom-guard-server` is a dev-dependency here, for the example and the scenario driver only. It must never depend on this crate (`scripts/check-guard-deps.sh`).

## Tests

```sh
cargo test -p crom-guard-testkit
```

- `tests/pipeline_e2e.rs` runs a full `crom_guard::Guard` over `support_ops`, a `FileCredentialBroker`, and a `FileAuditSink`, against the HTTP API (#351). The ledger is the oracle: an allowed refund has one effect carrying the decision id; an ownership change or revoke between authorize and execute has none; `customer.export` is denied, and fails closed even under a permitting policy.
- `tests/canary.rs` checks that the broker's secret and the session handle never appear in any `CallOutcome`, audit line, ledger entry, or `tracing` output. It installs a global trace subscriber, so it is its own test binary.
  The test environment registers `guard.delegate`; the canary also delegates a child and checks that the child handle appears only in the delegate result (#353).
- `tests/guard_adversarial.rs` runs the scenarios below in every lane and writes `$CARGO_TARGET_TMPDIR/guard-scenarios/report.json`. It needs `python3` 3.10 or newer for the Python client lane; without it, that lane is reported as skipped. The `crom` lane (`tests/crom_lane/mod.rs`, #413) drives each call through a scripted Crom run; for it the crate dev-depends on `crom-sdk` (`fake-provider`) and `crom-mcp`.
- `tests/guard_experiments.rs` runs the MILESTONE_18 §13.4 experiments (#363): E1 (the hand-written authorization matrix `fixtures/experiments/e1-matrix.json`, in process and through the reference server), E2 (Guard on vs the direct-credential baseline), E4a (provider transcripts in `fixtures/experiments/e4a/` replayed through a Crom SDK agent against the reference server; dev-depends on `crom-sdk` and `wiremock`), and E5 (latency, reported only). Each writes `$CARGO_TARGET_TMPDIR/guard-experiments/<e>.json`. `src/experiments.rs` holds the report types and the reproducibility metadata, which the scenario report carries too. See [docs/guard/experiments.md](../../docs/guard/experiments.md).
- `tests/bypass.rs` (Linux) attacks a Guard child process from agent Bash run through Crom's `BashTool`, with the sandbox `required` and `off`, and as a separate uid via `sudo -n` (#360, G13, G14, G16, G18). The ledger and audit log are the oracle. It dev-depends on `crom-core` and `crom-tools`; see [docs/guard/deployment.md](../../docs/guard/deployment.md).

## Scenarios (`scenario`)

`fixtures/scenarios/gNN-*.json` holds the Guard adversarial scenarios G1–G18 (#361), one per file. `scenario::FIXTURES` embeds them. `scenario::run(&fixture, &env)` replays one fixture and returns a `ScenarioReport` scored on this crate's ledger. A `ScenarioEnv` is where it runs:

- `InProcessEnv`: `Guard::call` with a manual clock, approvals, and a file audit log;
- `DirectEnv`: the unguarded baseline, a direct business-API tool holding the full credential;
- the driver adds the reference server, the server with the Python client, and the server with Crom as the client (`crom`).

G13, G14 and G18 run from agent Bash in the Linux isolation lanes (sandbox required, required with network, off, and a separate uid; harness in `tests/isolation/mod.rs`), and G17 against `crom-guard check` (#360) as well as in the step lanes with a hostile description served through Guard (#413). A lane that cannot run is reported as skipped, never as passing. The fixture format, probes, verdicts and `report.json` schema are in [docs/guard/scenarios.md](../../docs/guard/scenarios.md).
