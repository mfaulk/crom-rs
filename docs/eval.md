# Coding-Agent Capability Evaluation Harness (M14 + M15)

The eval harness runs the agent against controlled coding tasks in isolated
workspaces, scores the result, and emits debuggable artifacts. It answers a
question unit tests cannot: **given a realistic repository task, can the agent
inspect code, plan, edit files, run checks, recover from failures, and produce
a correct patch?**

M15 extends the M14 harness into an adapter-based, schema-versioned,
isolation-aware, flake-aware, behavior-aware, regression-tracking platform.
Companion docs:

- [eval-schema.md](eval-schema.md) — the task manifest schema (v1 + v2).
- [eval-adapters.md](eval-adapters.md) — adapters (internal, directory, and the
  external benchmark MVPs).
- [eval-isolation.md](eval-isolation.md) — isolation backends and environment
  metadata.

## What's new in M15

- **Schema v2** with capability labels, behavior rules, hidden verification,
  quarantine, visibility, prompt variants, and required/forbidden path policy.
  M14 (v1) manifests still parse and run unchanged.
- **Adapters** (`eval adapters`): `internal`, `directory`, plus MVP `swebench`,
  `aider`, and `terminal` adapters that run local benchmark-style slices through
  the same pipeline.
- **Isolation backends** (`--isolation local|docker|podman`) with recorded
  environment metadata; `local` is the default and works everywhere.
- **Validation** (`eval validate-task`, `eval validate-suite [--repeat N]`) with
  flake detection and canary checks (leak detection).
- **Hidden verification**: checks (and an optional fixture overlay) applied only
  *after* the agent completes, so the agent cannot overfit to them.
- **Behavior metrics** derived from the event/patch timeline (read-before-write,
  test-after-edit, deleted tests, blind edits, lockfile changes, …).
- **Primary `resolved` score** plus a structured **failure taxonomy**.
- **Multi-run statistics** (`--repetitions k`): pass@1 / pass@k, per-task
  variance, failure-class distribution.
- **Regression index** (`target/code-agent-eval/index.jsonl`) feeding
  `eval history`, `eval trend`, `eval regressions`, `eval task-history`.
- **Triage** (`eval triage`, `eval show`, `eval open`).

## M15 command reference

```console
$ code-agent eval adapters
$ code-agent eval run <suite|task> --adapter internal
$ code-agent eval run <path> --adapter directory
$ code-agent eval run <suite> --isolation local --network false
$ code-agent eval run <suite> --repetitions 5
$ code-agent eval run <suite> --prompt-variant concise
$ code-agent eval run <path> --adapter aider --attempts 2
$ code-agent eval validate-task <id-or-path>
$ code-agent eval validate-suite <suite> --repeat 5
$ code-agent eval history
$ code-agent eval trend <suite>
$ code-agent eval regressions --baseline <run-or-sha> --candidate <run-or-sha>
$ code-agent eval task-history <task-id>
$ code-agent eval triage <run-id>
$ code-agent eval show <run-id> <task-id>
$ code-agent eval open <run-id> <task-id>
```

A run that uses repetitions writes per-repetition artifacts under
`<run-id>/rep-<n>/tasks/<task-id>/` and aggregate stats to
`<run-id>/repetitions.json`.

The harness lives in the `code-agent-eval` crate (`crates/eval`) and is exposed
through the `code-agent eval` CLI. It drives the agent exclusively through the
headless [SDK runtime](../crates/code-agent-sdk) from M12 — it never shells out
to the CLI and never builds a separate mini-agent. Behavior is judged from
structured SDK events, not by scraping model prose.

## What it is for

- Catching regressions in the agent's coding ability across releases.
- Exercising tool execution, event capture, sessions, permissions, planning,
  and command execution end-to-end against real (tiny) repositories.
- Producing a self-contained artifact bundle for every run so a failed task can
  be debugged after the fact.

It is intentionally pragmatic: a small, fast, inspectable first suite rather
than a sprawling benchmark.

## Quick start

List tasks:

```console
$ code-agent eval list
$ code-agent eval list --suite smoke
$ code-agent eval list --tag rust
```

Run the deterministic smoke suite offline (no model, no network):

```console
$ code-agent eval run --suite smoke --provider fake
```

Run a single task with the scripted golden provider:

```console
$ code-agent eval run golden-edit-recover --provider golden
```

Run the realistic Rust suite against a live provider (opt-in):

```console
$ CODE_AGENT_EVAL_LIVE=1 ANTHROPIC_API_KEY=sk-... \
    code-agent eval run --suite rust-small --provider anthropic
```

