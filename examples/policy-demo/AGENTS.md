# policy-demo

Tiny crate that parses simple `KEY=VALUE` config files. Used as the M2
policy-demo: it ships with one failing test, a `.env` file the agent
must not modify, and `.agent/settings.json` configuring policy and hooks.

## Rules

- Do **not** read or modify `.env` or `.env.*` files.
- Allowed cargo commands: `cargo test`, `cargo check`, `cargo fmt`, `cargo clippy`.
- Approval is required before running anything else.
