# Support reference deployment

[#358](https://github.com/mfaulk/crom-rs/issues/358). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §14. A runnable Guard with the support operations, used by the Python reference client and meant for reuse by #360, #361 and #362.

| Piece | Path |
|---|---|
| Guard binary with the support operations | `crates/crom-guard-testkit/examples/crom-guard-support.rs` |
| Registry it serves | `crom_guard_testkit::support_registry` |
| Config | `examples/guard-support/guard.toml` |
| Policies and schema | `examples/guard-support/policies/` |
| Grant fixture | `examples/guard-support/grants/acme-agent-operator.json` |
| Driver | `examples/guard-support/run-python-client.sh` |
| Python client | `examples/guard-support/clients/python/` ([README](../../examples/guard-support/clients/python/README.md)) |
| Crom agent settings for the isolation profile (#360) | `examples/guard-support/agent/crom-settings.json` |

## `crom-guard-support`

The stock `crom-guard` registers no operations ([server.md](server.md)). This example binary calls `crom_guard_server::cli::main_with` with `support_registry`: `customer.read`, `invoice.list`, `refund.issue`, `customer.export` and `guard.delegate`. Its CLI is the stock one (`serve`, `policy validate`, `audit verify`).

```sh
cargo build --locked -p crom-guard-testkit --example crom-guard-support
# binary: <target-dir>/debug/examples/crom-guard-support
```

It is a testkit example, not a `crom-guard-server` target: `crom-guard-testkit` takes `crom-guard-server` as a dev-dependency, and `crom-guard-server` never depends on the testkit (`scripts/check-guard-deps.sh`).

Under `serve` it also runs the testkit business API in the same process, on its own thread and runtime. The `support_ops` resolvers read the API's records in process (the application's trusted data access), so the two cannot be split. Environment, read only by `serve`:

| Variable | Default | Meaning |
|---|---|---|
| `CROM_GUARD_SUPPORT_CREDENTIALS` | required | The broker credentials file that `guard.toml` names. The business API accepts exactly these secrets (`ApiCredential::from_broker_json`) |
| `CROM_GUARD_SUPPORT_API_LISTEN` | `127.0.0.1:8480` | Business API address; must be loopback |
| `CROM_GUARD_SUPPORT_LEDGER` | in memory | JSONL ledger path, truncated at start |

Any of these failing exits 1 before a listener starts.

## `guard.toml`

Every path in it is relative (`admin.sock`, `admin.token`, `credentials.json`, `audit.jsonl`, `policies/…`), so it is meant to be copied, with `policies/`, into a private 0700 state directory that also holds a 0600 `admin.token` and `credentials.json`. Guard's startup checks ([server.md](server.md), "Refusing to start") apply. The agent endpoint defaults to `127.0.0.1:8470`; pass `--agent-listen` to move it.

## Driver

`run-python-client.sh` sets up that state directory with a random admin token and business-API secret, starts `crom-guard-support` on free loopback ports, issues grants through the admin socket with `trusted_app.py`, and runs the scripted Python agent. It then checks that the agent never held the business-API credential, that no log contains a secret, and that the audit log verifies. The client README has the matrix and the checks.

The driver runs the Python agent on Guard's uid without a sandbox, so it is outside the isolation profile and `crom-guard check` fails for it; it demonstrates the properties that do not depend on isolation (GP1–GP5, GP7, GP8). Bash and credential-file bypass, and the profile that prevents it, are covered by [deployment.md](deployment.md) (#360).

The admin client (`trusted_app.py`) and the grant fixture are independent of the Python agent, so other drivers can reuse them to issue grants, revoke them, and approve pending requests.
