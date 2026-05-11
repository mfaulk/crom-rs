# planning-demo

End-to-end M11 planning demo. The crate ships with a planted bug in
`subtract` and a failing test `subtract_works`. Use `code-agent` to draft
and approve a plan, then act on it.

## Try it

```bash
cd examples/planning-demo

# 1. Draft a structured plan.
code-agent plan "Fix the failing test in subtract"

# 2. Inspect the plan.
code-agent plans show latest

# 3. Approve it.
code-agent plans approve latest

# 4. Execute the approved plan.
code-agent act latest

# 5. (Optional) Re-plan if execution diverges.
code-agent replan latest "Tests still fail; try a different approach"
```

The plan store under `.agent/plans/` keeps every revision; archived
runs remain inspectable with `code-agent plans show <ID>`.

## Offline checks

```bash
cargo test --workspace
```

`cargo test` is the validation command declared in
`.agent/settings.json`, so it runs after the agent finishes.
