# AGENTS.md — beta-demo

Tiny crate used to demo the M9 beta UX (`crom doctor`, `crom
init`, `crom tui`, approval cards, `/changes`, `/diff`, `/status`).

## Workflow
- `cargo test` runs two tests; one fails (`parse_header_rejects_empty_name`).
- The agent's job is to flip the empty-name branch in `src/lib.rs` to
  return `Err("empty name")` so both tests pass.
- The agent must write/edit through the workspace approval flow and create
  a checkpoint before mutating.

## Sensitive paths
- Never modify `.agent/` or `tests/` without explicit approval.
