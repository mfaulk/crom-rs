---
description: Review changes since a base ref. Pre-loads the diff and changed file list before the model turn.
argument-hint: "[base-ref]"
allowed-tools:
  - Read
  - Grep
  - Glob
  - Bash(git diff *)
  - Bash(git log *)
---

Review the changes since $1.

Changed files:
!`git diff --name-only $1`

Detailed diff:
!`git diff --stat $1`

Look for:
- security issues
- API breakage
- missing tests

Be concise: one bullet per finding.
