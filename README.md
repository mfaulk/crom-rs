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
- **Crom Guard (M18):** enforceable, task-scoped authority for agents that
  act on business systems, usable from any MCP client. A trusted application
  issues an agent a grant (tenant, delegator, actions, customers, refund
  limits, expiry); the `crom-guard` service authorizes every call to a
  protected operation with Cedar policies, binds approvals to the exact
  action, lets agents delegate only narrower authority to subagents, holds
  the downstream credential itself, and writes a hash-chained audit log
  (`crom-guard audit verify`). `crom-guard check` is a static,
  point-in-time preflight: it fails a deployment that breaks one of the
  isolation-profile conditions it checks (file ownership and modes, Crom's
  sandbox and MCP settings), and cannot see changes made after it runs or
  conditions outside that list. Start with the
  [tutorial](docs/guard/tutorial.md); design and API in
  [docs/guard/](docs/guard/README.md); measured results and their limits in
  [docs/benchmarks/M18-guard.md](docs/benchmarks/M18-guard.md). Crom connects
  to Guard as an MCP client (`ToolPolicy::guard`,
  [docs/sdk/guard.md](docs/sdk/guard.md)), and the reference application is
  [examples/guard-support/](examples/guard-support/README.md).
- **Measurement:** a reproducible evaluation harness with leveled coding
  tasks and hidden tests, spend-capped live runs, a scoreboard, and an
  adversarial security suite. The suite scores attacks on the effects they
  actually produce, not on whether the model refused.

## Limitations and roadmap

What exists today protects a **coding agent working in a local workspace**,
and, through Crom Guard, the protected operations of one reference
application. Several important pieces are not built yet:

- **Crom Guard v0 (M18) is a reference, not a product.** Its claims hold in
  one supported deployment (Linux, Guard on its own state directory, agent
  Bash sandboxed without network or the agent under its own uid, Guard as
  the only path to the business API), for the reference support operations
  and policies, against the tested scenarios
  ([report](docs/benchmarks/M18-guard.md#what-the-results-do-not-show)).
  Grants live in memory and do not survive a restart; there are no
  self-contained capability tokens, OAuth flows, or distributed revocation;
  executors and resolvers are trusted application code; and no live-model
  run has been made yet (the scripted lanes are the evidence). Follow-ups are
  listed in [MILESTONE_18 §21](docs/milestones/MILESTONE_18.md#21-follow-up-milestone-candidates).
- **Crom's own policy is not resource-aware.** Outside Guard, policy decides
  on tool names and a few arguments (shell command, file path). Rules like
  "this customer's records only" need a Guard protected operation.
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
- **Benchmarks are early.** The M17 live capability baseline
  ([docs/benchmarks/M17-baseline.md](docs/benchmarks/M17-baseline.md)) and
  the M18 Guard report ([docs/benchmarks/M18-guard.md](docs/benchmarks/M18-guard.md),
  scripted lanes only) are first measurements on one model pair and one
  reference application.
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
- Crom Guard: [tutorial](docs/guard/tutorial.md), [core types](docs/guard/README.md),
  [pipeline](docs/guard/pipeline.md), [server and admin API](docs/guard/server.md),
  [deployment](docs/guard/deployment.md), [M18 report](docs/benchmarks/M18-guard.md)
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
