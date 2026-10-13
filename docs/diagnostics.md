# Command diagnostics (M16)

`crom` turns raw command output (compiler errors, test failures, panics,
shell errors) into a structured, serializable **diagnostic** instead of an
opaque text blob. Diagnostics power the repair loop, repair artifacts, and eval
metrics, and are exposed directly through the `crom diagnostics` CLI.

The implementation lives in `crates/crom-core/src/diagnostics/` and is
pure logic with no I/O, so it is reused by the Bash tool path, the repair
controller, the eval harness, and the CLI.

## What is parsed

A single command invocation becomes a `CommandDiagnostic`:

- `status` — `Success | ExitFailure | Timeout | Signaled | SpawnFailure`,
  derived from the exit code / timeout flag.
- `parsed` — a list of `ParsedDiagnostic` records (one per compiler error,
  failing test, lint, panic, ...). Each carries a `source`, `severity`, `kind`,
  optional rustc `code`, message, file/line/column span, rustc help/note text,
  and machine-applicable suggested fixes when present.
- `summary` — a compact distillation: the primary failure kind, primary
  file/line, the list of failing tests, the set of affected files, a one-line
  `short_message`, and a `repair_hint`.
- `fingerprint` — a stable identity for the failure (see below).
- `stdout_excerpt` / `stderr_excerpt` — capped copies of the raw streams. The
  full, un-truncated logs are preserved separately (the Bash tool result text
  and the eval `command-output/` artifacts).

Parsing is defensive: malformed or truncated output never panics, and anything
it cannot classify becomes `DiagnosticKind::Unknown` rather than a guess.

### Rust / Cargo

Two paths, preferred first:

1. **Cargo JSON** (`cargo {check,test,clippy} --message-format=json...`). The
   parser reads `compiler-message` records and extracts the code, level,
   rendered diagnostic, primary span, child notes/help, and suggested
   replacements.
2. **Plain-text fallback** scans `error[E0308]: ...`, `error: ...`,
   `warning: ...` headers and the following `--> file:line:col` location line.

Failures are classified into `DiagnosticKind` using the rustc error **code**
first (e.g. `E0308 → TypeMismatch`, `E0432 → MissingImport`,
`E0277 → TraitBound`, `E0382 → MoveError`, `E0502/E0499/E0505 → BorrowChecker`)
and **message heuristics** second when no code is present.

### Cargo test

libtest output is parsed into failing tests: the `---- name stdout ----`
blocks, the `panicked at file:line:col` location, the
`` assertion `left == right` failed `` left/right snippets, the `failures:` name
list, and the `test result: FAILED. N passed; M failed` summary. Bare panics
(`thread '...' panicked at ...`) are recognized too. Failures classify into
`TestAssertionFailure`, `TestPanic`, `SnapshotMismatch`, ...

### Shell / command-level

Timeouts, `command not found`, and `permission denied` are recognized as
command-level failures (`Timeout`, `CommandNotFound`, `PermissionDenied`).

## Fingerprints and repeated-failure detection

A `DiagnosticFingerprint` combines the normalized command, the primary kind,
the primary rustc code, the primary file (relativized to the cwd), and a hash
of the normalized primary message. Normalization strips ANSI codes, collapses
whitespace, and replaces temp-dir paths, hex addresses, and volatile numbers so
that the *same* failure across attempts — even in a different temp directory —
hashes identically, while a *different* failure does not.

`FailureHistory::record` returns `NewFailure` or `Repeated { count }`, which the
repair loop uses to detect thrashing.

## Compaction

`compact()` renders a high-signal, model-facing summary while the raw logs stay
in artifacts. It keeps the primary error, the first few warnings (only when
there are no errors), all failing tests up to a limit, assertion left/right
snippets, rustc help/note text, suggested fixes, and a tail of raw output. It
drops successful-test spam, dependency build noise, duplicate diagnostics, and
long backtraces. Compaction is deterministic and never drops the primary
failure. Limits come from the `[diagnostics]` config section.

## Command classification

`classify_command()` labels a shell command with a set of intents
(`Validation`, `FocusedValidation`, `Formatting`, `Inspection`, `Build`,
`Test`, `Mutation`, `Destructive`, `Network`, ...). It is conservative and
lexical — it does **not** replace the permission policy, it augments it. The
repair loop uses it to decide which command outputs to parse for diagnostics
and which commands count as validation-after-mutation.

`cargo test` → `Validation + Test`; `cargo test parser::foo` adds
`FocusedValidation`; `cargo fmt --check` → `Validation + Formatting`; bare
`cargo fmt` → `Formatting + Mutation`; `rm -rf` → `Destructive`;
`git reset --hard` → `Mutation + Destructive`; `curl` → `Network`.

## CLI

```text
crom diagnostics parse       <log-file>   # full structured diagnostic (JSON)
crom diagnostics summarize   <log-file>   # compact model-facing summary
crom diagnostics fingerprint <log-file>   # stable failure fingerprint
```

These operate on a captured stdout/stderr log file (use `-` for stdin) without
an agent session, so they are handy for debugging the parser or scripting.
Pass `--command`, `--exit-code`, `--timed-out`, and `--cwd` to refine
classification and the fingerprint.

```console
$ cargo test 2>&1 | crom diagnostics fingerprint - --command "cargo test"
kind:        type_mismatch
code:        E0308
file:        src/config.rs
fingerprint: 62972b89
```

## Limitations

- Plain-text parsing recognizes the common rustc/libtest shapes, not every
  possible message. Prefer the Cargo JSON path for completeness.
- Classification from message heuristics is best-effort; unknown shapes are
  `Unknown`, never fabricated.
- Package/crate inference for focused validation is shallow and best-effort.

See also [repair-loop.md](repair-loop.md).
