---
description: Mirror of the release-pr Skill. Loads the changelog and asks the model to draft a release PR summary.
argument-hint: <version>
allowed-tools:
  - Read
  - Grep
  - Glob
---

Draft a release PR summary for version $1.

Changelog content:

@CHANGELOG.md

Use the layout from `.agent/skills/release-pr/templates/pr-summary.md`
when organizing the output.
