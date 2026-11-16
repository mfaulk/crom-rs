# OS sandbox for agent tool processes (Linux)

The built-in `Bash` tool can run each command inside an OS sandbox
(M17.3, #253). The design is
[ADR 0001](decisions/0001-sandbox-backend.md); the properties it enforces
(P1–P7) are defined in the [threat model](threat-model.md) §7.

Backend: **landlock + seccomp + rlimits + a subreaper supervisor**
(`crates/crom-tools/src/sandbox/linux.rs`). Linux only. Every other platform,
and any Linux host that cannot fully enforce the configuration, **refuses**
the command under `mode = required` before spawning anything.

## What a sandboxed command gets

| Property | Enforcement |
|---|---|
| P1 writes | Read-write only to the workspace, a private `TMPDIR` (`crom-sbx-*`, removed after the call), and `/dev/null`. Requires landlock ABI ≥ 3 (truncate is restricted). |
| P2 reads | Read-only: `/usr`, `/bin`, `/sbin`, `/lib*`, `/etc`, `/proc`, `/sys/devices/system/cpu`, `/dev/{zero,random,urandom}`, `/nix/store`, `$CARGO_HOME/{bin,registry,git,config.toml,config}`, `$RUSTUP_HOME`, toolchain bin dirs, plus `sandbox.read_paths`. Nothing else (no `$HOME`, no `~/.ssh`, no task source, no eval runs dir). |
| P3 network | seccomp refuses `socket(2)` for every family by default, and for `AF_UNIX` when network is granted, so host loopback services and unix sockets (D-Bus, docker) stay unreachable. `io_uring` is refused. Granting network is all-or-nothing; there is no per-host allowlist. |
| P4 environment | The child environment is cleared to an allowlist (`PATH`, locale vars, `TERM`, `TZ`, `USER`, `LOGNAME`, `RUSTUP_TOOLCHAIN`, `CARGO_TERM_COLOR`, `NO_COLOR`, `CLICOLOR`, `RUST_BACKTRACE`, `SOURCE_DATE_EPOCH`) plus `sandbox.env_allow`. `HOME`, `TMPDIR`, `TMP`, and `TEMP` point at the private tmp dir; `CARGO_HOME`/`RUSTUP_HOME` keep the host paths (read-only). Provider keys never reach the child. |
| P5 process tree | A supervisor process is made a child subreaper; on normal exit, timeout, cancel, or drop of the tool future it SIGKILLs every descendant, including `setsid`'d ones. It also dies with the host process. |
| P6 resources | `RLIMIT_NPROC` (the UID's task count at spawn + `max_processes`), `RLIMIT_DATA`, `RLIMIT_FSIZE`, `RLIMIT_CPU`, `RLIMIT_CORE = 0`. |
| P7 privilege | `NO_NEW_PRIVS`; refuses to run as root. With landlock ABI ≥ 6 the command cannot signal processes outside its domain. |

Each Bash result records the outcome in `metadata.sandbox`:
`{ "mode": "required", "enforced": true, "backend": "landlock", "landlock_abi": N, "setup_ms": … }`,
or `{ "mode": "off", "enforced": false }`.

## Configuration

Settings section `sandbox` in the user settings file
(`${CROM_CONFIG_HOME:-$HOME/.config/crom}/settings.json`):

| Key | Default | Meaning |
|---|---|---|
| `mode` | `"off"` | `"required"` sandboxes every Bash call and fails closed. |
| `network` | `false` | Allow IP sockets (all-or-nothing). |
| `read_paths` | `[]` | Extra read-only paths. |
| `env_allow` | `[]` | Extra environment variable names passed to the child. |
| `max_processes` | `1024` | Tasks above the UID's count at spawn. |
| `max_memory_bytes` | `8 GiB` | `RLIMIT_DATA` per process. |
| `max_file_size_bytes` | `4 GiB` | `RLIMIT_FSIZE`. |
| `max_cpu_seconds` | `600` | `RLIMIT_CPU` per process. |
| `min_landlock_abi` | `3` | Minimum ABI accepted; values below 3 are raised to 3. |

Repository-controlled files (`<cwd>/.agent/settings.json` and
`<cwd>/.agent/settings.local.json`) may only **tighten** the sandbox:
`mode = "required"`, `network = false`, lower limits, and a higher minimum ABI
apply; `mode = "off"`, `network = true`, extra read paths, and extra env names
are ignored. Plugin settings defaults cannot change the sandbox.

SDK: `ToolPolicy { sandbox: crom_core::SandboxConfig::required(), .. }`.

Eval: `crom eval run --sandbox required|off`; the default is `required` for
live providers and `off` for `fake`/`golden`. See
[eval isolation](../eval-isolation.md#agent-sandbox).

## Overhead

`crom eval run` measures the mean extra wall time of a sandboxed `true` over an
unsandboxed one (20 warm calls) and records it as
`repro.security.agent_sandbox_overhead_ms` and `run.json`
`agent_sandbox.overhead_ms`. Most of it is the `/proc` scan that counts the
UID's tasks for the `RLIMIT_NPROC` budget, so it grows with the number of
processes on the host (about 30 ms with ~1,800 processes; 2–3 ms on a CI
runner per ADR 0001 §3.4).

## Known gaps

- `RLIMIT_NPROC` is per UID, not per call: the budget is the UID's task count
  at spawn plus `max_processes`, so it loosens when other processes of the same
  user exit. ADR 0001 §5 proposed a per-call cgroup scope instead.
- The whole workspace is writable, including `<workspace>/.agent/`
  (threat model G9). Landlock rules only grant access, so a sub-directory of a
  writable tree cannot be excluded.
- No bubblewrap fallback: hosts with landlock ABI < 3 fail closed.
- MCP stdio servers are not sandboxed yet (ADR 0001 §6 makes this opt-in);
  `crom-core` depends on `crom-mcp`, so the spawn needs an injected hook.

## Testing the sandbox safely

The P6 containment tests (`process_count_is_bounded`, `fork_bomb_is_contained`,
`memory_limit_is_enforced`) will exhaust the host if the sandbox is broken: on
2026-11-14 a mutation check that removed the sandbox ran the fork bomb directly
on a workstation and it ran out of memory (#253). They are therefore:

1. `#[ignore]`d, so `cargo test` and `cargo nextest` never run them;
2. refused unless the test process sits in a cgroup with `pids.max` ≤ 4096 and
   `memory.max` ≤ 16 GiB, a limit that does not depend on the code under test;
3. refused unless a probe through the same Bash path shows seccomp mode 2,
   `NoNewPrivs: 1`, and `RLIMIT_NPROC`/`RLIMIT_DATA` lower than the host's.

Run all sandbox suites with:

```console
$ scripts/sandbox-tests.sh            # TasksMax=1024, MemoryMax=8G scope
$ SANDBOX_TESTS_TASKS_MAX=256 SANDBOX_TESTS_MEMORY_MAX=2G scripts/sandbox-tests.sh fork_bomb
```

The script compiles outside the scope, then runs the test binaries in a
`systemd-run --user --scope` with `TasksMax`, `MemoryMax`, and
`MemorySwapMax=0`. It refuses to run without a systemd user session.
`CROM_REQUIRE_SANDBOX_TESTS=1` turns "sandbox unavailable" skips into failures
(set in the `security / OS sandbox (linux)` CI job).

When checking that the tests catch a broken sandbox (mutation testing), run
only through this script. Never bypass the guards or run the containment tests
outside the scope.
