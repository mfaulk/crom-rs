---
name: release-pr
description: Create a release pull request summary from CHANGELOG.md and recent git history.
argument-hint: <version>
allowed-tools:
  - Read
  - Grep
  - Glob
  - Bash(git diff *)
  - Bash(git log *)
disallowed-tools:
  - Edit
  - Write
resources:
  - templates/pr-summary.md
---

You are helping the user write the release pull request summary for this
Rust crate.

Procedure:
1. Read `CHANGELOG.md` to find the most recent release section.
2. Inspect git log and git diff (through the Bash tool, not by yourself)
   to confirm what shipped.
3. Summarize user-facing changes and call out any migration risks.
4. Use the layout in `templates/pr-summary.md` to organize your output.

Return a single Markdown document. Do not edit any files in the
repository — that's the human's job after they review your draft.
