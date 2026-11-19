# Guard deployment and isolation profile

[#360](https://github.com/mfaulk/crom-rs/issues/360). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §7.5 and §11; threat model [§9](../security/threat-model.md#9-crom-guard-m18) (GP6).

Guard enforces only the path it mediates. "A protected operation needs Guard's authorization" holds only if the agent has no other route to the business API or to Guard's authority: no direct HTTP, no shell read of Guard's credential, token or environment, no write to its audit log, no admin socket, and no other MCP server. This page defines the deployment in which that holds, the `crom-guard check` preflight that refuses other deployments, and the bypass tests that attack it.

## Isolation profile

```
host (Linux, landlock ABI ≥ 3)
├─ crom-guard (user A)
│    state dir 0700, owned by A:
│      guard.toml, policies/          not writable by others
│      credentials.json, admin.token  0600
│      audit.jsonl                    0600, hash-chained
│      admin.sock                     0600 unix socket (the admin API)
│    agent endpoint 127.0.0.1:<port>/mcp
├─ business API  127.0.0.1:<port>, accepts only Guard's credential
├─ trusted app   talks to admin.sock as user A
└─ agent, one of:
     (a) Crom on user A, Bash with sandbox.mode = "required", network = false,
         Guard as its only MCP server (examples/guard-support/agent/crom-settings.json)
     (b) any client under its own user B ≠ A, ≠ root
```

| Requirement | Why | Enforced by |
|---|---|---|
| Guard runs as its own unprivileged user | Guard's files and `/proc/<pid>/environ` are then closed to every other uid | `check`: `guard-user` |
| Every directory holding the credential file, admin token, audit log or admin socket is 0700 and owned by Guard | No other uid can read, list, replace, or connect to them | `check`: `state-dir` |
| Credential file and admin token are 0600 regular files owned by Guard | As above, per file | `serve` and `check`: `credentials`, `admin-token` |
| `guard.toml`, the schema, the policies, the state directory, and every ancestor of each up to `/` are owned by Guard or root and not group/world-writable (a sticky directory such as `/tmp` aside) | Whoever can rename an ancestor can swap in their own policy, credential or socket | `check`: `config-file`, `policy-files`, `state-path` |
| Credential file, admin token and audit log have a single hard link | A second link elsewhere would keep the secret readable under other permissions | `check`: `credentials`, `admin-token`, `audit-log` |
| The audit log is 0600, owned by Guard, and verifies | | `serve` and `check`: `audit-log` |
| The agent endpoint has a fixed, nonzero port | Port 0 picks a new port each start, so the agent's MCP URL cannot be checked against it | `check`: `agent-listen` |
| The admin API is a 0600 unix socket in the state directory (required for agent (b)) | Any local uid can connect to a loopback port; a sandboxed agent (a) cannot open any socket | `serve` and `check`: `admin-socket` |
| Agent (a): `sandbox.mode = "required"` | Landlock keeps Guard's state out of the read and write sets; seccomp refuses sockets; landlock's ptrace scoping denies `/proc/<guard>/environ` | `check`: `sandbox-mode` |
| Agent (a): `sandbox.network` is not `true` | With network, Bash could call the business API directly (it still needs the credential) | `check`: `sandbox-network` |
| Agent (a): `--crom-settings` is the file Crom actually loads: `settings.json` in `$CROM_CONFIG_HOME` (or legacy `$CODE_AGENT_CONFIG_HOME`), else `${XDG_CONFIG_HOME:-$HOME/.config}/crom` (or a legacy `code-agent` directory), owned by Guard or root up to `/`, and outside the workspace | A copy elsewhere would be checked while Crom loads another file | `check`: `crom-settings` |
| Agent (a): `--crom-workspace` is given, is not in Crom's trust store (`trusted-workspaces.json`), and neither contains nor lies inside Guard's state, config, policy directories, Guard's home, or Crom's config directory | The workspace is in the sandbox's write set; a trusted workspace's `.agent/settings.json` and `.agent/mcp.json` can loosen the sandbox and add MCP servers | `check`: `crom-workspace` |
| Agent (a): no Guard file lies under the sandbox's built-in read set (`/usr`, `/etc`, `/proc`, … `$CARGO_HOME/{bin,registry,git,config.toml,config}`, `$RUSTUP_HOME`, the directories of `cargo`, `rustc` and `rustup` on `PATH`) | Sandboxed Bash can always read those (tested) | `check`: `sandbox-paths` |
| Agent (a): every `sandbox.read_paths` entry is absolute, exists, is not a symlink, is outside the workspace, is owned by Guard or root up to `/`, and does not overlap Guard's state | A writable ancestor or a symlink could be swapped to point at Guard's state after `check` | `check`: `sandbox-read-paths`, `sandbox-paths` |
| Agent (a): `sandbox.env_allow` names none of Guard's secret variables | | `check`: `sandbox-env` |
| Agent (a): exactly one MCP server, Streamable HTTP, at Guard's agent endpoint, marked `"guard": true`; no stdio server; MCP config files outside the workspace and owned by Guard or root up to `/` | Another server could expose the same API unguarded, and stdio servers run unsandboxed (ADR 0001 §6) | `check`: `mcp-servers` |
| Agent (a): `plugins.allow_plugin_mcp = false` (or plugins disabled) | Plugins add MCP servers by default | `check`: `plugin-mcp` |
| Agent (a): an `allowed_tools` rule for `mcp__<guard>__*` | MCP tools are `External` and need an explicit allow rule; without it the agent cannot use Guard at all | `check`: `guard-allow-rule` |
| Agent (b): a uid other than Guard's and other than root | | `check`: `agent-uid` |
| Guard is never the agent's parent | Guard's environment would be inherited ([credentials.md](credentials.md)) | Operator; the reference driver starts the agent with `env -i` |

What the profile does **not** give:

- Agent (b) can still reach the business API over loopback. The control is the API's own authentication: only Guard holds the credential, so a direct call gets `401` (tested). Removing the route needs a network namespace or an owner-match firewall rule (`iptables -m owner --uid-owner B`), which `check` cannot see.
- Agent (a) on Guard's uid relies on the sandbox for Bash only. Crom itself (the host process) runs on Guard's uid unsandboxed and could read Guard's files; it is trusted not to. Use agent (b) when the agent host is not trusted.
- `check` reads the settings file and the MCP config files it is given. It does not see `--mcp-config` flags it was not told about, or plugin directories. It refuses a trusted workspace, because a trusted workspace's `.agent/settings.json` and `.agent/mcp.json` are loaded; an untrusted workspace's `.agent/settings.json` can only tighten the sandbox. Keep the workspace untrusted (do not run `crom trust` on it).
- `check` is a point-in-time check. Anything that changes between `check` and `serve` (or later) is not seen. Run it on every start, as the same user and in the same environment, for example as systemd `ExecStartPre=/usr/bin/crom-guard check …` in Guard's unit, and keep the paths it checks owned by Guard or root so they cannot change behind it.
- `--agent-user` is trusted as given. `check` verifies that user is not Guard's uid or root; it cannot see which uid the agent actually runs as.
- The Python reference client ([reference-deployment.md](reference-deployment.md)) runs unsandboxed on Guard's uid and is outside this profile; `check` fails for it. It demonstrates GP1–GP5, GP7 and GP8, which do not depend on isolation.

## Preflight: `crom-guard check`

```sh
# Agent (a): Crom sandboxed on Guard's uid
crom-guard check --config /var/lib/crom-guard/guard.toml \
  --crom-settings ~/.config/crom/settings.json --crom-workspace ~/work [--crom-mcp-config mcp.json]
# Agent (b): a client under its own user
crom-guard check --config /var/lib/crom-guard/guard.toml --agent-user crom-agent
```

Run it as Guard's user, in the environment Guard is started with (an env credential or `admin.token_env` must be set). For agent (a) that is also Crom's environment: `check` resolves Crom's settings path, trust store and the sandbox's built-in read set from `HOME`, `CROM_CONFIG_HOME`, `XDG_CONFIG_HOME`, `CARGO_HOME`, `RUSTUP_HOME` and `PATH`, as Crom does. It reads files and metadata only: it binds no listener, starts no agent, and prints no secret. Each check prints `ok: <id>: …` or `FAIL: <id>: …`. Exit codes: 0 every check passed; 1 at least one failed (the deployment is outside the profile); 2 usage error, including an unknown `--agent-user`.

`--crom-settings` requires `--crom-workspace`. At least one of `--crom-settings` and `--agent-user` is required: with neither, `agent-isolation` fails, because nothing would keep the agent away from Guard's state. Both may be given. Crom's settings are read as plain JSON (`sandbox`, `mcp`, `plugins`, `allowed_tools`), so `crom-guard-server` still depends on no other `crom-*` crate. On a platform other than Linux, `--crom-settings` fails `platform`: the Bash sandbox exists only there.

`check` is static (see the TOCTOU note above). It does not prove the kernel can enforce the sandbox; Crom does that itself under `required` (it refuses Bash when landlock ABI < 3 or seccomp is unavailable, [sandbox.md](../security/sandbox.md)). The bypass tests below prove the profile against a live Guard.

## Setting it up

```sh
sudo useradd --system --home-dir /var/lib/crom-guard --create-home --shell /usr/sbin/nologin crom-guard
sudo chmod 0700 /var/lib/crom-guard
sudo install -o crom-guard -m 0644 examples/guard-support/guard.toml /var/lib/crom-guard/
sudo install -o crom-guard -m 0755 -d /var/lib/crom-guard/policies
sudo install -o crom-guard -m 0644 examples/guard-support/policies/* /var/lib/crom-guard/policies/
# admin.token and credentials.json: owned by crom-guard, mode 0600
sudo install -o crom-guard -m 0600 admin.token credentials.json /var/lib/crom-guard/
sudo -u crom-guard crom-guard check --config /var/lib/crom-guard/guard.toml --agent-user crom-agent
sudo -u crom-guard env -i PATH=/usr/bin:/bin \
  CROM_GUARD_SUPPORT_CREDENTIALS=/var/lib/crom-guard/credentials.json \
  crom-guard-support serve --config /var/lib/crom-guard/guard.toml
```

The trusted application runs as `crom-guard` (or root) to reach `admin.sock`. For agent (a), copy `examples/guard-support/agent/crom-settings.json` into the agent user's Crom settings, export the session handle as `CROM_GUARD_HANDLE` for Crom (the settings interpolate it into the MCP `Authorization` header; the sandbox's environment allowlist keeps it out of Bash), and adjust the URL if Guard's agent port is not 8470.

