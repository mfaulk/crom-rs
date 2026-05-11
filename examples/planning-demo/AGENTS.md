# planning-demo

This crate has one intentionally broken function (`subtract`) and a failing
test (`subtract_works`). The M11 planning workflow should:

1. Draft a plan that inspects `src/lib.rs` and the failing test.
2. Propose a single `Edit` to `src/lib.rs` and a single `Bash(cargo test)`
   validation.
3. After user approval, act on the plan and complete the steps.

The plan store lives at `.agent/plans/`. The session log is at
`.agent/sessions/`.

## Notes

- Plans must be reviewed and approved before `code-agent act` runs them.
- Out-of-scope tool calls trigger the configured deviation policy
  (`ask` in this demo).
- `cargo test` validates the fix.
