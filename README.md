# Crom

Crom is a Rust-native agent runtime and security framework for building
autonomous software that operates under explicit, enforceable authority.

Today, an AI agent usually runs with the full privileges of whoever started
it: their shell, files, network, and credentials. A model's good behaviour is
the only thing standing between a misleading instruction and a real effect.
Crom takes a different approach: it separates probabilistic agent reasoning
from deterministic authorization, execution isolation, and auditing, so the
limits on what an agent may do are enforced by software rather than by
prompts, and every attempted action leaves a record.

Crom is both a working agent runtime (an embeddable Rust SDK, a CLI, and a
terminal UI, focused on coding tasks today) and the foundation for that
security model. The long-term goal is to let agents work across real systems
without inheriting unrestricted human privileges. Part of that is in place;
much of it is still planned (see [Limitations and roadmap](#limitations-and-roadmap)).

## What works today

- **Embeddable Rust SDK** (`crom-sdk`): typed requests, results, and events;
  approval callbacks; custom tools; run budgets; in-memory and patch-only
  workspaces. No TypeScript or Python runtime required.
  Conservative defaults: read-only, no shell, no MCP.
- **CLI and terminal UI** (`crom`): one-shot runs, chat, sessions,
  checkpoints and rewind, plan/act, subagents, skills, and plugins.
- **Multiple model providers:** Anthropic, OpenAI, Gemini, and
  OpenAI-compatible endpoints such as DeepSeek or a local Ollama/vLLM server,
  through one provider-profile system. Low-cost models are first-class.
- **Tools and MCP:** built-in file and shell tools, plus MCP servers over
  stdio and Streamable HTTP, with search-based loading for large tool
  catalogs.
- **Permission policy:** every tool call passes a policy gate before it runs:
  permission modes, allow/deny rules, hooks, approval callbacks, and a
  default deny. A cloned repository's `.agent/` settings and MCP config are
  untrusted until you trust the workspace, and until then may only tighten
  policy.
- **Linux sandbox for agent shell commands:** landlock, seccomp, and
  resource limits. Writes are confined to the workspace and a private temp
  directory, there is no network by default, the environment is reduced to
  an allowlist (provider keys never reach the command), and every
  child process is cleaned up. When the sandbox is required but cannot be
  enforced, the command is refused.
- **Audit:** every gated tool call produces a structured `SecurityDecision`
  record (what was attempted, which layer decided, and the outcome), with
  secrets redacted.
- **Measurement:** a reproducible evaluation harness with leveled coding
  tasks and hidden tests, spend-capped live runs, a scoreboard, and an
  adversarial security suite. The suite scores attacks on the effects they
  actually produce, not on whether the model refused.

## Limitations and roadmap

What exists today protects a **coding agent working in a local workspace**.
Several important pieces are not built yet:

- **Authorization is not yet resource-aware.** Policy decides on tool names
  and a few arguments (shell command, file path). It cannot express rules
  like "this customer's records only" or "refunds up to $100".
- **Delegated, task-scoped authority is planned, not implemented.** This
  includes trusted principals, grants an application issues to an agent,
  narrower authority for subagents, keeping downstream credentials away from
  agents, and enforcement usable from other agent frameworks. It is the goal
  of **Crom Guard**, planned for milestone M18
  ([plan](docs/milestones/MILESTONE_18.md),
  [issues](https://github.com/mfaulk/crom-rs/milestone/2)). The architecture
  is proposed in [ADR 0002](docs/security/decisions/0002-guard-architecture.md),
  and the Cedar policy model in
  [ADR 0003](docs/security/decisions/0003-guard-policy-engine.md).
  `crates/crom-guard` defines the core types and traits
  ([docs/guard/README.md](docs/guard/README.md)) and the hash-chained audit
  log ([docs/guard/audit.md](docs/guard/audit.md)), and the downstream
  credential brokers ([docs/guard/credentials.md](docs/guard/credentials.md)),
  with no enforcement yet;
  `crates/crom-guard-server` only runs `crom-guard audit verify <log>`. `crates/crom-guard-testkit` provides the controlled business API and
  effect ledger the M18 tests measure against, and the reference policy set
  (`examples/guard-support/policies/`) is validated in CI by
  `scripts/validate-guard-policies.sh`.
- **The sandbox is opt-in and Linux-only.** `sandbox.mode` defaults to `off`.
  It covers agent shell commands, not MCP servers or validation commands.
  Other platforms refuse sandboxed commands rather than run them unprotected.
- **No OS keychain yet** ([#205](https://github.com/mfaulk/crom-rs/issues/205)).
  Use environment variables or the opt-in `0600` file store.
- **Security results are bounded.** The adversarial suite shows that the
  tested attacks produce no effects when the sandbox is required. It is not a
  claim of immunity to prompt injection. See the
  [threat model](docs/security/threat-model.md) for what is and is not
  covered.
- **No published benchmark numbers yet.** The live baseline is in progress
  ([#297](https://github.com/mfaulk/crom-rs/issues/297)).
- Not published to crates.io. Install from source.

Plans live in [`docs/milestones/`](docs/milestones/) and the
[issue tracker](https://github.com/mfaulk/crom-rs/issues).

## Quickstart

Requires Rust 1.89 or newer. The first three steps need no API key and no
network services.

```sh
git clone https://github.com/mfaulk/crom-rs.git
cd crom-rs
cargo build --workspace
cargo test --workspace
```

**Try it offline.** The `fake` provider is a deterministic stand-in that
echoes your prompt. It exercises the real CLI, tool, and session path, but it
does not reason:

```sh
cargo run -p crom-cli -- run --provider fake "List the files here" --allowed-tools Read,Glob

# Run the offline evaluation smoke suite (expects "resolved 5/5").
cargo run -p crom-cli -- eval run --suite smoke --provider fake

# Embed the runtime from Rust: the headless SDK demo, also offline.
cargo run --manifest-path examples/headless-sdk-demo/Cargo.toml -- simple-patch
```

**Run a real model.** This needs an API key and spends provider credit:

```sh
cargo install --path crates/crom-cli    # puts `crom` on your PATH

export ANTHROPIC_API_KEY=sk-ant-...     # or OPENAI_API_KEY / GEMINI_API_KEY
crom run "Find the bug in src/lib.rs and fix it" \
    --allowed-tools Read,Glob,Grep,Edit --max-turns 8

# A low-cost alternative, using an OpenAI-compatible provider profile:
export DEEPSEEK_API_KEY=sk-...
crom run --provider deepseek "Summarize what this crate does" --allowed-tools Read,Glob,Grep
```

`crom --help` lists every subcommand. Before letting the agent run shell
commands on Linux, see the [sandbox guide](docs/security/sandbox.md).

## Documentation

- [CLI guide](docs/cli.md): install, authentication, MCP servers, workspace
  trust, development
- [SDK overview](docs/sdk/overview.md) and [quickstart](docs/sdk/quickstart.md)
- [Providers and profiles](docs/providers.md)
- Security: [threat model](docs/security/threat-model.md),
  [sandbox](docs/security/sandbox.md), [audit events](docs/security/audit.md),
  [adversarial suite](docs/security/adversarial-suite.md)
- [Evaluation harness](docs/eval.md)
- [Examples](examples/README.md)
- [Milestones](docs/milestones/)

## Contributing

Issues and pull requests are welcome. See the
[development notes](docs/cli.md#developing) for lint, format, and sandbox
test commands. Commits follow
[Conventional Commits](https://www.conventionalcommits.org/).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
