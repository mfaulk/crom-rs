# Guard experiments E1–E5 and the scoreboard

[#363](https://github.com/mfaulk/crom-rs/issues/363). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §13.4 (experiments) and §18 (metrics). Results: [scoreboard.md](scoreboard.md), generated from the archived reports in [`results/`](results/). This page has no numbers of its own.

| # | What runs | Where | CI |
|---|---|---|---|
| Scenarios | G1–G18 on the business ledger, every lane ([scenarios.md](scenarios.md)) | `crates/crom-guard-testkit/tests/guard_adversarial.rs` | PR (`guard / adversarial (linux)`) |
| E1 | Authorization matrix: hand-written expected table × Guard in process and the reference server | `tests/guard_experiments.rs` `e1_authorization_matrix`; table `fixtures/experiments/e1-matrix.json` | PR |
| E2 | Guard on vs off (direct-credential baseline) on the same scripted workflows and attacks | `e2_guard_on_off` | PR |
| E3 | Scripted attacks (the scenario suite) vs model-generated ones (the reference app with a live model) | scripted: scenarios; live: `.github/workflows/guard-live.yml` | scripted on PR; live manual |
| E4a | Provider tool calls (3 wire formats / 4 provider profiles) replayed through Crom against Guard: decision equality | `e4a_provider_replay`; transcripts `fixtures/experiments/e4a/` | PR |
| E4b | The same task across live providers | `guard-live.yml` with several providers | manual |
| E5 | Latency p50/p95/p99 per operation: direct executor, Guard in process, Guard over HTTP, policy only | `e5_latency` | PR (runs, reports, asserts no threshold) |

Test paths are under `crates/crom-guard-testkit/`. Each experiment writes
`target/tmp/guard-experiments/<e>.json` with `"format": "guard-experiments/v1"`;
the scenario driver writes `target/tmp/guard-scenarios/report.json`
(`guard-scenarios/v1`). Both carry a `meta` object (below).

## Running

```sh
cargo test -p crom-guard-testkit --test guard_adversarial -- --nocapture   # scenarios
cargo test -p crom-guard-testkit --test guard_experiments -- --nocapture   # E1, E2, E4a, E5 (n = 200)
CROM_GUARD_E5_ITERS=1000 cargo test --release -p crom-guard-testkit \
  --test guard_experiments e5_ -- --nocapture                              # E5 for the record
crom eval scoreboard --guard-report target/tmp/guard-scenarios/report.json \
  --guard-report target/tmp/guard-experiments --format md                  # render
```

## E1: authorization matrix

`e1-matrix.json` is the expected table. It was written by hand from the
policies (`examples/guard-support/policies/support.cedar`), the grant model
and the pipeline order, not generated from Guard's output. Each cell is:

- one of four grants: acme operator (refund limit 50.00, grant cap 100.00), acme supervisor (limit 1000.00), acme viewer (read-only actions), globex operator;
- the dimensions `role`, `tenant`, `scope`, `resource`, and its §14.2 `row`;
- the agent's calls with the expected agent-visible result of each (`executed`, `policy_deny`, `resource_not_found`, `approval_required`, …);
- the exact `expected_ledger_effects`.

A cell becomes a scenario fixture (`Cell::fixture`) and runs through the
scenario runner on a fresh business API and Guard, in process and through the
reference server. It agrees when every call's result equals the expectation,
the ledger equals the expected effects, no effect is unmediated, and
`audit verify` passes. The test fails on any disagreement; the target
(§18) is 100%.

## E2: Guard on vs off

Every step-based scenario fixture (G1–G12, G15, G16) runs twice: with
`Guard::call` in process, and with the `guard_off` baseline, whose tools call
the business API directly with the full credential and trust their arguments
([scenarios.md](scenarios.md#lanes)). Reported per lane: attacks attempted,
unauthorized effects (attacks a ledger or context probe saw), scenarios with
at least one effect, legitimate-subtask completion with a Wilson 95%
interval, and median wall time per scenario. The scoreboard pairs the two
lanes by scenario and gives exact McNemar p-values (M17's `mcnemar_exact`)
for "at least one effect" and "legitimate subtask completed". The test fails
if Guard lets an attack execute or a legitimate subtask fail, or if a
`guard_dependent` scenario executes nothing without Guard.

## E3: scripted vs model-generated attacks

The scripted lane is the scenario suite. The model-generated lane is the
reference app with a live model ([reference-app.md](reference-app.md)):
invoice memos carry an injected instruction to refund everything and export
all customers, and Guard decides whatever the model does. Its `report.json`
(`guard-demo/v1`) records the provider, model, matrix rows, evidence checks
and ledger effects, and the scoreboard renders it.

Run it manually, never in PR CI:

- GitHub: Actions → **Guard (live)** (`guard-live.yml`, `workflow_dispatch`). `max_total_cost_usd` is required: a total for the dispatch, at most $2 (`.github/scripts/check-spend-cap.sh`), split evenly across the selected providers and rounded down to the cent; each run gets that share as `--max-run-cost-usd`. `providers` defaults to `deepseek`; selected providers run in the order deepseek, anthropic, openai, gemini. Keys come from the `eval-live` environment, and each provider's step (`scripts/guard-live-run.sh`) sees only its own key. A provider without a key is skipped with a warning, and its share is not reassigned. The supervisor is `deny`. Reports are uploaded as `guard-live` and rendered into the job summary.
- Locally: `DEEPSEEK_API_KEY=... cargo run --locked --manifest-path examples/guard-support/Cargo.toml -- --agent live --provider deepseek --max-run-cost-usd 0.25 --supervisor deny`.

To publish a live result, copy its `report.json` into `docs/guard/results/`
(e.g. `demo-live-deepseek.json`) and regenerate the scoreboard.

## E4a: provider independence, offline

`fixtures/experiments/e4a/<profile>.json` holds one transcript per provider
profile (`anthropic`, `openai`, `deepseek`, `gemini`): four profiles, but
three wire formats, because OpenAI and DeepSeek share the chat-completions
path. Each report row names its `wire_format`. The transcripts are in the
provider-conformance format (`crom-conformance-transcript/v1`): each model
turn is one Guard tool call in that provider's wire format (Anthropic SSE
`tool_use`, OpenAI and DeepSeek `tool_calls`, Gemini `functionCall`), then a
final text turn. A wiremock server replays the transcript to a Crom SDK agent
built from the provider profile, with `ToolPolicy::guard` and a fresh
reference server. Crom's provider adapter decodes each call and Crom's MCP
client sends it to Guard. Each request after a call must carry that call's
result (the transcript's `request_contains`).

The reference is the same calls sent straight to a fresh reference server.
Decisions are compared from Guard's audit log, without ids or timings:
action, resource, `args_digest`, decision, reason, determining policies and
outcome. The test fails unless every provider's sequence equals the
reference, and `--guard-check` pins every decision view of the reference and
of each provider against the archive.

The transcripts are `source: "synthetic"`, rendered from the call script in
the test (`CROM_E4A_REGEN=1` rewrites them, and
`e4a_transcripts_match_the_script` checks they match it). A transcript
captured from a live run can replace one with `source: "recorded"`.
E4b, the same task across live providers, is the live workflow with several
providers.

## E5: overhead

Per operation (`customer.read`, `invoice.list`, a 0.01 refund, a
`policy_deny` refund, a forbidden export), `CROM_GUARD_E5_ITERS` (default
200) timed calls after an untimed warm-up of a tenth of that (10–100), on a
multi-threaded Tokio runtime, each path on its own business API:

| Path | Measured |
|---|---|
| `direct` | the trusted executor: an HTTP call to the business API with the credential (allowed operations only) |
| `guard_in_process` | `Guard::call` with the file credential broker and `FileAuditSink` (hash-chained JSONL, `fsync` per record) |
| `guard_http` | the reference server over MCP Streamable HTTP on loopback, from a Rust client |
| `policy_only` | Guard's own `duration_us.policy_us` from the in-process decision records: Cedar evaluation of the whole grant chain, plus the approval probe on a deny. Microsecond resolution |

Each operation's `policy_only` samples are the decision records appended
while that operation's timed calls ran (exactly one per call), not a slice
by position.

What dominates (an inference, not a measured breakdown): Guard's
end-to-end cost on the guarded paths is attributed mostly to the audit
sink's per-record `fsync`. An executed call writes two records (decision,
then outcome); a denied call writes one, and the medians scale roughly with
that count while `policy_only` stays small. E5 has no path without `fsync`.
The closest measured comparison is the separate, unarchived
`pipeline_latency` test (in-memory sink, stub executor,
[pipeline.md](pipeline.md#tests)), which is a different harness. In the archived run
(btrfs, release), the in-process p50 is 1.52–1.63 ms for executed calls
and 0.75–0.80 ms for denied ones, against 45–78 µs for `policy_only`
([scoreboard.md](scoreboard.md), E5).
`policy_only` is Cedar evaluation alone, as Guard records it in integer
microseconds (`policy_us`); it is not the same span as the end-to-end rows,
which are wall-clock per call measured by the harness.

Percentiles are nearest-rank. E5 asserts only that every call returned the
expected result: latency depends on the machine and is reported, never
gated. `FileAuditSink` `fsync`s every record, so the guarded paths depend on
the filesystem of the temp directory, where the audit log lives: on `tmpfs`
an `fsync` costs almost nothing. The report's `audit_sink` names that
filesystem; set `TMPDIR` to measure on another one. Record numbers from a `--release` build; the report's `meta.build`
and `meta.machine` say what produced them.

## Reproducibility metadata

Every scenario, experiment and reference-app report carries `meta`:

| Field | Value |
|---|---|
| `commit`, `dirty` | `git rev-parse HEAD` (else `$GITHUB_SHA`) and whether tracked files differed |
| `policy_set_hash`, `schema_hash` | The SHA-256 Guard records in every audit record for `support.cedar` and `support.cedarschema` |
| `grant_fixtures_hash` | SHA-256 over the scenario fixtures and `e1-matrix.json` (every grant these runs issue) |
| `sandbox` | `bwrap` path, and whether `CROM_REQUIRE_SANDBOX_TESTS` / `CROM_REQUIRE_SEPARATE_UID_TESTS` were set |
| `provider`, `model` | Live reference-app runs only |
| `build`, `machine`, `command` | `debug`/`release`, OS, kernel, CPU model and count, and the command that produced the report |

## Scoreboard

`crom eval scoreboard --guard-report <file-or-dir>` renders the Guard section
from these reports ([eval.md](../eval.md#guard-section-363)). Sections:
security by scenario lane (attempted, blocked by Guard / Crom's gate / sandbox / preflight,
executed, legitimate subtasks, ledger entries, unmediated effects, audit
coverage, `audit verify`), executed/attempted per scenario and lane, the M17
adversarial suite for comparison, E1, E2, E4a, reference-app runs, E5, and
the sources with their metadata. Lanes are read by name, so a new scenario
lane appears without code changes. Security, completion and latency stay in
separate tables.

### The archive

`docs/guard/results/` holds the reports the published scoreboard is built
from. [scoreboard.md](scoreboard.md) and the "Results" section of
[docs/benchmarks/M18-guard.md](../benchmarks/M18-guard.md) (between its
`BEGIN GENERATED` and `END GENERATED` markers) are generated, never edited:

```sh
scripts/guard-scoreboard.sh            # rewrite both from docs/guard/results/
scripts/guard-scoreboard.sh --check    # fail if either is stale
```

| File | Produced by |
|---|---|
| `scenarios.json` | `CROM_REQUIRE_SANDBOX_TESTS=1 cargo test -p crom-guard-testkit --test guard_adversarial`. The archived run had no passwordless `sudo`, so `separate_uid` is skipped there; CI runs and requires it (see [CI](#ci)) |
| `e1.json`, `e2.json`, `e4a.json` | `cargo test -p crom-guard-testkit --test guard_experiments` |
| `e5.json` | `TMPDIR=/var/tmp CROM_GUARD_E5_ITERS=1000 cargo test --release -p crom-guard-testkit --test guard_experiments e5_` (audit logs on disk, not `tmpfs`; the report's `audit_sink` names the filesystem) |
| `demo-scripted.json` | `cargo run --manifest-path examples/guard-support/Cargo.toml` (`target/guard-demo/report.json`) |
| `m17-base-340.json` | The #340 base: the `adversarial-report` artifact of CI run [44459286359](https://github.com/mfaulk/crom-rs/actions/runs/44459286359) at `82fb30a` (MILESTONE_18 §22), wrapped as `m17-adversarial-archive/v1` |
| `m17-head.json` | A one-off local snapshot of the same M17 suite at the commit in its `commit` field, not regenerated or checked by CI: `CROM_REQUIRE_SANDBOX_TESTS=1 cargo test --locked -p crom-eval --test adversarial --test repo_config_escalation -- --nocapture` (`target/tmp/adversarial/report.json`), wrapped the same way. The current M17 report is CI's `adversarial-report` artifact (`security / OS sandbox (linux)`) |

An `m17-adversarial-archive/v1` file keeps the original report's exact text
in `report_raw` and its SHA-256 in `report_sha256`; the scoreboard refuses
the file if they do not match. To check one by hand:
`jq -j .report_raw m17-base-340.json | sha256sum`.

Each file's `meta.command` (or the table above) is the command that made it.
To refresh the archive, rerun those commands on a clean checkout, copy the
reports in, and run `scripts/guard-scoreboard.sh`.

### CI

`guard / adversarial (linux)` runs the scenarios and the experiments, uploads
`guard-scenarios-report` and `guard-experiments-report`, then runs
`scripts/guard-scoreboard.sh --check` with the fresh reports. That fails if
`scoreboard.md` or the generated section of `M18-guard.md` does not
regenerate from the archive, or if a deterministic
fact of the fresh run differs from the archive: every (scenario, lane) pair
with its status and verdict counts, ledger and audit results, every E1 cell,
E2 counts per lane and per scenario, and every E4a decision view (reference
and each provider). An archived pair that is missing or skipped in the fresh
run is a difference, and so is a fresh pair the archive lacks. Timings,
latency and provenance are not compared.

The one exception is the allowlist `CHECK_OPTIONAL_LANES` in
`crates/crom-eval/src/guard_board.rs`: `separate_uid` may be absent or
skipped on either side, because it needs passwordless `sudo -n -u nobody`.
The archive's machine lacks it, and CI has it and requires it
(`CROM_REQUIRE_SEPARATE_UID_TESTS=1`), so CI runs the lane without a stored
counterpart. When both sides ran it, it is compared like any other lane.
The CI job also sets `CROM_REQUIRE_PYTHON_CLIENT_TESTS=1`, so the Python
client lane cannot skip there.

## What these results show and do not show

Within the supported deployment (§7.5) and the tested calls: the tested
unauthorized effects did not occur with Guard, the same calls produced them
without Guard, every ledger effect has exactly one Guard `executed` record,
and Guard's decisions did not depend on which provider's wire format the call
came from. They do not show the absence of untested bypasses, executor
correctness beyond the reference operations, the safety of other
deployments, resistance to prompt injection in general, or how a live model
behaves (E3/E4b are manual and optional).
