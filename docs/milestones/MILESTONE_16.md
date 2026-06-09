# Milestone 16 — Repair Loop v1: Test Feedback Mastery

**Theme:** Make `code-agent` substantially better at the
edit → validate → diagnose → repair cycle. Compiler errors, test failures,
panics, and command output stop being unstructured text blobs: they are captured
as structured diagnostics, classified, summarized compactly, fingerprinted for
repeated-failure detection, and fed back into the agent as targeted, bounded
repair briefs. Repair behavior is tracked in eval artifacts and reports. The
M14/M15 eval framework, manifests, suites, and provider lanes continue to work
unchanged; repair is opt-in.

See [`docs/repair-loop.md`](../repair-loop.md),
[`docs/diagnostics.md`](../diagnostics.md), and
[`docs/eval-schema.md`](../eval-schema.md#repair-metrics-m16) for the user guide
and reference.

## Deliverables

### Diagnostics substrate (`crates/code-agent-core/src/diagnostics/`)

- `CommandDiagnostic` model: `status`, parsed records, distilled `summary`,
  stable `fingerprint`, capped stdout/stderr excerpts. Every enum carries an
  explicit `Unknown`; parsing never panics on malformed/truncated output.
- `parse_command_diagnostic(CommandDiagnosticInput) -> CommandDiagnostic` is the
  single entry point.
- Rust/Cargo extraction (`rust.rs`): Cargo JSON (`--message-format=json…`)
  preferred, plain-text rustc fallback. Code-first classification
  (`E0308 → TypeMismatch`, `E0432 → MissingImport`, `E0277 → TraitBound`,
  `E0382 → MoveError`, `E0502/E0499/E0505 → BorrowChecker`, …) with message
  heuristics as fallback. Captures help/note text and suggested replacements.
- Cargo-test extraction (`cargo_test.rs`): failing test names, panic
  file/line, `assertion left == right` snippets, multiple failures, summary
  counts; classifies `TestAssertionFailure`/`TestPanic`/…
- Compaction (`compact.rs`): deterministic, high-signal, never drops the primary
  failure; configurable via `[diagnostics]`.
- Fingerprinting + repeated-failure detection (`fingerprint.rs`):
  path/number/ANSI normalization so the same failure across temp dirs hashes
  identically; `FailureHistory::record → NewFailure | Repeated { count }`.
- Command classification (`command.rs`): `CommandIntent` set per command;
  focused-validation plan generation. Augments, does not replace, the
  permission policy.

### Repair controller (`crates/code-agent-core/src/repair.rs`)

- `RepairPolicy` (bounded, disableable), `RepairState` (serializable),
  `RepairController` driving `observe_validation → Cleared | Repair | Stop |
  Disabled`. Tracks validation-after-mutation and repeated fingerprints.
- `RepairBrief` with concise `render()`; `RepairAttemptRecord` for artifacts.

### Configuration (`settings.rs`)

- `[repair]`, `[diagnostics]`, `[completion]` sections plus
  `[validation].classify_commands` / `.focused_validation_suggestions`. Invalid
  `diagnostics.include_backtrace` is rejected at load. `RepairConfig::to_policy`
  / `DiagnosticsConfig::to_compaction` build effective values with defaults.

### Eval integration (`crates/eval/src/repair.rs`, `runner.rs`)

- `RepairSession` drives the runner's attempt loop: parses each failed
  validation, generates the brief used as the next attempt's input, bounds
  attempts, and stops on repeated identical failure.
- `RepairMetrics` on every `EvalResult`; repair-aware refinement of the failure
  class (original kept as secondary).
- Repair artifacts per task: `repair.json`, `repair.md`, `diagnostics.jsonl`,
  `validation-attempts.jsonl`. Raw command logs remain in the existing
  `command-output/` artifacts.
- `report.rs`: aggregate `repair` object + **Repair loop** section + per-task
  repair table in `summary.{json,md}`. `triage.rs`: repair classes grouped;
  `eval show` displays the repair trace.

### CLI

- `code-agent diagnostics parse | summarize | fingerprint <log-file>` — parse
  captured logs without an agent session (`-` for stdin).
- `code-agent eval run … --repair enabled|disabled --repair-max-attempts N
  --require-validation-after-edit --diagnostic-format text|json`.

### Repair eval suite

- `repair-smoke` / `repair-mini` suites with golden-scripted tasks:
  `repair-grep-recover` (within-run recovery) and `repair-grep-trap` (repeated
  identical failure detected and stopped). Runnable offline in CI with the
  golden provider.

## Failure classes added

`unrecovered_compile_error`, `unrecovered_test_failure`, `repeated_same_failure`,
`unvalidated_completion`, `repair_attempt_limit_exceeded`,
`diagnostic_parse_failure`. The M15 taxonomy is preserved.

## Tests

- Parser unit tests (rustc JSON + plain text, cargo-test assertions/panics/multi,
  timeout/command-not-found, compaction, stable + distinct fingerprints).
- Command classification, repair controller (budgeting, repeated-failure stop,
  validation-after-mutation, focused suggestion, disabled no-op).
- Eval `RepairSession` unit tests + end-to-end integration tests on the real
  runner path with the golden provider (recover, repeated-failure stop, disabled).
- Settings round-trip + invalid-config rejection.

## Safety / CI

- No live providers, network, Docker, or Podman required for normal tests.
- Repair is bounded (no infinite retries) and disableable; eval defaults to
  repair **disabled** (opt-in per run), preserving existing behavior.
- Unknown/unavailable values are `None`/`null`, never fabricated. Raw logs are
  always preserved alongside compacted summaries.

## Out of scope

rust-analyzer integration, repository symbol index, new external benchmark
adapters, multi-agent repair debate, LLM-as-judge scoring, patch-minimization
engine, TUI redesign. Brief injection into the interactive TUI's own retry loop
is deferred; the eval/headless path drives the bounded retry today.
