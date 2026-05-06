# Project Instructions for Claude

## Milestone documentation

When a milestone is completed, add a corresponding milestone doc under
`docs/milestones/` named `MILESTONE_NN.md` (zero-padded, matching the
existing series).

A milestone is "completed" when its feature commit lands on `main`
(typically a `feat(...)` commit whose subject or body identifies the
milestone, e.g. `M9`, `(M9)`, or `milestone 9`).

The new doc should:

- Follow the structure of the most recent existing milestone doc
  (theme, deliverables grouped by area, settings/CLI surface where
  relevant, safety notes, demo, tests, out-of-scope).
- Be derived from the milestone's merge commit message and the actual
  files it changed — not invented. Verify referenced paths, settings
  keys, CLI commands, and event names exist in the tree before writing
  them down.
- Be concise and technically specific. No marketing language.

If a milestone ships across multiple PRs, update the existing doc
rather than creating a duplicate.
