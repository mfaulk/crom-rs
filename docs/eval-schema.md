# Eval task manifest schema

Tasks are described by a `task.toml` manifest plus an adjacent fixture
directory. The harness supports two schema versions:

- **v1** — the M14 manifest. No `schema_version` field; everything defaults.
- **v2** — the M15 manifest. Declares `schema_version = 2` and may add
  capability labels, behavior rules, hidden verification, quarantine,
  visibility, prompt variants, and required/forbidden path policy.

A manifest that omits `schema_version` is treated as v1. Unknown versions are
rejected unless `--allow-future-schema` is passed. Both versions run through the
same runner; every v2 field has a default, so v1 manifests parse unchanged.

## Field reference

```toml
schema_version = 2                  # 1 (default) or 2
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
forbidden_paths = ["Cargo.lock"]    # a change here is a policy violation
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

## Complete example

See `crates/crom-eval/eval/tasks/rust-hidden-sum-bug/task.toml` for a full v2 task
with hidden tests, an oracle patch, capability labels, behavior rules, and path
policy.

## Repair metrics (M16)

When the repair loop is enabled (`crom eval run <suite> --repair enabled`),
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
