# bugfix-demo

This is a tiny Rust crate used to exercise the `code-agent` end-to-end flow.

## Intent

The crate exposes a single function, `parse_token`, in `src/lib.rs`. The
function should return the numeric portion of a string of the form `vN` (for
example, `parse_token("v123")` should return `Some(123)`).

## What the agent should do

1. Run `cargo test` to see the current state of the tests.
2. The `parse_token` function in `src/lib.rs` has a small bug. Read the file
   and the failing tests, identify the bug, and fix it with a single targeted
   edit.
3. Re-run `cargo test` and confirm all tests pass.
4. Summarize the change.

Edit only `src/lib.rs`. Do not modify the tests.
