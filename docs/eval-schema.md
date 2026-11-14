# Eval task manifest schema

Tasks are described by a `task.toml` manifest plus an adjacent fixture
directory. The harness supports three schema versions:

- **v1** — the M14 manifest. No `schema_version` field; everything defaults.
- **v2** — the M15 manifest. Declares `schema_version = 2` and may add
  capability labels, behavior rules, hidden verification, quarantine,
  visibility, prompt variants, and required/forbidden path policy.
- **v3** — the M17 manifest. Declares `schema_version = 3` and adds a content
  version, capability level, task kind, provenance, limits, network,
  toolchain, reference, and expected outcome. See
  [Schema v3 (M17)](#schema-v3-m17).

A manifest that omits `schema_version` is treated as v1. Unknown versions are
rejected; there is no CLI flag to opt into a newer schema. All versions run
through the same runner; every later-version field has a default, so v1 and v2
manifests parse unchanged. v3-only fields in a v1/v2 manifest are a load error
rather than being silently ignored.

All shipped tasks under `crates/crom-eval/eval/tasks/` are v3.

## Field reference

v1/v2 fields (all still valid in v3; v3 fields are listed
[below](#schema-v3-m17)):

```toml
schema_version = 2                  # 1 (default), 2, or 3
id = "rust-bugfix-cli-arg-001"      # unique id (also the directory name)
title = "Fix CLI arg parsing bug"
description = "optional long description"
category = "bugfix"                 # bugfix | feature | refactor | test_writing | cli | negative | safety | ...
difficulty = "easy"                 # easy | medium | hard (free-form)
tags = ["rust", "single-file", "hidden-tests"]
suites = ["rust-mini"]              # suites this task joins
suite = "rust-mini"                 # convenience alias, merged into `suites`
mode = "modify_workspace"           # or "plan_then_act"
visibility = "public"               # or "private"
golden_script = "..."               # optional; defaults to id

prompt = "The CLI fails when --output is passed after the input path. Fix it."

max_turns = 20                      # turn budget (default 20)
timeout_secs = 180                  # per-task wall clock (default 180)
allowed_tools = ["Read", "Edit", "Bash"]  # empty = all built-in tools

# Path policy (v2).
required_paths = ["src/lib.rs"]     # must exist after a correct solution
forbidden_paths = ["Cargo.toml", "tests/"]  # a change here is a policy violation
oracle_patch = "oracle.patch"       # reference solution (validates solvability)
requires_network = false            # must be true to allow network in verify

[capabilities]                      # what the task exercises (all default false)
requires_repo_search = true
requires_test_feedback = true
requires_multifile_edit = false
requires_public_api_preservation = true
requires_cli_execution = false
requires_async_reasoning = false
requires_feature_flags = false
negative_noop = false
safety_sensitive = false

[behavior]                          # quality rules (advisory unless `enforce`)
require_read_before_write = true
require_validation_after_edit = true
penalize_test_deletion = true
penalize_large_blind_edit = true
penalize_lockfile_change = true
enforce = false                     # when true, a violated rule un-resolves the task

[quarantine]                        # optional; quarantined tasks reported separately
enabled = true
reason = "hidden test flaky on macOS"
expires = "2026-07-01"

[prompt_variants]                   # selected via --prompt-variant <name>|random
concise = "Fix the arg-order bug."
verbose = "..."
bug_report = "..."
customer_request = "..."

[fixture]
kind = "copy_dir"                   # or "empty"
path = "repo"                       # relative to the task directory

[oracle]                            # PUBLIC verification (visible during the run)
kind = "command"                    # "command" | "rust" | "none"
commands = ["cargo test --all-targets"]
command_timeout_secs = 120          # optional per-command timeout
# for kind = "rust": fmt/test/clippy/doc_tests booleans

[[oracle.assertions]]
kind = "file_contains"              # file_contains | file_not_contains | file_exists | file_missing
path = "src/lib.rs"
text = "pub fn parse_args"

[[oracle.behavior]]
kind = "must_run_command"           # must_emit_plan | must_use_tool | must_not_use_tool | must_run_command | must_not_run_command
command_contains = "cargo test"

[hidden]                            # HIDDEN verification (applied AFTER completion)
fixture = "hidden"                  # optional dir overlaid post-completion

[hidden.oracle]                     # same shape as [oracle]
kind = "command"
commands = ["cargo test --test hidden"]
```

### Path policy matching

`required_paths` / `forbidden_paths` entries match a changed file by exact
path, by directory prefix (`tests/`), or by a trailing `*` wildcard
(`src/gen*`). They are evaluated against `final.patch`, which never contains
`target/` or the workspace-root `Cargo.lock` (oracle commands regenerate them,
so the harness excludes them from the diff). A forbidden rule on those paths
can therefore never fire.

`requires_network = true` is required for any task whose verification needs
the network — for example a Rust fixture with crates.io dependencies
(`rust-serde-edge-case`) on a host without a warm registry cache.

## Schema v3 (M17)

```toml
schema_version = 3
task_version = "1.0.0"          # required; MAJOR.MINOR.PATCH. Bump on any prompt/fixture/oracle change
kind = "capability"             # required; harness | capability | security
level = "L1"                    # required; L0..L5 (capability tasks must be L1+)
network = "none"                # optional; none | registry (registry implies requires_network)
toolchain = "1.88.0"            # optional Rust toolchain pin (non-empty)
reference = "oracle_patch"      # oracle_patch | validated_oracle; required for capability tasks
oracle_patch = "oracle.patch"   # required when reference = "oracle_patch"
expected_outcome = "change"     # change (default) | no_change

[provenance]                    # optional; unknown keys rejected
author = "crom maintainers"
source = "original"             # or an upstream URL / benchmark name
license = "Apache-2.0 OR MIT"   # SPDX expression
created = "2026-11-14"          # YYYY-MM-DD

[limits]                        # optional; unknown keys rejected, zero values rejected
wall_secs = 300                 # overrides timeout_secs
max_turns = 16                  # overrides max_turns
cpu = 2.0                       # declared; not yet enforced by the runner
memory_mb = 2048                # declared; not yet enforced by the runner
max_tool_calls = 60             # declared; not yet enforced by the runner
max_cost_usd = 0.50             # declared; not yet enforced by the runner
```

### Kinds and levels

| `kind` | Meaning |
|---|---|
| `harness` | Validates the runtime, scripted (`golden`) or `fake` providers, and policy plumbing. Succeeds by construction offline; never a capability signal. Always `L0`. |
| `capability` | Measures coding capability at `L1`..`L5`. Must satisfy the rules below. |
| `security` | Adversarial/policy tasks (M17.3). No extra validation rules yet. |

Levels follow the capability ladder in
[MILESTONE_17.md](milestones/MILESTONE_17.md#5-capability-ladder): `L0`
runtime correctness, `L1` single-file coding, `L2` multi-step coding and
repair, `L3` real-repository engineering, `L4` genuine engineering issues,
`L5` external benchmark comparison.

### Load-time rules (`schema_version = 3`)

Checked whenever a manifest is loaded (`eval run`, `eval list`,
`validate-task`, ...); a violation is a load error:

- `task_version`, `kind`, and `level` are present; `task_version` is
  `MAJOR.MINOR.PATCH`.
- `kind = "capability"` is not `L0`.
- `network = "none"` is not combined with `requires_network = true`.
- `reference = "oracle_patch"` has an `oracle_patch` path.
- `reference = "validated_oracle"` is only used with
  `expected_outcome = "no_change"`.
- `provenance.created` is `YYYY-MM-DD`; `toolchain` is non-empty; limits are
  positive (`max_cost_usd` may be 0).

### Capability-task rules (`validate-task` / `validate-suite`)

For `kind = "capability"`, `crom eval validate-task` additionally runs the
fixture and fails (error) unless:

- `capability_hidden`: `[hidden]` declares at least one check.
- `capability_reference`: a `reference` is declared.
- `capability_base_outcome`: the unmodified fixture **fails** public + hidden
  verification, or — for `expected_outcome = "no_change"` — **passes** it.
- With `reference = "oracle_patch"`, the existing `oracle_patch_applies`,
  `oracle_patch_passes_public`, and `oracle_patch_passes_hidden` checks must
  pass. With `reference = "validated_oracle"` the unmodified fixture is the
  reference, so `capability_base_outcome` covers it.

`expected_outcome = "no_change"` (or the v2 `capabilities.negative_noop`
label) also suppresses the `base_fails_hidden` warning.

## Hidden verification

`[hidden]` holds checks that run **only after the agent completes**. If
`[hidden].fixture` is set, that directory is overlaid into the workspace just
before the hidden oracle runs — so hidden test files are never present while the
agent works, and the agent cannot overfit to them.

A task is **resolved** when:

```
public verification passes
&& hidden verification passes (if present)
&& no forbidden path changed
&& no required path missing
&& not timed out
&& no enforced behavior-rule violation
```

## Capabilities, categories, and tags

Reports and the regression index aggregate resolved-rate by `category`, each
`tag`, each enabled capability label, adapter, isolation backend, provider, and
model — so a regression scoped to (say) `requires_multifile_edit` is visible.

Run reports do not yet aggregate by v3 `kind`/`level`. Until they do, compute
capability pass rates only from suites whose tasks are all
`kind = "capability"`; the offline CI suites (`smoke`, `golden`, `safety`,
`repair-*`) are entirely `kind = "harness"`.

## Complete example

See `crates/crom-eval/eval/tasks/rust-hidden-sum-bug/task.toml` for a full v3
capability task with hidden tests, an oracle patch reference, provenance,
capability labels, behavior rules, and path policy.

## Repair metrics (M16)

When the repair loop is enabled (`crom eval run --suite <suite> --repair enabled`),
each task result carries a `repair` object and the run writes repair artifacts
(`repair.json`, `repair.md`, `diagnostics.jsonl`, `validation-attempts.jsonl`).
See [repair-loop.md](repair-loop.md) for the end-to-end flow.

Per-task `repair` metrics:

```text
enabled                            # was the repair loop on for this run
repair_attempt_count               # number of repair briefs generated
repair_success_count               # 1 if a failed validation was later recovered
compiler_error_recovered           # Option<bool> (None if no compile failure seen)
test_failure_recovered             # Option<bool> (None if no test failure seen)
same_error_repeated                # an identical failure fingerprint repeated
same_error_repeat_count            # max repeat count of any fingerprint
focused_validation_suggested       # the brief suggested a focused command
focused_validation_used            # Option<bool>: the agent ran a focused validation
validation_after_last_edit         # Option<bool>: a validation ran after the last edit
final_answer_without_validation    # finalized after a mutation without validating
diagnostic_parse_success           # the failing output parsed into a diagnostic
diagnostic_primary_kind            # the parsed DiagnosticKind, if any
diagnostic_primary_file/line       # the primary span, if any
repair_attempt_limit_exceeded      # the bounded loop hit its attempt budget
final_validation_status            # passed | failed | unknown
```

Unknown or unavailable values are `null`/`None` — never fabricated.

New failure classes (in addition to the M15 taxonomy):

```text
unrecovered_compile_error
unrecovered_test_failure
repeated_same_failure
unvalidated_completion
repair_attempt_limit_exceeded
diagnostic_parse_failure
```

When a task does not resolve and a repair signal applies, the primary failure
class is refined to one of the above and the original class is kept as a
secondary label, so the existing taxonomy is preserved.

The run `summary.json`/`summary.md` include an aggregate `repair` object and a
**Repair loop** report section (tasks with failed validation, recovered after
repair, unrecovered compile/test errors, repeated failures, median attempts,
focused-validation usage, validation-after-edit rate) plus a per-task table.
