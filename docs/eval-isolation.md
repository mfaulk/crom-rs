# Eval isolation backends

Verification commands (public and hidden oracles) run through an **isolation
backend**, selected with `--isolation`:

| backend  | description                                              | availability |
|----------|----------------------------------------------------------|--------------|
| `local`  | runs commands directly in the workspace directory        | always       |
| `docker` | runs commands inside a container via `docker run`        | opt-in       |
| `podman` | runs commands inside a container via `podman run`        | opt-in       |

`local` is the default and works everywhere. Container backends are best-effort:
if the engine is missing or the opt-in flag is unset, constructing the backend
fails and the run is reported with an `isolation_error` (it is not silently
downgraded).

> **Agent tool calls run on the host.** The isolation backend applies only to
> verification; a container backend does **not** sandbox what the agent runs.
> The agent's `Bash` calls are confined by the separate
> [agent sandbox](#agent-sandbox) (`--sandbox`), which is on by default for
> live providers.

## Flags

```console
$ crom eval run --suite <suite> \
    --isolation docker \
    --network false \
    --container-image rust:latest \
    --cpu-limit 2 \
    --memory-limit 2000000000
```

- `--network <bool>` — allow network during verification (default `false`).
  The `local` backend cannot enforce network isolation, so it records the
  *intent* rather than overclaiming enforcement. Container backends pass
  `--network none` when network is disabled.
- `--cpu-limit` / `--memory-limit` — enforced by container backends only
  (`--cpus`, `--memory`); recorded but not enforced for `local`.
- `--container-image` — the image to run in (default `rust:latest`).
- The workspace is bind-mounted with `:Z`, so SELinux-enforcing hosts
  (Fedora, RHEL) let the container read it; engines ignore the flag elsewhere.
- `crom eval validate-task` / `validate-suite` take the same `--isolation` and
  `--container-image` flags to verify tasks inside a container.
- `--sandbox required|off` — OS sandbox for the agent's `Bash` calls (see
  [Agent sandbox](#agent-sandbox)). Default `required` for live providers,
  `off` for `fake`/`golden`.

## Agent sandbox

`--sandbox required` runs every agent `Bash` call under the Linux OS sandbox
([docs/security/sandbox.md](security/sandbox.md)): writes only to the agent
workspace and a private tmp dir, no reads of `$HOME`, the task source, or the
runs dir, no network unless `--network true` or the task sets
`requires_network`, provider keys removed from the environment, and every
descendant killed when the call ends.

- Fails closed: if the host cannot enforce the sandbox (non-Linux, landlock
  ABI < 3, root), the run stops before any task starts with
  `agent sandbox is required but cannot be enforced on this host: …`. Pass
  `--sandbox off` to run unsandboxed.
- Before the first task the CLI measures the mean per-call overhead and prints
  `agent sandbox: required (landlock ABI N), overhead X ms/call`.
- Recorded in `run.json` as `agent_sandbox: { mode, overhead_ms }` and in each
  result as `repro.security.agent_sandbox` (`landlock` or `off`) and
  `repro.security.agent_sandbox_overhead_ms`.

```console
$ crom eval run --suite security --provider golden --sandbox required
```

## Opt-in

Container backends are gated behind environment variables, mirroring the
live-provider lane:

```console
$ CROM_EVAL_DOCKER=1 crom eval run --suite <suite> --isolation docker
$ CROM_EVAL_PODMAN=1 crom eval run --suite <suite> --isolation podman
```

Without the opt-in variable (or without the engine on `PATH`), the backend is
unavailable. Ordinary CI never sets these, so it always runs the `local` lane.

## Environment metadata

Every run records `environment.json` per task:

```json
{
  "os": "macos",
  "arch": "aarch64",
  "rustc": "rustc 1.88.0 (...)",
  "cargo": "cargo 1.88.0 (...)",
  "isolation": "local",
  "container_engine": null,
  "container_image": null,
  "network": false
}
```

For container backends, `container_engine` records the engine version and
`container_image` records the image. Hidden tests remain unavailable until after
the agent completes regardless of backend.

## Leakage

Grader material must not be reachable from the agent workspace (M17.2, #286).
Task-authoring rules are in
[eval-schema.md](eval-schema.md#where-grader-material-lives-m172-286).

| Layer | What it guarantees | Enforced by |
|---|---|---|
| Workspace root | The agent's cwd is `$TMPDIR/crom-eval-ws/ws-<random>/` (mode 0700) with only the public fixture + git baseline. `task.toml`, `prompt.md`, `normalized-task.json`, `hidden/`, `oracle.patch`, and earlier results are not inside it or beside it, so `cat ../task.toml` finds nothing. | runner (`crom_eval::workspace`) |
| Overlap guard | A workspace root inside (or containing) the runs dir or tasks dir is refused as a harness error. | runner |
| Hidden overlay | `[hidden].fixture` is copied only into the verification copy (a temp dir, archived to `runs/<id>/tasks/<task>/workspace/` afterwards) after the agent exits. | runner |
| Path tools | `Read`/`Write`/`Edit`/`Glob`/`Grep` reject paths outside the workspace, including absolute paths to the task source or run dir. | `crom-tools` path sandbox |
| Agent Bash, absolute paths | With `--sandbox required` (default for live providers), Bash cannot read the task source dir or the runs dir by absolute path. With `--sandbox off` it can. | [agent sandbox](#agent-sandbox) (#253) |

`TMPDIR` controls the workspace root. The `leak-canary` task (suite
`security`, `kind = "security"`) asks the agent to copy grader material into
`FOUND.txt`; it resolves only if no grader token leaks. Run it offline with:

```console
$ crom eval run --suite security --provider golden
```

Coverage lives in `crates/crom-eval/tests/leakage.rs`; the Bash
absolute-path test runs with `sandbox = required` and skips (or fails under
`CROM_REQUIRE_SANDBOX_TESTS=1`) on hosts without the sandbox.

## Known limitations

- Container backends confine verification only; agent tool calls run on the
  host under every backend. Agent `Bash` is OS-sandboxed only with
  `--sandbox required` (Linux only); with `--sandbox off` run live evals that
  enable Bash only against trusted tasks, or run the whole harness inside an
  externally sandboxed environment (container/VM).
- On timeout the harness kills the wrapping `sh`; a long-running child (e.g.
  cargo) may briefly outlive the timeout before exiting.
