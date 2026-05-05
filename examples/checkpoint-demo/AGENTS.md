# checkpoint-demo

Tiny crate demonstrating the M3 checkpoint + rewind workflow. The current
`parse_tokens` implementation does not trim whitespace, so the bundled
`tokens_are_trimmed` test fails. Your job:

1. Refactor `parse_tokens` so it trims whitespace from each token.
2. Add a new test file documenting at least one additional case (e.g.
   tokens with internal spaces, or empty input).

It's okay to make a mistake — checkpointing captures the workspace before
each edit, so the demo can rewind with `code-agent rewind <id>`.

Constraints:

- Do **not** modify `.env`/`.env.*` files.
- Allowed Bash commands: `cargo test`, `cargo check`, `cargo fmt`,
  `cargo clippy`, `cargo build`. Anything else needs explicit approval.
- The Bash checkpoint policy is `allow_with_warning`: shell commands run,
  but the demo will display a warning that Bash file changes are not
  captured by checkpointing.
