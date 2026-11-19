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

Every request needs `Authorization: Bearer <secret>` for one of the configured `ApiCredential`s. Without one, the API returns `401`. An id that belongs to another tenant returns the same `404` as a missing id.

| Method | Path | Ledger effect |
|---|---|---|
| GET | `/tenants/{t}/customers/{id}` | `customer_read` |
| GET | `/tenants/{t}/customers/{id}/invoices` | `invoice_list` (all returned ids) |
| GET | `/tenants/{t}/invoices/{id}` | `invoice_read` (response has an `ETag`) |
| POST | `/tenants/{t}/invoices/{id}/refunds` `{"amount_cents": n}` | `refund` |
| POST | `/tenants/{t}/customers/export` | `customer_export` (all returned ids) |

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

## Not yet included

`support_ops`, the `ProtectedOperation` resolvers and executors for `customer.read`, `invoice.list`, `refund.issue` and `customer.export`, is waiting on the core types in [#347](https://github.com/mfaulk/crom-rs/issues/347).

## Tests

```sh
cargo test -p crom-guard-testkit
```