Report and compare prior runs:

```console
$ code-agent eval report <run-id>
$ code-agent eval compare <baseline-run-id> <candidate-run-id>
```

## Provider modes

The `--provider` flag selects how the agent is driven for the whole run:

| mode        | description |
|-------------|-------------|
| `fake`      | A trivial echo provider. Proves the harness plumbing (workspace setup, event capture, oracles, scoring, artifacts) without proving model intelligence. Offline and deterministic. |
| `golden`    | A scripted provider that replays a canned sequence of tool calls for a specific task (read → edit → run check → observe failure → edit → pass). Catches regressions in tool schemas, event handling, sessions, and command execution. |
| `anthropic` (or any other name) | A real model provider. **Opt-in only**: requires `CODE_AGENT_EVAL_LIVE=1` and provider credentials (`ANTHROPIC_API_KEY`). If the opt-in flag or credentials are missing, affected tasks are reported as **skipped**, never failed. Never runs in ordinary CI. |

Set `CODE_AGENT_EVAL_MODEL` to override the live model (default
`claude-sonnet-4-6`).

## Suites

Tasks declare the suites they belong to in their manifest (`suites = [...]`):

- `smoke` — tiny tasks whose oracles pass deterministically under the **fake**
  provider. This is the CI lane.
- `golden` — tasks with a scripted golden provider sequence.
- `rust-small` — realistic Rust bug-fix / feature / refactor tasks intended for
  a live provider.
- `safety` — tasks exercising permission and destructive-command behavior.
- `planning` — tasks that require explicit plan-then-act behavior.

## Adding a new task

1. Create a directory under `crates/eval/eval/tasks/<task-id>/`.
2. Add a `task.toml` manifest (see the reference below).
3. Add a `repo/` fixture directory containing the starting project. Keep it
   tiny. If it is a Rust crate, put an empty `[workspace]` table at the top of
   its `Cargo.toml` so the parent workspace never absorbs the fixture.
4. (Optional) Add the task to a suite and/or register a golden script in
   `crates/eval/src/providers.rs` keyed by the task id.

Example layout:

```text
crates/eval/eval/tasks/rust-failing-unit-test/
  task.toml
  repo/
    Cargo.toml
    src/lib.rs
```

## Task manifest reference

```toml
id = "rust-cli-arg-parsing-bug"          # unique id (also the directory name)
title = "Fix CLI arg parsing bug"
description = "optional long description"
tags = ["rust", "bugfix", "small"]        # free-form; "small" tunes patch-quality
suites = ["rust-small"]                   # suites this task joins
mode = "modify_workspace"                 # or "plan_then_act"
golden_script = "rust-cli-arg-parsing-bug" # optional; defaults to id

prompt = """
The CLI fails when --output is passed after the input path. Fix the bug.
"""

max_turns = 20                            # turn budget (default 20)
timeout_secs = 180                        # per-task wall clock (default 180)
allowed_tools = ["Read", "Edit", "Bash"]  # empty = all built-in tools

[fixture]
kind = "copy_dir"                         # or "empty"
path = "repo"                             # relative to the task directory

[oracle]
kind = "command"                          # "command" | "rust" | "none"
commands = ["cargo test --all-targets"]   # for kind = "command"
# for kind = "rust":
# fmt = true
# test = true
# clippy = true
# doc_tests = false
command_timeout_secs = 120                # optional per-command timeout

[[oracle.assertions]]
kind = "file_contains"                    # file_contains | file_not_contains | file_exists | file_missing
path = "src/lib.rs"
text = "pub fn parse_args"

[[oracle.behavior]]
kind = "must_use_tool"                    # see oracle reference below
tool = "Read"
```

## Oracle reference

Oracles decide whether the agent succeeded. They are evaluated against the
post-run workspace and the captured event stream. The primary `kind` and any
assertions/behavior checks all contribute independent pass/fail checks.

### Command oracle

Runs shell commands in the workspace; each must exit zero. stdout/stderr/exit
code are captured to `command-output/`.

```toml
[oracle]
kind = "command"
commands = ["cargo test --all-targets", "cargo clippy --all-targets -- -D warnings"]
```

### Rust oracle

A convenience that expands to common cargo quality gates:

```toml
[oracle]
kind = "rust"
fmt = true        # cargo fmt --all --check
test = true       # cargo test --all-targets
clippy = true     # cargo clippy --all-targets -- -D warnings
doc_tests = false # cargo test --doc
```

### File-assertion oracle

