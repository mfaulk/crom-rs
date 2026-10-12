# Eval adapters

An **adapter** turns a *source* of tasks into the harness's common `EvalTask`
representation, so internal tasks, one-off directories, and external
benchmark-style slices all flow through the same runner, oracle, scoring, and
artifact pipeline. List them with:

```console
$ crom eval adapters
```

| id          | source                                          | maturity |
|-------------|-------------------------------------------------|----------|
| `internal`  | bundled `eval/tasks/<id>/task.toml` manifests   | full     |
| `directory` | a single local task directory                   | full     |
| `swebench`  | a local SWE-bench-style instance dir            | MVP      |
| `aider`     | a local Aider/Exercism-style Rust exercise      | MVP      |
| `terminal`  | a local Terminal-Bench-style task directory     | MVP      |

The external adapters are intentionally MVP-quality and operate on **local
fixtures only**. They prove the adapter boundary, not leaderboard-grade
execution. None of them clone arbitrary GitHub repos.

## internal

```console
$ crom eval run --suite rust-mini --adapter internal
$ crom eval run rust-hidden-sum-bug --adapter internal
```

Discovers `<tasks-dir>/<id>/task.toml`. This is the default adapter.

## directory

```console
$ crom eval run ./path/to/task --adapter directory
```

Accepts either a directory containing a `task.toml` (loaded directly) or a bare
directory containing `instruction.txt`/`prompt.txt` plus an optional `test.sh`
(the public verification). A directory of such directories is also accepted.

## swebench (MVP)

Layout:

```text
<instance>/
  instance.json        # { instance_id, repo, problem_statement, test_command, test_patch? }
  repo/                # the fixture repository
  test.patch           # optional hidden test patch (applied after completion)
```

The `problem_statement` becomes the prompt; the repo is materialized as the
fixture; the hidden verification applies `test_patch` (if present) and runs
`test_command`. Only local fixtures are supported.

```console
$ crom eval run ./instances/example__repo-1 --adapter swebench
```

## aider (MVP)

A small Rust exercise directory with an `instruction.txt` and a Cargo crate.
Public verification is `cargo test`. Supports `--attempts 2` for a single
retry with the failing test output fed back to the agent.

```console
$ crom eval run ./exercises/reverse-string --adapter aider --attempts 2
```

## terminal (MVP)

A directory with `instruction.txt` and `test.sh` (plus optional `Dockerfile`/
`image.txt`/`oracle.patch`). Runs in **local mode**: `test.sh` is the oracle.
Container execution is recorded but not required.

```console
$ crom eval run ./tasks/grep-task --adapter terminal
```

## Adding a new adapter

Implement `crom_eval::adapter::EvalAdapter` (`id`, `describe`,
`discover_tasks`) and register it in `AdapterRegistry::default()`. The adapter's
only job is to produce `EvalTask` values (including any hidden verification); the
runner executes them uniformly. See `crates/crom-eval/src/external.rs` for examples.