## Bypass tests

`crates/crom-guard-testkit/tests/bypass.rs` (Linux). Guard and the business API run in a child process (the test binary re-executed) with a 0700 state directory, the credential in `credentials.json` and the admin token only in Guard's environment. The trusted app issues the agent a grant, and one legitimate Guard call puts a mediated entry in the ledger. Each attack then runs through Crom's own `BashTool`, once per agent configuration.

Every attack script prints `RAN` first and `DENIED` when the OS refuses a step; with the sandbox required each test asserts both, and that Crom's Bash tool ran the script (no `tool error`), so a lane cannot pass because the attack never ran. The oracle is the business API's ledger and Guard's audit log. `unmediated` counts ledger entries with no matching Guard `executed` record (the `UnmediatedEffect` probe, MILESTONE_18 §13.2).

| Test | Attack from agent Bash | `required` (network off; and on, where listed) | `off` (probe validation) |
|---|---|---|---|
| `g13_direct_api_call` (G13) | `curl` and `/dev/tcp` to the business API, no credential | no connection; with network on (negative control): `401` | `401`; reachable |
| `g14_credential_file_read` (G14) | `cat` Guard's credential file | secret absent (network off and on) | secret read |
| `g14_stolen_credential_direct_call` (G14) | read the credential file, call the API with it | `unmediated = 0` (network off and on) | `200`, `unmediated = 1` |
| `g14_guard_environ_read` (G14) | read `/proc/<guard>/environ` | admin token absent (network off and on) | token read |
| `g16_admin_socket` (G16) | holding the admin token, `curl --unix-socket` to issue itself a grant | no connection (`AF_UNIX` is refused even with network on) | `201`: grant issued |
| `g18_audit_log_append`, `g18_audit_log_truncate` (G18) | append to, or truncate, Guard's audit log | `audit verify` passes against the earlier head (network off and on) | the chain breaks |
| `separate_uid_agent` | all of the above as user `nobody` via `sudo -n`, sandbox off | every read, append and admin connect denied; direct API `401`; `unmediated = 0` | — |
| `state_under_builtin_read_root_is_refused` | Guard state copied under a writable directory of the built-in read set (e.g. `$CARGO_HOME/registry`) | sandboxed `cat` reads the credential (the hole is real), and `check` fails `sandbox-paths` | — |
| `preflight_matches_the_tested_profile` | — | the attacked deployment passes `check` with the reference Crom settings, and fails it with `mode = "off"` or `network = true` | |
| `check_models_the_sandbox_read_set`, `check_models_the_crom_settings_path` | — | `check`'s copies of the sandbox's built-in read set and of Crom's settings path equal `crom_tools::sandbox::builtin_read_paths()` and `crom_core::settings::user_settings_path()` | |

