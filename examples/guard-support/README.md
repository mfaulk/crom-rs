# guard-support

Customer-support reference deployment for Crom Guard (MILESTONE_18 §14).
The Guard binary with the support operations is the `crom-guard-support`
example of `crates/crom-guard-testkit`
([docs/guard/reference-deployment.md](../../docs/guard/reference-deployment.md)).
The Crom-based demo driver arrives with #362. This directory is not a crate.

| Path | Contents |
|---|---|
| `policies/support.cedarschema` | Entity model v1 ([ADR 0003](../../docs/security/decisions/0003-guard-policy-engine.md) §4.2) |
| `policies/support.cedar` | Policy set v1: tenant wall, delegated reads, refund limits, supervisor approval, export forbid |
| `guard.toml` | Reference Guard config; all paths relative, for a private state directory |
| `grants/acme-agent-operator.json` | `GrantSpec` the trusted app issues: acme operator, customer `acme-cus-000`, refunds up to 100.00, one level of `guard.delegate` |
| `clients/python/` | Standard-library Python MCP client, scripted §14.2 agent, and admin client ([README](clients/python/README.md)) |
| `run-python-client.sh` | Starts Guard and the business API, issues grants, runs the Python client (#358) |

## Run the Python client

```bash
examples/guard-support/run-python-client.sh   # from the repo root; needs cargo and python3 >= 3.10
```

## Validate

Requires `cedar-policy-cli` at the version Guard pins:

```bash
cargo install --locked cedar-policy-cli --version 4.15.0
scripts/validate-guard-policies.sh            # from the repo root
CEDAR=/path/to/cedar scripts/validate-guard-policies.sh path/to/policies
```

The script runs `cedar validate --validation-mode strict --deny-warnings` and
`cedar format --check` on each `.cedar` file against the directory's single
`.cedarschema`. CI runs it in the `security / Guard Cedar policies` job. After
editing a policy, run `cedar format -p policies/support.cedar` to normalize it.
