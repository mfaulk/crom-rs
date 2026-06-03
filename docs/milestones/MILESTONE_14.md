# Milestone 14 — Coding-Agent Capability Evaluation Harness

**Theme:** A first-class evaluation subsystem that runs the agent against
controlled coding tasks in isolated workspaces, scores the result, and emits
debuggable artifacts. It answers a question unit tests cannot: given a
realistic repository task, can the agent inspect code, plan, edit files, run
checks, recover from failures, and produce a correct patch?

The harness drives the agent exclusively through the M12 headless SDK runtime
(`code-agent-sdk`) and judges behavior from structured SDK events, not from
model prose. It runs offline and deterministically in CI via fake/golden
providers, with opt-in support for live model providers.

See [`docs/eval.md`](../eval.md) for the full user guide and reference.

## Deliverables

### New crate: `code-agent-eval` (`crates/eval`)

| Module | Purpose |
|--------|---------|
| `task.rs` | Serde TOML manifest types (`Manifest`, `EvalTask`, `FixtureSpec`, `OracleSpec`, `AssertionSpec`, `BehaviorSpec`) and task discovery from `eval/tasks/`. |
| `fixture.rs` | Isolated workspace setup: recursive fixture copy, fresh git repo + baseline commit, unified-diff/`--stat`/numstat patch generation against the baseline. |
| `providers.rs` | Provider execution modes — `fake` (echo), `golden` (scripted `ScriptedFakeProvider` keyed by task id), `live` (Anthropic, gated by `CODE_AGENT_EVAL_LIVE`). |
| `runner.rs` | Orchestration: builds the SDK runtime over a filesystem workspace, runs one task or a selection, captures events via an `EventSink`, enforces turn/timeout budgets, decides `EvalStatus`, writes the artifact bundle. |
| `oracle.rs` | Command / Rust / file-assertion / behavior oracles with structured pass/fail outcomes. |
| `score.rs` | Weighted structured score (correctness 60% / patch 15% / process 15% / efficiency 10%) serialised to `score.json`. |
| `report.rs` | Per-task `report.md`, run `summary.json` / `summary.md`, and `compare` between two runs. |
| `artifacts.rs` | Run/task artifact directory layout and writers (events, transcript, patches, command output, score, report, preserved workspace). |

Core types mirror the milestone spec: `EvalTask { id, title, description,
prompt, fixture, allowed_tools, max_turns, timeout, oracle, tags, ... }` and
`EvalResult { task_id, status, score, patch, transcript_path, events_path,
report_path, workspace_path, ... }`.

### Oracles

- **Command** — runs shell commands; passes iff all exit zero; captures
  stdout/stderr/exit code per command.
- **Rust** — expands to `cargo fmt --all --check`, `cargo test --all-targets`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test --doc` per flags.
- **File assertion** — `file_contains` / `file_not_contains` / `file_exists` /
  `file_missing` against the post-run workspace.
- **Behavior** (from structured events) — `must_emit_plan`, `must_use_tool`,
  `must_not_use_tool`, `must_run_command`, `must_not_run_command`.

### Provider execution modes

- **Fake** — deterministic echo provider; proves harness plumbing offline.
- **Golden** — scripted tool-call sequences (read → edit → run check → observe
  failure → edit → pass) keyed by task id in `providers.rs`.
- **Live** — opt-in only via `CODE_AGENT_EVAL_LIVE=1` plus `ANTHROPIC_API_KEY`;
  missing opt-in/credentials report tasks as **skipped**, never failed.

### CLI surface (`code-agent eval`)

| Command | Behavior |
|---------|----------|
| `eval list [--suite S] [--tag T] [--tasks-dir DIR]` | List discoverable tasks (id/title/tags/suites). |
| `eval run [TASK_ID] [--suite S] [--provider fake\|golden\|<live>] [--runs-dir DIR] [--allow-failures]` | Run one task or a suite; writes the artifact bundle; exits non-zero on non-pass tasks unless `--allow-failures`. |
| `eval report <run>` | Print the summary from a prior run. |
| `eval compare <baseline> <candidate>` | Diff pass/fail/score/tool-count/runtime across two runs. |

`CODE_AGENT_EVAL_MODEL` overrides the live model (default `claude-sonnet-4-6`).

### Tasks and fixtures (`crates/eval/eval/tasks/`)

Thirteen tasks across suites `smoke`, `golden`, `rust-small`, `safety`,
`planning`:

`smoke-noop`, `golden-edit-recover`, `golden-rust-recover`,
`rust-failing-unit-test`, `rust-add-regression-test`, `rust-compiler-error`,
`rust-clippy-cleanup`, `rust-cli-arg-order`, `rust-serde-edge-case`,
`rust-refactor-preserve`, `plan-act-small-feature` (requires `must_emit_plan`),
`policy-denied-bash` (Bash disabled by policy), `dangerous-command-denial`
(`must_not_run_command` for destructive patterns).

Rust fixtures declare an empty `[workspace]` table so the parent workspace
never absorbs them; `crates/eval/eval` is also listed under workspace
`exclude`.

### Artifact bundle

Per run under `target/code-agent-eval/runs/<run-id>/`: `summary.json`,
`summary.md`, and per task `task.toml`, `prompt.md`, `transcript.jsonl`,
`events.jsonl`, `final.patch`, `final.diff`, `final_summary.md`,
`command-output/`, `score.json`, `report.md`, and the preserved `workspace/`.

## Safety notes

- Workspaces are isolated copies; the harness never mutates the fixture sources.
- Live provider runs require an explicit `CODE_AGENT_EVAL_LIVE=1` opt-in and
  credentials; absent either, the run skips cleanly rather than failing.
- Safety tasks assert the agent avoids destructive commands and behaves
  correctly when a tool is disabled by policy.
- `allowed_tools` maps to the SDK `BuiltinToolSet`, so a task can disable Bash
  (or any tool) at the policy layer.

## Tests

- `code-agent-eval` unit tests cover manifest parsing, fixture copy, git
  baseline + patch generation, command/file/behavior oracles, score
  serialisation, report/summary/compare rendering, and provider selection.
- `tests/harness.rs` drives the full pipeline end-to-end offline: task
  discovery (≥8), the smoke suite passing under the fake provider, and the
  golden edit-and-recover task (read → failing check → edit → passing check)
  with the command oracle confirming the post-run workspace.

## CI

- `ci.yml` gains an `eval` job: `cargo test -p code-agent-eval` plus
  `code-agent eval run --suite smoke --provider fake`, uploading the artifact
  bundle on failure. Deterministic and offline.
- `eval-live.yml` is a manual `workflow_dispatch` lane for live-provider evals
  (`CODE_AGENT_EVAL_LIVE=1`, `ANTHROPIC_API_KEY` secret); never runs on push/PR.

## Out of scope

- Calibrated/learned scoring models — scoring is intentionally pragmatic.
- Large or real-world fixture repositories — fixtures are deliberately tiny.
- Exact-diff matching — any patch satisfying the oracles is accepted.
- TUI integration of eval results.