The same attacks also run as scenarios G13, G14 and G18 in `tests/guard_adversarial.rs`, and G17 as `check` refusing a second MCP server or plugin MCP servers; their results are in the `guard-scenarios/v1` report ([scenarios.md](scenarios.md)).

With the sandbox `required`, every test sees no unauthorized effect: the ledger keeps only the mediated entry (`executed = 0` for the attacks). With it `off`, every isolation-dependent attack succeeds, which shows each probe can fire. The business API's `401` to a direct call holds in every mode; it is the one control that does not depend on isolation.

```sh
cargo test -p crom-guard-testkit --test bypass -- --nocapture
CROM_REQUIRE_SANDBOX_TESTS=1 CROM_REQUIRE_SEPARATE_UID_TESTS=1 cargo test -p crom-guard-testkit --test bypass
```

A test prints `SKIP:` and passes when the sandbox cannot enforce on the host or `/usr/bin/curl` is missing (`CROM_REQUIRE_SANDBOX_TESTS=1` fails instead), or when `sudo -n -u nobody` is unavailable (`CROM_REQUIRE_SEPARATE_UID_TESTS=1` fails instead). The CI job `guard / isolation (linux)` sets both, so on CI nothing skips. Preflight unit tests (one per failing condition) are in `crates/crom-guard-server/src/check.rs`; CLI exit codes in `crates/crom-guard-server/tests/cli.rs`.

## Out of scope

- Sandboxing MCP stdio servers (ADR 0001 §6); `check` refuses any stdio server instead.
- Per-host egress allowlists: sandbox network is all-or-nothing, so `check` requires it off.
- Enforcement on platforms other than Linux. Guard-only properties (GP1–GP5, GP7, GP8) still hold there.
- Removing agent (b)'s loopback route to the business API (see above).
