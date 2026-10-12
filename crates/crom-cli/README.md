# crom-cli

The `crom` command-line interface.

This crate builds the `crom` binary — the user-facing entry point that
wires the workspace together: model providers, built-in tools, MCP servers,
session persistence, checkpointing, authentication, and the terminal UI.

## What it does

- Runs the agent against a workspace, interactively (via `crom-tui`) or
  headless.
- Manages provider login and credentials (`crom-auth`).
- Selects a provider — Anthropic, OpenAI, or Gemini.
- Persists, resumes, forks, and exports sessions (`crom-session`).
- Captures checkpoints and supports rewind (`crom-checkpoint`).
- Enforces permissions, policies, and built-in safety hooks (block dangerous
  Bash, block `.env` writes, redact secrets, cap patch size, audit log).

## Install

```sh
cargo install --path crates/crom-cli
```

This puts the `crom` binary on your `PATH`. See the
[workspace README](../../README.md) for authentication and quickstart.
