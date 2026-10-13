# Repair loop (M16)

The repair loop makes `crom` better at the **edit → validate → diagnose →
repair** cycle. When a validation command fails after an edit, the runtime
parses the failure into a structured [diagnostic](diagnostics.md), classifies
it, summarizes it compactly, and feeds a targeted **repair brief** back into the
agent so it can make a focused fix — instead of stopping, thrashing on the same
error, or making broad speculative edits.

Everything is bounded and configurable, and can be fully disabled.

## How it works

1. The agent mutates files (Write/Edit, or a mutating shell command).
2. The runtime marks the workspace as needing validation.
3. The agent runs a validation command (`cargo test`, `cargo check`, ...).
4. **Pass** → the validation-needed flag clears.
5. **Fail** → the output is parsed into a `CommandDiagnostic` (primary error,
   failing tests, span, fingerprint).
6. The bounded `RepairController` records the attempt and decides:
   - emit a `RepairBrief` for the next attempt, or
   - **stop** because the same failure repeated too many times, or
   - **stop** because the attempt budget is exhausted.
7. The brief is rendered to concise text and used as the next attempt's input.

The controller also tracks **validation-after-mutation** so the harness can flag
a final answer produced after an edit that was never validated.

### Repair brief

A brief is provider-independent and concise:

```text
Validation failed after your last edit.

Command:
cargo test

Primary failure:
type_mismatch [E0308] in src/config.rs:74: mismatched types

Failing tests:
- cli::json_format_outputs_valid_json

Relevant excerpt:
<compact diagnostic excerpt>

This is repair attempt 1 of 3.

Instructions:
- Inspect the reported file/span before editing.
- Make the smallest targeted change that addresses this failure.
- Do not change unrelated files.
- Re-run focused validation first if possible: cargo test cli::json_format_outputs_valid_json
- Then run broader validation if the focused check passes.
```

When the same failure repeats, the brief prepends a stronger instruction not to
re-run the same command without changing the root cause.

### Focused validation

After a failure the loop suggests narrower commands: a failing unit test
`foo::bar` → `cargo test foo::bar`; an integration test file `tests/api.rs` →
`cargo test --test api`; a fmt failure → `cargo fmt --check`; a clippy failure →
`cargo clippy -- -D warnings`. Suggestions are included in the brief — they do
**not** bypass the Bash/tool permission policy.

## Configuration

```toml
[repair]
enabled = true
max_attempts = 3
max_same_failure_repeats = 2
require_inspection_before_repair = true
prefer_focused_validation = true
require_validation_after_mutation = true
stop_on_repeated_identical_failure = true

[diagnostics]
prefer_cargo_json = true
max_excerpt_bytes = 12000
max_errors = 5
max_warnings = 5
max_failing_tests = 10
include_backtrace = "short"   # off | short | full

[validation]
classify_commands = true
focused_validation_suggestions = true

[completion]
require_validation_after_mutation = true
allow_unvalidated_final_answer = false
```

Defaults are safe and bounded. Setting `repair.enabled = false` turns the loop
into a no-op (diagnostics are still parsed for metrics, but no brief is emitted
and no extra attempts run). Eval/headless defaults are stricter than the
interactive defaults: the eval harness ships repair **disabled** by default and
is opted-in per run with `--repair enabled`.

## Eval integration

The eval harness drives the repair loop on the real runner path. Enable it on a
run:

```text
crom eval run <suite> --repair enabled
crom eval run <suite> --repair disabled
crom eval run <suite> --repair-max-attempts 3
crom eval run <suite> --require-validation-after-edit
```

Per task/run, the harness writes repair artifacts alongside the existing
command-output logs:

```text
tasks/<task-id>/
  repair.json                # the structured repair trace
  repair.md                  # human-readable repair trace
  diagnostics.jsonl          # one CommandDiagnostic per failed validation
  validation-attempts.jsonl  # one record per validation attempt
```

Raw command logs are always preserved; the repair artifacts add structured
summaries on top.

Repair metrics are added to each task result and surfaced in `summary.json`,
`summary.md` (a **Repair loop** section plus a per-task table), and
`crom eval show <run-id> <task-id>`. `crom eval triage <run-id>`
groups repeated repair failures. See [eval-schema.md](eval-schema.md#repair-metrics-m16)
for the metric and failure-class definitions.

## Worked example

A task ships a buggy `add()` that fails `cargo test`. The agent reads the file,
runs `cargo test` (it fails), and the harness parses the failure:

```text
type_mismatch [E0308] in src/config.rs:74: mismatched types
```

A repair brief is injected naming the file/span and suggesting
`cargo test <failing_test>` as a focused check. The agent inspects the span,
makes a one-line targeted fix, runs the focused test (passes), then the broad
suite (passes). The final `repair.json` records `"recovered": true` and
`"final_validation_status": "passed"`.

If instead the agent keeps making the same non-fixing edit, the identical
failure fingerprint repeats; after `max_same_failure_repeats` the loop stops and
the task is classified `repeated_same_failure` rather than thrashing to the
attempt limit.

## Limitations

- The repair brief is injected through the eval harness's attempt loop (a real
  SDK runtime path). The interactive TUI exposes the same diagnostics and
  configuration, but the brief-injection retry loop is currently driven by the
  eval/headless path.
- `validation_after_last_edit` depends on the command classifier recognizing the
  validation command (cargo/rustc/make families). Non-cargo project checks may
  read as "no validation" — the metric is reported honestly (`None`/`false`)
  rather than fabricated.
- Diagnostic parsing coverage is the union of the Cargo JSON and plain-text
  rustc/libtest shapes; see [diagnostics.md](diagnostics.md#limitations).