```toml
[[oracle.assertions]]
kind = "file_contains"      # file exists and contains text
path = "src/lib.rs"
text = "pub fn parse_args"

[[oracle.assertions]]
kind = "file_not_contains"  # file exists and does NOT contain text
path = "src/main.rs"
text = "unwrap()"

# also: file_exists { path }, file_missing { path }
```

### Behavior oracle

Evaluated from structured events, never from prose:

```toml
[[oracle.behavior]]
kind = "must_emit_plan"                       # a PlanCreated event was emitted

[[oracle.behavior]]
kind = "must_use_tool"
tool = "Read"

[[oracle.behavior]]
kind = "must_not_use_tool"
tool = "Bash"

[[oracle.behavior]]
kind = "must_run_command"
command_contains = "cargo test"

[[oracle.behavior]]
kind = "must_not_run_command"
command_contains = "rm -rf"
```

## Scoring

Each task gets a structured score in `[0, 1]`, serialised to `score.json`:

| dimension        | weight | summary |
|------------------|--------|---------|
| correctness      | 60%    | fraction of oracle checks that passed; zero if the run errored with no progress |
| patch quality    | 15%    | penalises test deletions, broad rewrites for "small" tasks, and gutting logic |
| process quality  | 15%    | rewards reading files, running checks, planning when required, and recovering from a failed command; penalises tight command loops |
| efficiency       | 10%    | rewards completing within the turn/time budget; penalises timeouts and turn exhaustion |

`total` is the weighted sum. Scoring is deliberately simple; the score notes in
`report.md` explain how each number was derived.

## Artifact layout

Every run writes a self-contained bundle:

```text
target/code-agent-eval/runs/<run-id>/
  summary.json
  summary.md
  tasks/<task-id>/
    task.toml          # copy of the manifest
    prompt.md          # the prompt sent to the agent
    transcript.jsonl   # conversation-relevant events
    events.jsonl       # every SDK event
    final.patch        # unified diff vs the fixture baseline
    final.diff         # diff --stat summary
    final_summary.md   # one-page outcome
    command-output/    # NNN-<cmd>.stdout / .stderr / .status per oracle command
    score.json
    report.md
    workspace/         # the isolated workspace (preserved on success and failure)
```

Each task runs in its own git-initialised workspace; the final patch is the diff
against the baseline commit.

## CI behavior

The `eval` job in `.github/workflows/ci.yml` runs a fast, deterministic,
offline lane on every push/PR:

```console
cargo test -p code-agent-eval
code-agent eval run --suite smoke --provider fake
```

It uploads the artifact bundle when the smoke suite regresses. Live-provider
evals never run here.

Live evals are available manually via the `Eval (live provider)` workflow
(`.github/workflows/eval-live.yml`), triggered from the Actions tab. It sets
`CODE_AGENT_EVAL_LIVE=1` and uses the `ANTHROPIC_API_KEY` repository secret;
without the secret, live tasks skip cleanly.

## Known limitations

- The fake/golden providers prove the harness and tool plumbing, not model
  intelligence. Only live runs measure real capability.
- Golden scripts are fixed sequences keyed by task id; they replay the same
  tool calls regardless of the (deterministic) workspace.
- Scoring heuristics (patch/process/efficiency) are pragmatic, not calibrated;
  treat sub-scores as signal, not ground truth.
- Oracle commands run in the host environment, so Rust tasks require a working
  cargo toolchain (and network for fixtures with external dependencies).
- The workspace is isolated by directory (cwd), **not** by OS sandboxing. Under
  a live provider a task that enables Bash can in principle run commands that
  reach outside the fixture directory. Run live evals with Bash only against
  trusted tasks, or in an externally sandboxed environment (container/VM).
- Patch quality cannot tell "relevant" from "irrelevant" files perfectly; it
  uses tags (e.g. `small`) and simple churn/deletion heuristics.
- The harness avoids exact-diff matching by design: any patch that satisfies the
  command/file/behavior oracles is accepted.

## Repair loop (M16)

`code-agent eval run <suite> --repair enabled` turns on the bounded repair loop:
failed validations are parsed into structured diagnostics, a targeted repair
brief drives bounded retries, repeated identical failures are detected and
stopped, and repair metrics + artifacts (`repair.json`, `repair.md`,
`diagnostics.jsonl`, `validation-attempts.jsonl`) are recorded. The
`repair-smoke` / `repair-mini` suites exercise it offline with the golden
provider. See [repair-loop.md](repair-loop.md) and [diagnostics.md](diagnostics.md).
