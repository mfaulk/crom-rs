# bugfix-demo

This is a tiny Rust crate used to exercise the `code-agent` end-to-end flow.

## Intent

The crate exposes a single function, `parse_token`, in `src/lib.rs`. The
function should return the numeric portion of a string of the form `vN` (for
example, `parse_token("v123")` should return `Some(123)`).

## What the agent should do (initial run)

1. Run `cargo test` to see the current state of the tests.
2. The `parse_token` function in `src/lib.rs` has a small bug. Read the file
   and the failing tests, identify the bug, and fix it with a single targeted
   edit.
3. Re-run `cargo test` and confirm all tests pass.
4. Summarize the change.

Edit only `src/lib.rs`. Do not modify the tests.

## Follow-up task (used by `examples/e2e-demo.sh` after `code-agent resume`)

If the user's follow-up prompt asks for a SUMMARY:

1. Use `Glob` and/or `Grep` to find every file in this crate that mentions
   `parse_token` (source and tests).
2. Use `Write` to create `SUMMARY.md` at the crate root containing:
   - a one-line description of the bug,
   - the exact fix that was applied,
   - a bulleted list of every file that references `parse_token`.
3. Do not modify `src/lib.rs` or any file under `tests/` during the follow-up.
4. Do not run `cargo test` again on the follow-up; the bug is already fixed.
