# Coding-Agent Capability Evaluation Harness (M14 + M15)

The eval harness runs the agent against controlled coding tasks in isolated
workspaces, scores the result, and emits debuggable artifacts. It answers a
question unit tests cannot: **given a realistic repository task, can the agent
inspect code, plan, edit files, run checks, recover from failures, and produce
a correct patch?**

M15 extends the M14 harness into an adapter-based, schema-versioned,
isolation-aware, flake-aware, behavior-aware, regression-tracking platform.
Companion docs:

- [eval-schema.md](eval-schema.md) — the task manifest schema (v1, v2, v3).
- [eval-authoring.md](eval-authoring.md) — task authoring guide (kinds, levels,
  capability-task requirements).
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
- **Isolation backends** (`--isolation local|docker|podman`) for verification
  commands, with recorded environment metadata; `local` is the default and
  works everywhere. Agent tool calls always run on the host.
- **Validation** (`eval validate-task`, `eval validate-suite [--repeat N]`) with
  flake detection and canary checks (leak detection).
- **Hidden verification**: checks (and an optional fixture overlay) applied only
  *after* the agent completes, so the agent cannot overfit to them.
- **Behavior metrics** derived from the event/patch timeline (read-before-write,
  test-after-edit, deleted tests, blind edits, lockfile changes, …).
- **Primary `resolved` score** plus a structured **failure taxonomy**.
- **Multi-run statistics** (`--repetitions k`): pass@1 / pass@k, per-task
  variance, failure-class distribution.
- **Regression index** (`target/crom-eval/index.jsonl`) feeding
  `eval history`, `eval trend`, `eval regressions`, `eval task-history`.
- **Triage** (`eval triage`, `eval show`, `eval open`).

## M15 command reference

```console
$ crom eval adapters
$ crom eval run --suite <suite> --adapter internal
$ crom eval run <task-id> --adapter internal
$ crom eval run <path> --adapter directory
$ crom eval run --suite <suite> --isolation local --network false
$ crom eval run --suite <suite> --repetitions 5
$ crom eval run --suite <suite> --repetitions 5 --seed 42
$ crom eval run --suite <suite> --prompt-variant concise
$ crom eval run <path> --adapter aider --attempts 2
$ crom eval validate-task <id-or-path>
$ crom eval validate-suite <suite> --repeat 5
$ crom eval history
$ crom eval trend <suite>
$ crom eval regressions --baseline <run-or-sha> --candidate <run-or-sha>
$ crom eval task-history <task-id>
$ crom eval triage <run-id>
$ crom eval show <run-id> <task-id>
$ crom eval open <run-id> <task-id>
```

`eval run` takes a suite only via `--suite`; a positional argument is a task
id (internal adapter) or a path (other adapters). With neither, it runs
`smoke`.

A run that uses repetitions writes per-repetition artifacts under
`<run-id>/rep-<n>/tasks/<task-id>/` and aggregate stats to
`<run-id>/repetitions.json`.

The harness lives in the `crom-eval` crate (`crates/crom-eval`) and is exposed
through the `crom eval` CLI. It drives the agent exclusively through the
headless [SDK runtime](../crates/crom-sdk) from M12 — it never shells out
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
$ crom eval list
$ crom eval list --suite smoke
$ crom eval list --tag rust
```

Run the deterministic smoke suite offline (no model, no network):

```console
$ crom eval run --suite smoke --provider fake
```

Run a single task with the scripted golden provider:

```console
$ crom eval run golden-edit-recover --provider golden
```

Run the realistic Rust suite against a live provider (opt-in):

```console
$ CROM_EVAL_LIVE=1 ANTHROPIC_API_KEY=sk-... \
    crom eval run --suite rust-small --provider anthropic
