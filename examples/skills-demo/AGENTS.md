# skills-demo

Tiny crate exercised by the M7 Skills + slash commands alpha demo.

The `.agent/skills/` directory contains two example Skills:
- `release-pr` — packages a workflow that reads `CHANGELOG.md` and
  produces a release PR summary, optionally using
  `templates/pr-summary.md` as scaffolding.
- `rust-api-review` — packages a workflow that reviews the public API
  surface of this crate and suggests improvements.

The `.agent/commands/` directory contains two slash commands:
- `release-pr.md` — explicit invocation that mirrors the Skill.
- `review-changes.md` — uses safe `@file` and `!`bash`` expansion to
  pre-load the diff before the model turn.
