# guard-support

Customer-support reference deployment for Crom Guard (MILESTONE_18 §14).
Only the Cedar policy set exists so far; the trusted app, business API, and
demo driver arrive with #362. This directory is not a crate yet.

| Path | Contents |
|---|---|
| `policies/support.cedarschema` | Entity model v1 ([ADR 0003](../../docs/security/decisions/0003-guard-policy-engine.md) §4.2) |
| `policies/support.cedar` | Policy set v1: tenant wall, delegated reads, refund limits, supervisor approval, export forbid |

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