```

Report and compare prior runs:

```console
$ crom eval report <run-id>
$ crom eval compare <baseline-run-id> <candidate-run-id>
```

## Provider modes

The `--provider` flag selects how the agent is driven for the whole run:

| mode        | description |
|-------------|-------------|
| `fake`      | A trivial echo provider. Proves the harness plumbing (workspace setup, event capture, oracles, scoring, artifacts) without proving model intelligence. Offline and deterministic. |
| `golden`    | A scripted provider that replays a canned sequence of tool calls for a specific task (read → edit → run check → observe failure → edit → pass). Catches regressions in tool schemas, event handling, sessions, and command execution. |
| any provider profile id | A real model provider resolved from a provider profile (`anthropic`, `openai`, `deepseek`, `gemini`, `local`, or an id defined under `providers` in user/project settings; see [providers.md](providers.md)). Any other value is rejected with `unknown provider`. **Opt-in only**: requires `CROM_EVAL_LIVE=1` and the profile's API-key env var (e.g. `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`). If the opt-in flag or credentials are missing, affected tasks are reported as **skipped**, never failed. Never runs in ordinary CI. |

Set `CROM_EVAL_MODEL` to override the live model (default: the profile's
`default_model`, e.g. `claude-sonnet-4-6` for `anthropic`, `deepseek-chat`
for `deepseek`).

## Suites

Tasks declare the suites they belong to in their manifest (`suites = [...]`):

- `smoke` — tiny tasks whose oracles pass deterministically under the **fake**
  provider. This is the CI lane.
- `golden` — tasks with a scripted golden provider sequence.
- `rust-small` — realistic Rust bug-fix / feature / refactor tasks intended for
  a live provider.
- `rust-mini` — the M15 Rust slice: the `rust-small` tasks except
  `golden-rust-recover`, plus `rust-hidden-sum-bug` (hidden verification).
  Intended for a live provider.
- `safety` — tasks exercising permission and destructive-command behavior.
  Passes deterministically under the **fake** provider.
- `planning` — tasks that require explicit plan-then-act behavior.
- `security` — `kind = "security"` scenarios. `leak-canary` checks that grader
  material is unreachable from the agent workspace; it passes under the
  **golden** provider (see [eval-isolation.md](eval-isolation.md#leakage)).
- `repair-smoke` / `repair-mini` — M16 repair-loop tasks scripted for the
  **golden** provider. `repair-grep-trap` is a negative task: it is expected
  *not* to resolve (see [Repair loop](#repair-loop-m16)).
- `private` — tasks with `visibility = "private"`. Excluded from suite
  selection and `eval list` unless `--include-private` is passed.
- `l1` — the L1 capability corpus (M17.2, #287): single-file Rust tasks, all
  `kind = "capability"`, `level = "L1"`, `network = "none"`, each with hidden
  verification and a validated reference. Intended for a live provider; CI
  validates it model-free with `crom eval validate-suite l1`.
- `l2` — the M17 L2 capability corpus (multi-step coding and repair). Every
  task is `kind = "capability"`, `level = "L2"`, with hidden tests and a
  reference. Intended for a live provider; see [L2 corpus](#l2-corpus-m172).
- `l3` — M17.2 L3 capability corpus (real-repository engineering): ten
  multi-crate Rust workspaces with hidden behavioral tests and oracle-patch
  references. Intended for a live provider; see [L3 corpus](#l3-corpus-m172).

### L1 capability corpus

| Task | Skill | Reference | Hidden verification |
|---|---|---|---|
| `rust-failing-unit-test` | bug fix (visible failing test) | `oracle_patch` | `max_value` at all positions, duplicates, `i64` extremes |
| `rust-hidden-sum-bug` | bug fix (no visible failing test) | `oracle_patch` | `add` on mixed-sign inputs |
| `rust-compiler-error` | compile error after a field rename | `oracle_patch` | `area` values; public `width`/`height` fields kept |
| `rust-implement-from-doc` | implement a `todo!()` fn from its doc | `oracle_patch` | run-length encoding: repeated runs, multi-digit counts, Unicode |
| `rust-edge-case-paginate` | edge-case handling / overflow | `oracle_patch` | page 0, `per_page` 0, past-the-end, `usize` overflow |
| `rust-refactor-preserve` | refactor preserving behavior | `oracle_patch` | behavior on all radii + at most one PI literal / negative check |
| `rust-api-addition` | small API addition | `oracle_patch` | `peek`, `is_empty`, `Default`, `FromIterator` |
| `rust-add-regression-test` | test writing | `oracle_patch` | slugify behavior + mutation check: the agent's test must fail against the original bug |
| `rust-error-handling` | panics → typed `Result` | `oracle_patch` | each `ParseError` variant, check order, `?` via `Box<dyn Error>` |
| `rust-borrow-iter-fix` | iterator ownership/borrowing | `oracle_patch` | order/boundary, borrowed (pointer-equal) names, change count |
| `rust-nochange-leap-year` | negative (`no_change`) | `validated_oracle` | Gregorian century rule; "fixing" 1900 fails |
| `rust-nochange-midpoint` | negative (`no_change`) | `validated_oracle` | rounding and `u32::MAX` inputs; `(a + b) / 2` fails |

Hidden fixtures live in each task's `hidden/` directory (outside `repo/`) and
are overlaid only after the agent finishes; oracle patches live beside
`task.toml`. Script checks under `hidden/hidden_checks/` run via `sh`.

### L2 corpus (M17.2)

Ten dependency-free, multi-module Rust crates (115–305 lines of Rust each) under
`crates/crom-eval/eval/tasks/l2-*`. Each change task's `oracle.patch` touches
at least two files; hidden tests in `hidden/tests/` are overlaid only after the
agent finishes. Public verification is `cargo test` unless noted.

| Task | Exercises | Reference touches |
|---|---|---|
| `l2-order-discount-codes` | cross-module feature (order, pricing, receipt) | 4 files |
| `l2-shape-trait-refactor` | trait signature change + new required/provided methods, call-site updates | 6 files |
| `l2-expr-test-cascade` | lexer + parser root causes behind a cascade of failing tests | 2 files |
| `l2-feature-flag-json` | repair code behind the `json` cargo feature; public verification also runs `cargo test --features json` | 2 files |
| `l2-config-error-enum` | replace `Result<_, String>` with a typed `ConfigError` across modules | 5 files |
| `l2-library-borrow-repair` | borrow-checker / type errors after a by-reference `Store` API change | 2 files |
| `l2-todo-cli-done` | new CLI subcommand plus an integration test that runs the binary | 4 files |
| `l2-contacts-schema-v2` | versioned text-format migration with escaping and a `migrate` helper | 4 files |
| `l2-pipeline-shutdown` | worker pool: hang (leaked sender), lock held across work, out-of-order results; deterministic watchdog tests | 3 files |
| `l2-noop-bankers-rounding` | `no_change`: bug report contradicts the documented rounding policy (`reference = "validated_oracle"`) | — |

```console
crom eval validate-suite l2                               # offline; CI runs this
crom eval run --suite l2 --provider <profile> --repair enabled
crom eval run --suite l2 --provider <profile> --repair disabled
```

Each run needs a cargo toolchain; no task needs the network.

### L3 corpus (M17.2)

Each `l3-*` task is a `kind = "capability"`, `level = "L3"` task whose
fixture is a realistic three-crate Cargo workspace with an existing, passing
test suite. The agent must locate the root cause or extension point across
crates; grading is behavioral.

| Task | Category | Crates | What the reference changes |
|---|---|---|---|
| `l3-ledger-split` | bugfix | `money`, `ledger`, `report` | Exact remainder allocation in `Money::allocate` |
| `l3-semver-prerelease` | bugfix | `versions`, `registry`, `resolver` | Pre-release precedence + Cargo pre-release matching |
| `l3-logstore-reopen` | bugfix | `wire`, `logstore`, `kvtool` | LEB128 varint decoding (data loss on reopen) |
| `l3-http-header-case` | bugfix | `http-types`, `middleware`, `server` | Case-insensitive header names and content codings |
| `l3-cron-ranges-steps` | feature | `calendar`, `cronexpr`, `scheduler` | `A-B`, `*/S`, `A-B/S` cron field syntax |
| `l3-buildplan-cycles` | bugfix | `manifest`, `depgraph`, `planner` | Cycle detection/reporting instead of a panic |
| `l3-csv-quoting` | bugfix | `csvread`, `schema`, `colstats` | RFC 4180 quoting, CRLF, embedded newlines |
| `l3-mdtoc-anchors` | bugfix | `mdscan`, `slugger`, `toc` | GitHub slugs, duplicate suffixes, fenced code |
| `l3-throttle-refill` | bugfix | `clock`, `bucket`, `gateway` | Token-bucket refill drift and exact `Retry-After` |
| `l3-layercfg-merge` | bugfix | `confval`, `layers`, `appconfig` | Deep merge of layers and `APP_A__B` env overrides |

Verification:

- Public oracle: `cargo test --workspace --offline` (passes on the
  unmodified fixture; guards against regressions).
- Hidden oracle: `acceptance_*.rs` integration tests overlaid from
  `hidden/crates/<crate>/tests/` after the agent finishes, run with
  `cargo test --offline -p <crate> --test <name>`.
- `crom eval validate-suite l3` proves, offline, that every reference patch
  passes public + hidden verification and every unmodified fixture fails it.
  CI runs it on every push/PR; `crates/crom-eval/tests/l3_corpus.rs` checks
  structure (kind/level, offline commands, hidden dir outside `repo/`,
  path-only dependencies, provenance, size budget).

Provenance and storage: the fixtures are original, synthetic workspaces
(`provenance.source = "original"`, `Apache-2.0 OR MIT`), not snapshots of
upstream repositories. They depend only on `std` and on each other through
path dependencies, so no `cargo vendor` tarball or registry cache is needed
and verification works with `network = "none"`. The corpus adds ~190 KB of
plain files to the repository (~4.1k lines of fixture Rust, 10–25 KB per
task); `l3_corpus.rs` caps each task at 96 KB. Larger pinned upstream
snapshots (the 2k–30k LoC target in #289) need a separate storage decision
(git bundle plus vendored-deps tarball, or a fixtures repository) and are
not part of this corpus.

Leakage: as for every task (#286), only `repo/` is copied into the agent
workspace; `hidden/` is overlaid onto the workspace root recursively after
the agent exits, so `hidden/crates/<crate>/tests/acceptance_*.rs` lands next
to that crate's existing `tests/`. `task.toml` and `oracle.patch` never enter
the workspace.

## Adding a new task

1. Create a directory under `crates/crom-eval/eval/tasks/<task-id>/`.
2. Add a `task.toml` manifest (see the reference below). New tasks should use
   schema v3 (`schema_version = 3` with `task_version`, `kind`, `level`); see
   [eval-authoring.md](eval-authoring.md) and the
   [v3 field reference](eval-schema.md#schema-v3-m17).
3. Add a `repo/` fixture directory containing the starting project. Keep it
   tiny. If it is a Rust crate, put an empty `[workspace]` table at the top of
   its `Cargo.toml` so the parent workspace never absorbs the fixture.
4. (Optional) Add the task to a suite and/or register a golden script in
   `crates/crom-eval/src/providers.rs` keyed by the task id.
5. Run `crom eval validate-task <task-id>`; it must report 0 errors.

Example layout:

```text
crates/crom-eval/eval/tasks/rust-failing-unit-test/
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
target/crom-eval/
  index.jsonl              # regression index, one entry per task result
  runs/<run-id>/
    run.json               # run metadata, schema v2 (provider, model, flags, seed, git, host, crom version)
    summary.json
    summary.md
    behavior-summary.json  # behavior metrics aggregated over the run
    repetitions.json       # only with --repetitions > 1
    tasks/<task-id>/
      task.toml            # copy of the manifest
      normalized-task.json # the task after adapter normalization
      adapter.json         # adapter that produced the task
      provider.json        # provider id, model, endpoint host (never the key)
      isolation.json       # isolation backend + settings
      environment.json     # host/toolchain metadata
      prompt.md            # the prompt sent to the agent
      transcript.jsonl     # conversation-relevant events
      events.jsonl         # every SDK event
      tool-calls.jsonl     # tool invocations
      security.jsonl       # SecurityDecision audit records (docs/security/audit.md)
      security-summary.json # audit coverage + blocked/executed counts
      final.patch          # unified diff vs the fixture baseline
      patch.diff           # same diff, for adapter/benchmark tooling
      final.diff           # diff --stat summary
      final_summary.md     # one-page outcome
      command-output/      # NNN-<cmd>.stdout / .stderr / .status per oracle command
      public-test.log      # public verification command log
      hidden-test.log      # hidden verification log (tasks with [hidden])
      verification.json    # per-check public/hidden verification results
      behavior.json        # behavior metrics for the task
      score.json
      result.json          # the full task result, schema v2 (status, outcome, failure class, usage, repro metadata, …)
      index-entry.json     # the row appended to index.jsonl
      report.md
      triage.md
      repair.json, repair.md, diagnostics.jsonl, validation-attempts.jsonl
                           # repair-loop artifacts (--repair enabled)
      workspace/           # verification copy: the agent workspace moved here after the
                           # agent exits, plus the hidden overlay (preserved)
