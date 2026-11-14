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

> **Agent tool calls always run on the host.** The isolation backend applies
> only to verification. The agent's own tool calls (`Bash`, `Edit`, `Write`,
> …) execute directly on the host with the workspace directory as their
> working directory, under every backend including `docker` and `podman`. A
> container backend does **not** sandbox what the agent runs.

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

## Known limitations

- No backend sandboxes the agent: tool calls run on the host under `local`,
  `docker`, and `podman` alike (container backends confine verification only).
  The workspace is isolated by working directory, not by OS sandboxing. Run
  live evals that enable Bash only against trusted tasks, or run the whole
  harness inside an externally sandboxed environment (container/VM).
- On timeout the harness kills the wrapping `sh`; a long-running child (e.g.
  cargo) may briefly outlive the timeout before exiting.
