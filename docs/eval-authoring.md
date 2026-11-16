# Eval task authoring guide

How to write a task for `crates/crom-eval/eval/tasks/`. The manifest field
reference is [eval-schema.md](eval-schema.md); the general harness is
[eval.md](eval.md).

## 1. Pick a kind and level

| You are testing... | `kind` | `level` |
|---|---|---|
| Runtime plumbing, a scripted `golden` replay, or `fake`-provider behavior | `harness` | `L0` |
| Whether the agent can solve a coding problem | `capability` | `L1`..`L5` |
| Whether policy/sandbox stops an unsafe action | `security` | as appropriate |

A task that passes by construction under an offline provider is `harness`,
even if its fixture is a Rust crate. Only `capability` tasks belong in
capability pass rates.

Levels: `L1` single-file coding, `L2` multi-step coding and repair (typically
two or more files in the reference), `L3` real-repository engineering, `L4`
genuine engineering issues, `L5` external benchmark comparison.

## 2. Write the manifest

Start from `crates/crom-eval/eval/tasks/rust-hidden-sum-bug/task.toml`:

```toml
schema_version = 3
task_version = "1.0.0"
kind = "capability"
level = "L1"
network = "none"
reference = "oracle_patch"
oracle_patch = "oracle.patch"     # must live outside the public fixture dir

[provenance]
author = "..."
source = "original"
license = "Apache-2.0 OR MIT"
created = "YYYY-MM-DD"
```

Bump `task_version` whenever the prompt, fixture, hidden tests, or reference
change, so results are only compared across identical task content.

## 3. Capability-task requirements

`crom eval validate-task <id>` enforces these; each is an error, not a warning:

1. **Hidden verification.** A `[hidden]` section with at least one check,
   usually a hidden test overlaid from a `hidden/` directory that sits beside
   (not inside) `repo/`.
2. **A reference.** Either `reference = "oracle_patch"` with a patch that
   applies to the fixture and makes public **and** hidden verification pass,
   or, for a task whose correct answer is to change nothing,
   `expected_outcome = "no_change"` with `reference = "validated_oracle"`.
3. **A meaningful base.** The unmodified fixture must fail public + hidden
   verification (`change`), or pass it (`no_change`).

Also keep hidden test names and the reference out of the prompt; the canary
checks flag both.

## 4. Validate

```console
$ crom eval validate-task <task-id>
$ crom eval validate-suite <suite> --repeat 5   # flake detection
$ CROM_EVAL_PODMAN=1 crom eval validate-suite <suite> --isolation podman \
    --container-image docker.io/library/rust:1-slim   # verification in a container, network none
```

Both must report 0 errors before the task is merged. `--isolation
docker|podman` runs every verification command in the container backend
(opt-in via `CROM_EVAL_DOCKER=1` / `CROM_EVAL_PODMAN=1`); an unavailable
backend is a validation error, never a silent fallback to the host.
