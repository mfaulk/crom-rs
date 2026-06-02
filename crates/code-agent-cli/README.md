# code-agent-cli

The `code-agent` command-line interface.

This crate builds the `code-agent` binary — the user-facing entry point that
wires the workspace together: model providers, built-in tools, MCP servers,
session persistence, checkpointing, authentication, and the terminal UI.

## What it does

- Runs the agent against a workspace, interactively (via `code-agent-tui`) or
  headless.
- Manages provider login and credentials (`code-agent-auth`).
- Selects a provider — Anthropic, OpenAI, or Gemini.
- Persists, resumes, forks, and exports sessions (`code-agent-session`).
- Captures checkpoints and supports rewind (`code-agent-checkpoint`).
- Enforces permissions, policies, and built-in safety hooks (block dangerous
  Bash, block `.env` writes, redact secrets, cap patch size, audit log).

## Install

```sh
cargo install --path crates/code-agent-cli
```

This puts the `code-agent` binary on your `PATH`. See the
[workspace README](../../README.md) for authentication and quickstart.
