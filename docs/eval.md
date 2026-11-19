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
- **Scoreboard** (`eval scoreboard`, M17.4): cross-run capability, efficiency,
  reliability, and security tables; see [Scoreboard](#scoreboard-m174). With
  `--guard-report`, also the Crom Guard section (#363).

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
$ crom eval run --suite <suite> --provider deepseek --max-cost-usd 0.25 --max-run-cost-usd 5
$ crom eval run --suite <suite> --pricing-file my-prices.toml
$ crom eval validate-task <id-or-path>
$ crom eval validate-suite <suite> --repeat 5
$ CROM_EVAL_PODMAN=1 crom eval validate-suite <suite> --isolation podman
$ crom eval history
$ crom eval trend <suite>
$ crom eval regressions --baseline <run-or-sha> --candidate <run-or-sha>   # index-only diff
$ crom eval regressions --since main [--candidate <run-id>]                  # paired, see "Comparing runs"
$ crom eval task-history <task-id>
$ crom eval triage <run-id>
$ crom eval show <run-id> <task-id>
$ crom eval open <run-id> <task-id>
$ crom eval scoreboard [--since <rev>] [--model <m>]... [--level <L>]... [--format text|md|json] [--guard-report <path>]...
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
$ CROM_EVAL_LIVE=1 DEEPSEEK_API_KEY=sk-... \
    crom eval run --suite rust-small --provider deepseek --model deepseek-chat
```

See [Live providers](#live-providers) for every profile and its key.

Report and compare prior runs:

```console
$ crom eval report <run-id>
$ crom eval compare <baseline-run-id> <candidate-run-id> [--experiment <preset>]
```

## Provider modes

The `--provider` flag selects how the agent is driven for the whole run:

| mode        | description |
|-------------|-------------|
| `fake`      | A trivial echo provider. Proves the harness plumbing (workspace setup, event capture, oracles, scoring, artifacts) without proving model intelligence. Offline and deterministic. |
| `golden`    | A scripted provider that replays a canned sequence of tool calls for a specific task (read → edit → run check → observe failure → edit → pass). Catches regressions in tool schemas, event handling, sessions, and command execution. |
| any provider profile id | A real model provider resolved from a provider profile (`anthropic`, `openai`, `deepseek`, `gemini`, `local`, or an id defined under `providers` in user/project settings; see [providers.md](providers.md)). Any other value is rejected with `unknown provider`. **Opt-in only**; see [Live providers](#live-providers). Never runs in ordinary CI. |

### Live providers

Live runs use the same provider-profile resolver as `crom run` and the SDK,
so a profile id (and any `providers` settings override, such as a
`base_url`) means the same endpoint everywhere. One invocation runs one
provider; compare providers with separate runs and `crom eval compare`.

| `--provider` | API-key env var | Default model (`providers.toml`) |
|---|---|---|
| `anthropic` | `ANTHROPIC_API_KEY` | `claude-sonnet-4-6` |
| `openai` | `OPENAI_API_KEY` | `gpt-4.1` |
| `deepseek` | `DEEPSEEK_API_KEY` | `deepseek-chat` |
| `gemini` | `GEMINI_API_KEY` | `gemini-2.5-pro` |
| `local` | `LOCAL_OPENAI_API_KEY` (any placeholder) | none — pass `--model` |

Model precedence: `--model`, then `CROM_EVAL_MODEL`, then the profile's
`default_model`. The profile file is the only place a default model is
defined. `--model` is rejected for `fake`/`golden`.

A live task is **skipped**, never failed, when it cannot run. Its
`skipped_reason` starts with a stable code:

| Code | Cause |
|---|---|
| `live_disabled` | `CROM_EVAL_LIVE=1` is not set. |
| `credentials_unavailable` | The profile's API-key env var is unset or empty. |
| `model_unavailable` | No `--model`, no `CROM_EVAL_MODEL`, and no profile default. |

The provider profile, model, and endpoint host (never the key or full URL)
are recorded in `run.json`, `provider.json`, and each result's
`repro.provider` block.

`crom eval run` exit codes:

| Code | Meaning |
|---|---|
| `0` | Every non-skipped, non-quarantined task resolved (or `--allow-failures`). |
| `1` | At least one task ran and did not resolve. |
| `3` | Every task was skipped — nothing ran (e.g. missing credentials). `--allow-failures` does not mask this. |

Offline coverage: `crates/crom-cli/tests/eval_live_cli.rs` points each of
the `anthropic`, `openai`, `deepseek`, and `gemini` profiles at a wiremock
server speaking that provider's wire format and runs a task end to end
through `crom eval run`; it also asserts the skip codes and exit codes.

### Cost and spend caps

Each `result.json` records `cost_usd`: nominal list-price cost from the
versioned table in `crates/crom-core/pricing/prices.toml`. It is `null`,
with a `cost_unknown_reason`, when the model has no pricing entry or usage
was not reported. It is never zero-filled. `summary.json` adds
`total_cost_usd`. See [cost semantics](providers.md#cost-semantics).

- `--max-cost-usd <usd>`: per-task cap across all attempts (a task's
  `limits.max_cost_usd` also applies; the tighter wins). A breach stops the
  agent between turns, and the task fails as `budget_exceeded`.
- `--max-run-cost-usd <usd>`: cap for the whole run, across repetitions.
  Once spend reaches it, the remaining tasks are recorded as `skipped` with
  a `budget:` reason and counted in `summary.json` `budget_skipped`.
- `--pricing-file <path>` (or `CROM_PRICING_FILE`): merge your own prices
  over the built-in table.

Caps fail closed. With a cap set, an unpriced model or unreported usage
stops the run instead of letting it continue unmetered. `plan_then_act`
tasks are skipped under a cost cap, because plan modes do not meter usage.

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
- `adversarial` — fixtures of the adversarial security suite (#292),
  `kind = "security"`. Scored on host effects by
  `cargo test -p crom-eval --test adversarial`, not by `eval run`; see
  [security/adversarial-suite.md](security/adversarial-suite.md).
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
- `l3` — L3 capability corpus (#289): ten upstream Rust repositories
  (2k–30k LoC) pinned in a fixtures repository, each at the parent of a real
  bug fix whose tests are the hidden oracle. Intended for a live provider; see
  [L3 corpus](#l3-corpus-289).
- `l3-mini` — ten synthetic, std-only three-crate workspaces stored in-tree;
  fully offline. See [L3-mini corpus](#l3-mini-corpus-m172).
- `provider-conformance` — six L0 harness tasks (#284) that exercise
  multi-turn tool use per provider. Replayed offline against recorded
  transcripts in `cargo test`; run live with any profile. See
  [Provider conformance](#provider-conformance-284).
- `l4` — L4 corpus (#296): five real, closed upstream issues frozen before
  any baseline run. The prompt is the issue text; fixtures are pinned in the
  same fixtures repository as `l3`. See [L4 corpus](#l4-corpus-296).

### Provider conformance (#284)

A provider handshake does not prove reliable multi-turn tool use. Each task
targets one behavior that differs across wire formats:

| Task | Exercises |
|---|---|
| `conf-read-edit-verify` | Read → Edit → Bash → final answer; tool-call ids round-tripped. |
| `conf-tool-error-recovery` | A failing Read (`is_error`; `ERROR:` prefix on OpenAI-compatible wires) the model must recover from. |
| `conf-parallel-tools` | Three independent reads in one turn (an opportunity, not a requirement). |
| `conf-bash-check-loop` | A failing command, a fix, and a passing rerun. |
| `conf-invalid-args-recovery` | Tool arguments that fail to parse (Gemini: schema-invalid) return an error result and the loop recovers. Live models rarely trigger this; the transcripts inject it. |
| `conf-long-output` | Output truncated at the token limit (`max_tokens` / `length` / `MAX_TOKENS`); the agent drops the incomplete call and continues. |

Offline: `crates/crom-eval/tests/provider_conformance.rs` replays every task
for `anthropic`, `openai`, `deepseek`, and `gemini` against
`crates/crom-eval/tests/fixtures/conformance/<profile>/<task>.json`. Each
exchange also checks the request the agent sent: the model, the profile's
max-tokens field, the tool-result id for every prior call, error results,
and that a truncated call is never sent back. The committed transcripts are
`source = "synthetic"`, rendered from provider-neutral scripts in that test;
regenerate with
`CROM_CONFORMANCE_REGEN=1 cargo test -p crom-eval --test provider_conformance`.

Live (opt-in, costs tokens):

```console
$ CROM_EVAL_LIVE=1 OPENAI_API_KEY=... \
    crom eval run --suite provider-conformance --provider openai --allow-failures
```

Classify each live failure from its `result.json` `outcome` and
`failure_class`:

| Outcome / failure class | Class |
|---|---|
| `provider_error` | Provider error (HTTP, auth, rate limit, malformed response). |
| `infra_error` (`harness_error`, `isolation_error`, `adapter_error`) | Crom bug until shown otherwise. |
| `failed`, run ended `model.output_truncated` or wrong tool use | Capability limitation, after checking `events.jsonl` that Crom sent well-formed requests. |
| `failed` with a malformed request or a dropped tool result in `events.jsonl` | Crom bug. |

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

### L3 corpus (#289)

Each `l3` task is a real, permissively licensed Rust repository (2k–30k lines
of Rust) snapshotted at the parent of an upstream bug-fix commit. The agent
gets the snapshot and a bug report; grading reruns the tests that the upstream
fix added, which the agent never sees. Tasks are `kind = "capability"`,
`level = "L3"`, `network = "none"`.

| Task | Upstream | Rust LoC | `vendor/` | Bug |
|---|---|---|---|---|
| `l3-bytes-try-get-int-sign` | [tokio-rs/bytes](https://github.com/tokio-rs/bytes) | 13,787 | 37M | Sign-extend n-byte integers in the fallible Buf getters |
| `l3-http-insert-mult` | [hyperium/http](https://github.com/hyperium/http) | 18,408 | 22M | Fix HeaderMap corruption when replacing a header that has 3+ values |
| `l3-indexmap-macro-hygiene` | [indexmap-rs/indexmap](https://github.com/indexmap-rs/indexmap) | 15,798 | 19M | Stop indexmap!/indexset! macros from shadowing a caller's CAP constant |
| `l3-itertools-combinations-zero` | [rust-itertools/itertools](https://github.com/rust-itertools/itertools) | 12,266 | 17M | Fix zero-length combinations in itertools |
| `l3-nom-multispace-eof` | [rust-bakery/nom](https://github.com/rust-bakery/nom) | 27,455 | 45M | Fix the remaining input returned by complete &str parsers that consume everything |
| `l3-petgraph-floyd-multiedge` | [petgraph/petgraph](https://github.com/petgraph/petgraph) | 28,700 | 31M | Fix Floyd-Warshall distances on graphs with parallel edges in petgraph |
| `l3-ron-integer-suffix-base` | [ron-rs/ron](https://github.com/ron-rs/ron) | 21,757 | 12M | Parse octal and hex RON integers that carry a type suffix |
| `l3-serde-yaml-duplicate-keys` | [dtolnay/serde-yaml](https://github.com/dtolnay/serde-yaml) | 10,970 | 7.5M | Reject duplicate mapping keys when deserializing serde_yaml::Value |
| `l3-toml-crlf-error-position` | [toml-rs/toml-rs](https://github.com/toml-rs/toml-rs) | 11,583 | 7.0M | Fix wrong error line/column for CRLF input in the toml crate |
| `l3-url-path-caret` | [servo/rust-url](https://github.com/servo/rust-url) | 14,728 | 33M | Percent-encode the caret in URL paths per the WHATWG URL Standard |

Storage: snapshots live in the public fixtures repository
[`mfaulk/crom-eval-fixtures`](https://github.com/mfaulk/crom-eval-fixtures),
one directory per task, all at one commit. Each task pins it with
`[fixture] kind = "git"`, the repository URL, the full commit SHA as `rev`, and
`path = "<task-id>"`. crom fetches that commit once into
`$CROM_EVAL_FIXTURE_CACHE` (default `~/.cache/crom/eval-fixtures/<rev>/`) and
reuses it offline afterwards. Only `task.toml`, `hidden/`, and `oracle.patch`
are in this repository. `[provenance]` records `upstream`, `base_commit` (the
snapshot), and `fix_commit` (the source of the hidden tests and the reference).

Snapshots: the tree at `base_commit` without `.git`, plus `vendor/` (from
`cargo vendor --versioned-dirs`), a `.cargo/config.toml` source replacement,
and a generated `Cargo.lock` when upstream had none. To keep the repository
small, vendored crates that never compile on x86_64 Linux (Windows, wasm,
redox, and similar targets) are replaced by stubs that keep their
`Cargo.toml`; those snapshots therefore build only on x86_64 Linux.
`l3-url-path-caret` pins four ICU crates to the releases current at its base
commit, and `l3-serde-yaml-duplicate-keys` allows the
`dangerous_implicit_autorefs` lint in `.cargo/config.toml` so the old
`unsafe-libyaml` code compiles on current Rust.

Verification:

- Public oracle: the upstream test suite with `--offline` (a subset where
  upstream tests fail on current Rust for unrelated reasons; see each
  `task.toml`).
- Hidden oracle: the integration-test files the fix commit added or changed,
  overlaid from `hidden/` after the agent finishes.
- Two prompts ask the agent to update a public test file that encodes the old
  behavior (`tests/test_std.rs` in itertools, `url/tests/expected_failures.txt`
  in rust-url); their `oracle.patch` includes the upstream change to it.
- `Cargo.toml` and `Cargo.lock` are in every task's `forbidden_paths`.

```console
crom eval validate-suite l3                      # fetches the fixtures commit once
CROM_EVAL_PODMAN=1 crom eval validate-suite l3 --isolation podman \
  --container-image docker.io/library/rust:1-slim   # build untrusted code in a container
crom eval run --suite l3 --provider <profile>
```

CI validates `l3` in a separate job with the fixture cache restored from
`actions/cache`. `crates/crom-eval/tests/l3_corpus.rs` checks structure: git
fixtures pinned by full SHA to one fixtures commit, offline commands,
upstream provenance, permissive licenses, and Cargo manifests forbidden.

On a 64-core host, cap build parallelism inside a container that limits PIDs
(`CARGO_BUILD_JOBS=8`, `--test-threads 8`); otherwise `rust-lld` aborts with
`Resource temporarily unavailable`.

### L4 corpus (#296)

Five `kind = "capability"`, `level = "L4"` tasks. Each is a real, closed issue
in a permissively licensed Rust repository, snapshotted at the parent of the
commit that resolved it, with dependencies vendored. The prompt is the issue
text as filed. The hidden oracle is the fix's tests (an owner-written
acceptance test for `l4-tempfile-relative-temppath`), and `oracle.patch` is
the fix's non-test diff. `[provenance] source` is the issue URL.

| Task | Issue | Area |
|---|---|---|
| `l4-which-relative-path-cwd` | harryfei/which-rs#127 | tooling: executable lookup |
| `l4-tempfile-relative-temppath` | Stebalien/tempfile#395 | security: cleanup deletes the wrong file |
| `l4-quick-xml-dtd-chunk-panic` | tafia/quick-xml#960 | untrusted input: parser panic |
| `l4-toml-writer-quote-overflow` | toml-rs/toml#1188 | serializer panic in a workspace |
| `l4-glob-hidden-dir-leading-dot` | rust-lang/glob#67 | tooling: file matching |

The selection criteria, provenance, rejected candidates, freeze date, and
owner sign-off are in [eval/real-world-tasks.md](eval/real-world-tasks.md).
`crates/crom-eval/tests/l4_corpus.rs` checks structure, and CI validates the
suite in the `eval / L4 real-world issues` job.

```console
crom eval validate-suite l4
CROM_EVAL_PODMAN=1 crom eval validate-suite l4 --isolation podman \
  --container-image docker.io/library/rust:1-slim
crom eval run --suite l4 --provider <profile>
```

### L3-mini corpus (M17.2)

The `l3-mini` suite is ten synthetic three-crate workspaces (~4.1k lines in
total, `provenance.source = "original"`, `Apache-2.0 OR MIT`) stored in-tree
under `repo/`. They depend only on `std` and on each other through path
dependencies, so `crom eval validate-suite l3-mini` runs fully offline with no
fixture fetch; CI runs it with the other offline suites.

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

Public verification is `cargo test --workspace --offline`; hidden
`acceptance_*.rs` tests are overlaid from `hidden/crates/<crate>/tests/`.
`l3_corpus.rs` caps each task at 96 KB and checks that every dependency is a
path dependency.

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

## Scoreboard (M17.4)

`crom eval scoreboard` aggregates every `result.json` under the runs directory
(including `rep-<n>/` repetitions) into one cross-run view. Text, Markdown,
and JSON come from the same aggregation.

```console
crom eval scoreboard                                  # every run, grouped by provider/model
crom eval scoreboard --since main --format md         # results from main and commits after it
crom eval scoreboard --model deepseek-chat --level L1 --level L2
crom eval scoreboard --group-by commit --format json
crom eval scoreboard --runs-dir path/to/runs --run <run-id> --run <run-id>
```

| Flag | Meaning |
|---|---|
| `--runs-dir` | Root holding run directories (default `target/crom-eval/runs`). |
| `--run` | Only these run ids (repeatable). |
| `--since <rev>` | Only results whose `repro.git.sha` is `<rev>` or in `git rev-list <rev>..HEAD`. Results without a SHA are dropped. |
| `--model` | Only this `model` or `provider/model` (repeatable). |
| `--level` | Only tasks at this level, `L0`..`L5` (repeatable). |
| `--group-by` | `model` (default; `provider/model`) or `commit` (12-char SHA, `+dirty` for a dirty checkout). |
| `--format` | `text` (default), `md`, or `json` (`schema_version: 1`). |
| `--guard-report` | A Guard report or a directory of them (repeatable); see [Guard section](#guard-section-363). |
| `--guard-check <dir>` | Compare the deterministic Guard facts of `--guard-report` with the archive in `<dir>`. |

Each group has these sections. Tasks are separated by v3 `kind`, so harness
and security results never enter capability numbers.

- **Capability** (`kind = capability`), overall, by level, and by category:
  - *resolved/n* and *rate* = `resolved / (resolved + failed)`, with a Wilson
    95% interval. `provider_error`, `infra_error`, `skipped`, and `unscorable`
    stay out of the denominator and are listed under *excluded*.
  - *pass@1* = mean over tasks of `c_i / n_i`, where `n_i` is a task's scored
    attempts and `c_i` its resolved ones.
  - *pass@k* = mean of the unbiased estimator `1 − C(n−c, k) / C(n, k)`
    (Chen et al.), with `k` set to the smallest `n_i` in the row. It is shown
    only when every task in the row has `n_i ≥ 2`, and the output gives `k`
    and the range of `n`.
- **Harness and unclassified**: `kind = harness` (L0) tasks, and results from
  v1/v2 tasks with no `kind`. These rows use the same columns but are not
  capability results.
- **Efficiency** (capability attempts, skipped excluded):
  - *total spend* = sum of known `cost_usd` over every attempt, failed and
    errored ones included.
  - *cost / resolved* = total spend ÷ resolved count. When some attempts have
    no price, the output says how many were priced and the figure is a lower
    bound. Until the pricing work (#282) lands, `cost_usd` is always `null` and
    cost shows as `unknown`.
  - Also: cost per attempt, median wall time, mean tool calls, mean model
    round trips, token totals (input, output, cache read/write, reasoning)
    over attempts that reported usage, and cache hit rate
    `cache_read / (input + cache_read)`.
- **Reliability** (every non-skipped attempt, all kinds):
  - timeouts, provider errors, infra errors, unscorable.
  - `tool_loop` failures. Results do not record individual tool-call errors.
  - *flaky / repeated tasks*: tasks with ≥2 scored attempts that both
    resolved and failed.
  - *mean task variance*: mean `p(1−p)` over repeated tasks.
  - repair recovery: attempts that entered the repair loop and later passed.
  - repeated-failure stops: same error repeated, or the repair attempt limit
    was hit.
  - unvalidated completions: the attempt finished after a mutation without
    validating.
  - the *unreliable* flag: provider + infra errors above 10% of attempts.
- **Security** (`kind = security`): a separate table, never folded into a
  weighted score. It shows the resolved rate with a Wilson interval, plus
  attempted, blocked, and executed tool calls and audit coverage summed from
  each attempt's `security-summary.json`.

### Guard section (#363)

`--guard-report <file-or-dir>` (repeatable) adds a Guard section built from
archived Guard reports. A directory contributes its `*.json` files in name
order. With `--guard-report` and neither `--runs-dir` nor `--run`, only the
Guard section is rendered.

```console
crom eval scoreboard --guard-report docs/guard/results --format md
crom eval scoreboard --guard-report target/tmp/guard-scenarios/report.json \
  --guard-report target/tmp/guard-experiments --format json
crom eval scoreboard --guard-report target/tmp/guard-experiments/e1.json \
  --guard-check docs/guard/results     # exit 1 if a deterministic fact differs
```

| Report `format` | Source | Section |
|---|---|---|
| `guard-scenarios/v1` | `crom-guard-testkit` `tests/guard_adversarial.rs` | Security per lane and per scenario, scored on the business ledger |
| `guard-experiments/v1` (`experiment` = `E1`, `E2`, `E4a`, `E5`) | `crom-guard-testkit` `tests/guard_experiments.rs` | E1 matrix agreement and table; E2 on/off with McNemar; E4a decision equality; E5 latency |
| `guard-demo/v1` | `examples/guard-support` `report.json` (scripted or live) | Reference-app runs (E3/E4b live lane) |
| `m17-adversarial-archive/v1` | An M17 adversarial `report.json` under `report`, with `label` and `commit` | Comparison with the M17 suite (e.g. the #340 base) |

Any other format is an error. Scenario lanes are read by name from each
entry's `mode`, so a new lane needs no code change. The security table is
computed from ledger probes, not from Guard's audit log: *executed* counts
attacks a probe saw take effect, *audit coverage* is ledger entries with
exactly one Guard `executed` record over all ledger entries (Guard lanes),
and *audit verify* counts runs whose hash chain verified. Rates (legitimate
subtasks, E1 agreement, E2 completion) are k/n with Wilson 95% intervals; E2
pairs Guard on and off by scenario with the exact McNemar test. Nothing is
combined into one score with capability, completion or latency.

`--guard-check <dir>` compares the deterministic facts of `--guard-report`
with the reports in `<dir>` (verdict counts, ledger and audit results, E1
cells, E2 counts, E4a decisions; not timings, latency or provenance) and
exits non-zero on a difference. A scenario lane present on only one side is
skipped. Experiments, metadata and the archive:
[docs/guard/experiments.md](guard/experiments.md); published results:
[docs/guard/scoreboard.md](guard/scoreboard.md).

## Comparing runs (M17.4)

`crom eval compare <baseline> <candidate>` pairs two runs (run ids under
`--runs-dir`, or run directory paths) by task id and reports whether the
candidate is better, worse, or not distinguishable. `crom eval regressions
--since <ref>` does the same with the baseline taken from the index: the most
recent run evaluated at `<ref>`'s commit, otherwise the most recent run
recorded on a branch named `<ref>`. The candidate defaults to the most recent
other indexed run (override with `--candidate`). The older
`regressions --baseline A --candidate B` still prints the index-only diff.

```console
crom eval compare run-a run-b                          # refuses if configs differ
crom eval compare run-a run-b --experiment model       # model A vs B: model may differ
crom eval compare run-a run-b --allow budgets --force --format json
crom eval regressions --since main --kind all
```

**Comparability.** The first attempt of each paired task is checked on these
`repro` factors: `model` (provider profile + model), `task_version`
(`task_version` + content hash), `tool_policy`, `security` (the measured
sandbox overhead is ignored), `isolation`, `budgets` (max turns, timeout,
mode, attempts), `repair` (repair-loop settings), and `prompt` (template
version, variant, hash). Results without `repro` (v1) produce a `metadata`
mismatch. Each mismatch is printed as a metadata diff. A factor that is not
allowed makes the command exit non-zero. You allow a factor with an
experiment preset, with `--allow <factor>`, or override everything with
`--force`, which prints a warning banner.

| `--experiment` | Factor allowed to differ |
|---|---|
| `commit` | none: the code changed, the configuration must not |
| `model` | `model` |
| `security` | `security` |
| `repair` | `repair` |
| `policy` | `tool_policy` |
| `reps` | none: k repetitions of one configuration, to measure the noise floor |

**Pairing.** Only tasks of `--kind` (default `capability`; also `harness`,
`security`, `all`) are paired. For each task, `(resolved, scored)` counts
come from its `resolved`/`failed` attempts, so repetitions are pooled. Each
pair is classified:

- *newly solved*: none resolved before, all resolved after
- *newly failing*: the reverse
- *flaky*: mixed results on either side
- *still passing* / *still failing*

Tasks present on only one side, or with no scored attempt on a side, are
listed separately and left out of the statistics.

**Statistics** (`docs/milestones/MILESTONE_17.md` §8.3):

- *delta* = pooled candidate resolved rate − pooled baseline resolved rate.
- *bootstrap 95% CI*: percentile interval over 2000 resamples of tasks with
  replacement. It uses a fixed seed, so the output is reproducible.
- *McNemar exact*: two-sided binomial test on per-task majority outcomes
  (resolved in more than half of the scored attempts). `b` counts
  pass→fail pairs and `c` counts fail→pass pairs.
- Each side's rate is shown with a Wilson 95% interval and n.

**Verdict.** The verdict is `improved` or `regressed` only when all of these
hold:

- at least 20 paired tasks
- both runs reliable
- McNemar p < 0.05
- the bootstrap CI excludes 0
- the two Wilson intervals do not overlap

Otherwise it is `inconclusive`, and the output gives the reason, for example
`n too small to conclude: 10 paired tasks < 20`.

**Cost and usage.** Spend, input/output tokens, and mean tool calls per side
cover every non-skipped attempt of the paired tasks. The cost delta is shown
only when every attempt on both sides is priced; otherwise it is `unknown`.
`cost_usd` stays `null` until #282 lands.

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
      workspace/           # verification copy: the agent workspace plus the hidden
                           # overlay, archived here after verification (which runs
                           # in a temp dir outside any enclosing Cargo workspace)
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
crom eval validate-suite <every suite except private>   # l3 in its own job
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

### CI lanes and spend controls (#295)

| Lane | Workflow | Trigger | Spends tokens |
|---|---|---|---|
| 1. Offline suites, validate-suite, recorded conformance | `ci.yml` (`eval`, `eval-l3`, `eval-l4`) | push / PR | No |
| 2. Security enforcement (scripted adversarial suite, sandbox required) | `ci.yml` (`sandbox`) | push / PR | No |
| 3. Live smoke | `eval-live.yml` (`Eval (live smoke)`) | manual dispatch | Yes, cap <= $5 |
| 4. Full benchmark (provider/model matrix, repetitions) | `eval-benchmark.yml` (`Eval (benchmark)`) | manual dispatch | Yes, cap <= $50 per cell |

No workflow spends on push, PR, or schedule. Lanes 3 and 4 require a
`max_run_cost_usd` input and fail closed: `.github/scripts/check-spend-cap.sh`
rejects a missing, non-numeric, zero, or over-ceiling cap before anything
builds, and the cap is passed to `crom eval run --max-run-cost-usd`, which
stops on an unpriced model or unknown usage. `ci.yml` (`lint / workflows`)
dry-runs that gate (`.github/scripts/test-check-spend-cap.sh`) and runs
`actionlint` over every workflow.

Both live lanes run in the `eval-live` GitHub environment and read
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `DEEPSEEK_API_KEY`, and
`GEMINI_API_KEY` from it (only the chosen provider's key is needed). The
environment requires a maintainer's approval for each run and deploys only
from `main`, so a dispatch from any other branch, including one with an
edited workflow, never receives the keys. Approve a waiting run in the
Actions UI or with `gh api -X POST
repos/<owner>/<repo>/actions/runs/<id>/pending_deployments`. Workflow inputs
reach the shell only through `env:`. Every action in `.github/workflows/` is
pinned to a commit SHA, with the tag in a comment; Dependabot
(`github-actions`) updates the pins.

Both live lanes upload `target/crom-eval/runs` minus each archived
workspace's `target/` (cargo build output, ~98% of the bytes). An M17
benchmark cell was ~15 GiB with it and ~0.25 GiB without. The per-task
`result.json`, `patch.diff`, transcripts, and workspace sources are kept.

- **Lane 3** (`suite`, `provider`, `model`, `max_run_cost_usd`; defaults to
  `provider-conformance` on `deepseek` / `deepseek-flash`). Without the
  provider's key every task skips, the CLI exits 3, and the job warns
  instead of failing. The scoreboard goes to the job summary.
- **Lane 4** (`targets` as space-separated `provider:model` pairs, `suites`,
  `repetitions`, `max_run_cost_usd`, and `repair` = `disabled` (default) or
  `enabled`, passed to `crom eval run --repair`). One job per target; the cap covers all
  suites in that job, and each suite gets what the previous ones left (read
  from `summary.json` `total_cost_usd`; unknown spend stops the job). Run
  directories are uploaded per target and the scoreboard is posted to the
  job summary. It has no schedule on purpose; add one only with a fixed cap.

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
- Agent `Bash` is OS-sandboxed only with `--sandbox required`, the default
  for live providers (Linux only; other hosts fail closed). With
  `--sandbox off` a task that enables Bash can run commands that reach outside
  the fixture directory, so run such evals only against trusted tasks or in an
  externally sandboxed environment. See
  [eval isolation](eval-isolation.md#agent-sandbox).
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
