# Milestone 15 — Eval Framework v2: Benchmark Interop, Reliability, and Regression Intelligence

**Theme:** Extend the M14 evaluation harness from a small internal coding-agent
eval into an adapter-based, schema-versioned, isolation-aware, flake-aware,
behavior-aware, regression-tracking platform that runs internal tasks and small
external benchmark-style slices through the same artifact and reporting
pipeline. M14 manifests, suites, and the `fake`/`golden`/live provider lanes
continue to work unchanged.

See [`docs/eval.md`](../eval.md), [`docs/eval-schema.md`](../eval-schema.md),
[`docs/eval-adapters.md`](../eval-adapters.md), and
[`docs/eval-isolation.md`](../eval-isolation.md) for the user guide and
reference.

## Deliverables

### Schema v2 (`schema.rs`, `task.rs`)

- `schema_version` (defaults to 1 for M14 manifests); unknown versions rejected
  unless `--allow-future-schema`.
- New manifest surface: `category`, `difficulty`, `suite` alias, `visibility`,
  `[capabilities]`, `[behavior]` rules, `[quarantine]`, `[prompt_variants]`,
  `[hidden]` verification, `required_paths`, `forbidden_paths`, `oracle_patch`,
  `requires_network`. Every field defaults, so v1 manifests parse and run
  unchanged.

### Adapters (`adapter.rs`, `external.rs`)

- `EvalAdapter` trait + `AdapterRegistry`. The runner is adapter-agnostic; an
  adapter's job is discovery + normalization into `EvalTask`.
- Adapters: `internal`, `directory` (full); `swebench`, `aider`, `terminal`
  (MVP, local fixtures only). `eval adapters` lists them.

### Isolation backends (`isolation.rs`)

- `IsolationBackend` trait, `LocalBackend` (default, everywhere), and a
  `ContainerBackend` for `docker`/`podman` (opt-in via `CODE_AGENT_EVAL_DOCKER`
  / `CODE_AGENT_EVAL_PODMAN`; missing engine ⇒ `isolation_error`).
- `CommandLimits` (timeout/network/cpu/memory/image); network disabled by
  default for verification. `EnvMetadata` written to `environment.json`.

### Hidden verification (`runner.rs`)

- `[hidden]` checks (and optional fixture overlay) run **only after** the agent
  completes, so hidden tests are never visible during the run.
- Primary binary score `resolved = public && hidden && no forbidden-path change
  && no required-path missing && not timed out && no enforced-rule violation`.

### Behavior analysis (`behavior.rs`)

- `BehaviorMetrics` derived from the event/patch timeline: `read_before_write`,
  `grep_before_edit`, `test_after_edit`, `compiler_error_recovered`,
  `same_error_repeated`, `plan_submitted`, `large_blind_edit`, `deleted_tests`,
  `weakened_assertions`, `cargo_toml_changed`, `cargo_lock_changed`,
  `validation_command_count`, `edit_count`, `unique_files_read/edited`, … with
  `Option<_>` where data is unavailable.
- `[behavior]` rules evaluated into quality signals (advisory unless `enforce`).

### Failure taxonomy (`failure.rs`)

- `FailureClass` (21 variants incl. `compile_error`, `public_test_failure`,
  `hidden_test_failure`, `overfit_public_tests`, `forbidden_path_changed`,
  `timeout`, `isolation_error`, `adapter_error`, `oracle_validation_failed`, …)
  with one primary class + secondary labels per failed run.

### Validation + flake detection + canaries (`validate.rs`)

- `validate-task` / `validate-suite [--repeat N]`: manifest/schema, fixture
  materialization, public + hidden runnability, base-fails-hidden, oracle-patch
  apply + pass, path-policy sanity, network declaration; flake verdicts
  (`stable_pass`/`stable_fail`/`flaky_public`/`flaky_hidden`/`timeout_sensitive`
  /`harness_error`); canary checks for leaked metadata / hidden names / visible
  oracle or hidden dirs.

### Multi-run statistics (`multirun.rs`)

- `--repetitions k`: pass@1 / pass@k, per-task pass rate + flaky flag, failure
  -class distribution, median cost/wall/tool-calls. Written to
  `repetitions.json`.

### Regression index + trends (`index.rs`)

- Append-only JSONL index at `target/code-agent-eval/index.jsonl` (run id, git
  sha/branch/dirty, suite, task, adapter, isolation, provider, model, resolved,
  failure class, cost, wall, tool calls, patch lines, category, tags,
  capabilities).
- `eval history`, `eval trend <suite>`, `eval regressions --baseline --candidate`
  (newly-fixed / new-regressions / unchanged / chronic / failure-class shift /
  median tool-call delta), `eval task-history <task-id>`.

### Triage (`triage.rs`)

- `eval triage <run>` groups failures by class (highest-value first);
  `eval show <run> <task>` prints prompt/status/failure/behavior-warnings/last
  tool calls/verification tails/artifact paths; `eval open <run> <task>` lists
  artifact paths.

### Report aggregation (`report.rs`)

- `summary.json`/`summary.md` add `resolved` count, `failure_classes`, and
  resolved-rate aggregation by category / tag / capability / adapter / isolation
  / provider / model, plus a behavior summary.

## CLI surface added (`code-agent eval`)

`adapters`, `validate-task`, `validate-suite`, `history`, `trend`,
`regressions`, `task-history`, `triage`, `show`, `open`, plus `run` flags:
`--adapter`, `--isolation`, `--network`, `--container-image`, `--cpu-limit`,
`--memory-limit`, `--repetitions`, `--prompt-variant`, `--attempts`,
`--include-private`, `--no-index`. M14's `list`/`run`/`report`/`compare` keep
working.

## Artifacts added

Per task: `result.json`, `behavior.json`, `verification.json`,
`tool-calls.jsonl`, `patch.diff`, `public-test.log`, `hidden-test.log`,
`environment.json`, `isolation.json`, `adapter.json`, `normalized-task.json`,
`index-entry.json`, `triage.md`. Per run: `repetitions.json`,
`behavior-summary.json`, `run.json`. M14 artifacts are preserved.

## Corpus

Existing tasks upgraded to schema v2 with `category`/`difficulty`/capability
labels and added to a `rust-mini` suite. New tasks: `noop-already-correct`
(negative), `prompt-variants-demo`, `private-canary-demo` (private suite), and
`rust-hidden-sum-bug` (hidden tests + oracle patch + path policy).

## Tests

Deterministic, offline (`code-agent-eval` unit tests + `tests/harness.rs` +
`tests/m15.rs`): schema v1/v2 parsing and rejection, capability/behavior/
quarantine/prompt-variant parsing, adapter registry + discovery, validation +
flake + canary, local isolation + timeout + env metadata, behavior metrics,
multi-run aggregation, index round-trip + history/trend/regressions, triage/
show, and end-to-end hidden verification + directory + swebench-local runs.

## Safety / CI

Normal tests require no API keys, network, Docker, or Podman. Live providers
remain opt-in (`CODE_AGENT_EVAL_LIVE=1`); container backends are opt-in
(`CODE_AGENT_EVAL_DOCKER`/`CODE_AGENT_EVAL_PODMAN`) or report `isolation_error`.
Hidden tests are never present in the workspace during the agent run.

## Out of scope

Full SWE-bench Verified execution, leaderboard submission, full Terminal-Bench
compatibility, a web dashboard, LLM-as-judge scoring, and SQLite indexing
(JSONL is used instead) — per the milestone non-goals.
