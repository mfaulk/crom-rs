# subagent-demo

This crate exists so the M6 demo has something concrete to review.

## Intent

A tiny Rust crate exposing `parse_token`, `compare_secret`, and a public
`ApiError` type. Each one has a deliberately seeded issue so the
specialized subagents shipped under `.agent/agents/` have findings to
report.

## What the parent agent should do

When given a prompt like

> "Review this crate for security, tests, and API design."

the parent agent should delegate to subagents:

1. `security-reviewer` — find input-validation / secret-handling issues.
2. `test-analyst` — find missing-test gaps and run `cargo test`.
3. `api-reviewer` — find documentation / API ergonomics issues.

The parent's final answer should synthesize each subagent's findings into
three short sections.

## Subagent definitions

See `.agent/agents/`.