```

With `--repetitions k` (k > 1) each repetition writes its own
`<run-id>/rep-<n>/tasks/<task-id>/` tree.

`result.json`, `run.json`, and `summary.json` follow result schema v2
(outcome classes, pass-rate denominators, reproducibility metadata,
comparability rule, v1 back-compat); see
[Result schema v2](eval-schema.md#result-schema-v2-m174).

Each task runs in its own git-initialised workspace under
`$TMPDIR/crom-eval-ws/`, outside the run directory, so grader material is not
beside it (#286). The final patch is the diff against the baseline commit.

## CI behavior

The `eval` job in `.github/workflows/ci.yml` runs a fast, deterministic,
offline lane on every push/PR:

```console
cargo test -p crom-eval
crom eval validate-suite <smoke|golden|safety|repair-smoke|l1|l2|l3>
crom eval run --suite smoke --provider fake
crom eval run --suite golden --provider golden
crom eval run --suite safety --provider fake
crom eval run --suite security --provider golden
crom eval run --suite repair-smoke --provider golden --repair enabled --allow-failures
```

`repair-smoke` runs with `--allow-failures` because `repair-grep-trap` is a
negative task; the job then asserts from each task's `result.json` that
`repair-grep-recover` resolved and `repair-grep-trap` did not, with
`repair.same_error_repeated = true`. Golden Rust tasks run `cargo test` inside
the fixture, so the job needs a working cargo toolchain.

It uploads the artifact bundle when any offline suite regresses. Live-provider
evals never run here.

Live evals are available manually via the `Eval (live provider)` workflow
(`.github/workflows/eval-live.yml`), triggered from the Actions tab. It sets
`CROM_EVAL_LIVE=1` and uses the `ANTHROPIC_API_KEY` repository secret;
without the secret, live tasks skip cleanly.

## Known limitations

- The fake/golden providers prove the harness and tool plumbing, not model
  intelligence. Only live runs measure real capability.
- Golden scripts are fixed sequences keyed by task id; they replay the same
  tool calls regardless of the (deterministic) workspace.
- Scoring heuristics (patch/process/efficiency) are pragmatic, not calibrated;
  treat sub-scores as signal, not ground truth.
- Oracle commands run in the host environment, so Rust tasks require a working
  cargo toolchain (and network for fixtures with external dependencies; those
  tasks set `requires_network = true`, e.g. `rust-serde-edge-case`).
- The workspace is isolated by directory (cwd), **not** by OS sandboxing. Under
  a live provider a task that enables Bash can in principle run commands that
  reach outside the fixture directory. Run live evals with Bash only against
  trusted tasks, or in an externally sandboxed environment (container/VM).
- Patch quality cannot tell "relevant" from "irrelevant" files perfectly; it
  uses tags (e.g. `small`) and simple churn/deletion heuristics.
- The harness avoids exact-diff matching by design: any patch that satisfies the
  command/file/behavior oracles is accepted.

## Repair loop (M16)

`crom eval run --suite <suite> --repair enabled` turns on the bounded repair loop:
failed validations are parsed into structured diagnostics, a targeted repair
brief drives bounded retries, repeated identical failures are detected and
stopped, and repair metrics + artifacts (`repair.json`, `repair.md`,
`diagnostics.jsonl`, `validation-attempts.jsonl`) are recorded. The
`repair-smoke` / `repair-mini` suites exercise it offline with the golden
provider. See [repair-loop.md](repair-loop.md) and [diagnostics.md](diagnostics.md).
