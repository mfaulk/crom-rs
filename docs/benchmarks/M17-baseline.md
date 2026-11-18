# M17 live baseline (#297)

The first live capability baseline for Crom: L1–L4 on one low-cost and one
high-capability model, 3 repetitions per task, with the OS sandbox required.
It is a starting point to improve on, not a target. Every attempt is
reported, including the invalid first L3/L4 runs (see
[Harness defect](#harness-defect-found-and-fixed-during-the-baseline)).

## Setup

| Item | Value |
|---|---|
| Models | `deepseek/deepseek-flash` (low-cost), `anthropic/claude-opus-5-5` (high-capability) |
| Suites | `l1` (12 tasks), `l2` (10), `l3` (10), `l4` (5 frozen tasks, [selection](../eval/real-world-tasks.md) signed off 2026-11-16) |
| Repetitions | 3 per task (L4: 3 attempts) |
| Agent sandbox | `required` (landlock, recorded as `repro.security.agent_sandbox = landlock`) |
| Repair loop | `disabled` (the `crom eval run` default) unless stated |
| Approvals | `allow_all` (eval default) |
| Runner | GitHub-hosted `ubuntu-latest`, 4 CPUs, 16 GiB, `eval-benchmark.yml` |
| Pricing | `crates/crom-core/pricing/prices.toml` (nominal list prices) |
| Result schema | v2 ([eval-schema.md](../eval-schema.md)) |

| Levels | Model | Commit | Workflow run | Cap |
|---|---|---|---|---|
| L1, L2 | DeepSeek | `de00616` | [43963697491](https://github.com/mfaulk/crom-rs/actions/runs/43963697491) | $5 |
| L1, L2 | Opus | `de00616` | [43963750496](https://github.com/mfaulk/crom-rs/actions/runs/43963750496) | $50 |
| L3, L4 | Opus | `2175c4b` | [44373465883](https://github.com/mfaulk/crom-rs/actions/runs/44373465883) | $50 |
| L3, L4 | DeepSeek | `e180603` | [44391287609](https://github.com/mfaulk/crom-rs/actions/runs/44391287609) | $5 |

L3/L4 were re-run after the harness fix (#370). The commits between
`de00616` and `e180603` change the eval harness (#370), artifact upload
(#371), error classification (#372), and docs/licences (#365, #368, #369);
none changes the agent, prompts, tools, or task corpus.

## Scoreboard

Rates are resolved / (resolved + failed). Provider errors are excluded from
the denominator and listed. Intervals are Wilson 95%.

| Level | DeepSeek resolved | rate [95% CI] | pass@3 | Opus resolved | rate [95% CI] | pass@3 |
|---|---|---|---|---|---|---|
| L1 | 34/36 | 0.944 [0.819, 0.985] | 1.000 | 36/36 | 1.000 [0.904, 1.000] | 1.000 |
| L2 | 28/30 | 0.933 [0.787, 0.982] | 1.000 | 28/30 | 0.933 [0.787, 0.982] | 1.000 |
| L3 | 27/28 (+2 provider errors) | 0.964 [0.823, 0.994] | 1.000 (k=2) | 30/30 | 1.000 [0.886, 1.000] | 1.000 |
| L4 | 10/15 | 0.667 [0.417, 0.848] | 0.800 | 12/15 | 0.800 [0.548, 0.930] | 0.800 |
| **All** | **99/109** | 0.908 [0.839, 0.949] | 0.955 | **106/111** | 0.955 [0.899, 0.981] | 0.973 |

With 10–15 tasks per level the intervals overlap at every level. The only
gap between the models is L4, and it is not significant at this sample size.

## Cost and efficiency

| Level | DeepSeek spend | $/resolved | Opus spend | $/resolved |
|---|---|---|---|---|
| L1 | $0.134 | $0.004 | $3.478 | $0.097 |
| L2 | $0.384 | $0.014 | $6.243 | $0.223 |
| L3 | $0.899 | $0.033 | $22.943 | $0.765 |
| L4 | $0.564 | $0.056 | $15.881 | $1.323 |
| **All** | **$1.980** | **$0.020** | **$48.544** | **$0.458** |

| Metric | DeepSeek | Opus |
|---|---|---|
| Median wall time per attempt | 48.4 s | 27.0 s |
| Mean tool calls / round trips | 22.8 / 19.8 | 5.5 / 6.1 |
| Tokens in / out | 1.86M / 0.97M (+0.50M reasoning) | 10.42M / 0.34M |
| Cache read tokens / hit rate | 43.9M / 0.959 | 0 / 0.000 |
| Usage reported | 111/111 | 111/111 |

Opus is 23× more expensive per resolved task. DeepSeek uses 4× more tool
calls but caches 96% of its input, which the provider does automatically.
Crom sends no Anthropic `cache_control`, so every Opus turn pays full input
price. Input tokens account for about $41.7 of Opus's $48.5.

Total live spend for #297, including the invalid and interrupted runs:
about $93.0 (DeepSeek $4.38 including the $0.37 repair run, Opus $88.62).

## Failure taxonomy

All non-resolved attempts in the published runs:

| Task | Model | Attempts failed | Failure class | Notes |
|---|---|---|---|---|
| `rust-nochange-midpoint` (L1) | DeepSeek | 1/3 | `unrecovered_test_failure` | |
| `rust-refactor-preserve` (L1) | DeepSeek | 1/3 | `unvalidated_completion` | |
| `l2-pipeline-shutdown` (L2) | DeepSeek | 2/3 | `unvalidated_completion` | edited forbidden `tests/run.rs` |
| `l2-pipeline-shutdown` (L2) | Opus | 2/3 | `forbidden_path_changed` | added new tests to forbidden `tests/run.rs` |
| `l3-ron-integer-suffix-base` (L3) | DeepSeek | 1/3 + 1 provider error | `unvalidated_completion` | |
| `l3-itertools-combinations-zero` (L3) | DeepSeek | 1 provider error | `provider_error` | |
| `l4-which-relative-path-cwd` (L4) | Opus | 3/3 | `overfit_public_tests` ×1, `unvalidated_completion` ×2 | |
| `l4-which-relative-path-cwd` (L4) | DeepSeek | 3/3 | `overfit_public_tests` ×2, `unvalidated_completion` ×1 | |
| `l4-glob-hidden-dir-leading-dot` (L4) | DeepSeek | 2/3 | `overfit_public_tests` | |

| Failure class | DeepSeek | Opus |
|---|---|---|
| `overfit_public_tests` (public checks pass, hidden checks fail) | 4 | 1 |
| `unvalidated_completion` (finished without a passing validation) | 5 | 2 |
| `forbidden_path_changed` | 0 | 2 |
| `unrecovered_test_failure` | 1 | 0 |
| `provider_error` (excluded from rates) | 2 | 0 |

The two DeepSeek provider errors were dropped connections (`error sending
request`, `error decoding response body`). They were classified as the
non-retryable `ProviderError::Http`, so the attempt ended. Fixed in #374.

`l2-pipeline-shutdown` is a task-spec defect. Its `forbidden_paths` include
`tests/run.rs`, but the prompt does not say so. Both models added tests for
the contract they had just implemented, and were failed for it. The scores
above are unchanged; the task needs its prompt fixed before the next
baseline (#377).

The scoreboard also flags "unvalidated completions" as a behavior signal on
resolved attempts (Opus 40/111, DeepSeek 57/111). The agent often declares
the task done without a final passing test run, even when the hidden checks
later pass.

## Repair on vs off (L2, DeepSeek)

Same suite, model, and 3 repetitions; `repair = enabled` ran on `335f8fc`
([run 44393715904](https://github.com/mfaulk/crom-rs/actions/runs/44393715904),
$3 cap), and `disabled` is the L2 baseline row above.

| Metric | Repair off | Repair on |
|---|---|---|
| Resolved | 28/30 (0.933 [0.787, 0.982]) | 29/30 (0.967 [0.833, 0.994]) |
| Spend / $ per resolved | $0.384 / $0.014 | $0.370 / $0.013 |
| Tasks with a failed validation | 0 | 0 |
| Recovered after repair | 0 | 0 |
| Final answer without validation | 21/30 | 21/30 |
| Failures | `l2-pipeline-shutdown` ×2 (`unvalidated_completion`) | `l2-expr-test-cascade` ×1 (`forbidden_path_changed`) |

**Inconclusive.** The repair loop never triggered: in neither arm did any
attempt end a validation run with a parseable failure for the loop to act on.
The one-attempt difference is noise. Repair can only help once the agent
validates. In 21 of 30 attempts it gave its final answer without a
validation after its last edit, which is the bottleneck in finding 1.

## Harness defect found and fixed during the baseline

The first L3/L4 runs (`de00616`, runs 43963697491 and 43963750496) are
**invalid** and excluded from the tables above:

- Hidden checks ran in a verification copy under `target/crom-eval/runs/…`,
  inside the crom-rs checkout. The 8 upstream tasks whose manifests do not
  declare `[workspace]` found crom-rs's root `Cargo.toml`, and Cargo refused
  to build (`current package believes it's in a workspace when it's not`).
- 48 of the 66 failed attempts across both models were this error. Every
  attempt on these tasks failed, for both models:
  - L3: `l3-bytes-try-get-int-sign`, `l3-itertools-combinations-zero`,
    `l3-ron-integer-suffix-base`, `l3-serde-yaml-duplicate-keys`.
  - L4: `l4-glob-hidden-dir-leading-dot`, `l4-quick-xml-dtd-chunk-panic`,
    `l4-tempfile-relative-temppath`, `l4-which-relative-path-cwd`.
- Offline CI missed it: `cargo test` places run dirs under `/tmp`, and the
  synthetic L1/L2 tasks declare their own workspace.
- Fixed in #370: hidden checks run in a fresh temp directory, and the copy is
  then moved into the run dir. Regression test:
  `crates/crom-eval/tests/verification_location.rs`.

For the record, the invalid runs scored L3 16/30 and L4 3/15 (DeepSeek), and
L3 18/30 and L4 3/15 (Opus). An interrupted DeepSeek re-run (44373414333)
stopped after 11 L3 attempts when the account ran out of credit (HTTP 402;
classified as a billing error since #372).

## Security suite

Not measured live. `crom eval run --suite adversarial` runs the fixtures
without provisioning host canaries, so it is not a security measurement
([adversarial-suite.md](../security/adversarial-suite.md#not-covered)). The
scripted lane result stands: 10/10 scenarios with `executed = 0` under
`sandbox = required`, run in CI by `security / OS sandbox (linux)`.

## Human review of L4 patches

_Pending._ Automated verification is reported above. Human review of every L4
patch is recorded separately here and must not change the automated
numbers. Patches are under `run-*/rep-*/tasks/l4-*/patch.diff` in the
archive.

## Findings: top bottlenecks

1. **Validation discipline.** `unvalidated_completion` and
   `overfit_public_tests` account for 12 of 15 scored failures, and the
   only task neither model solved (`l4-which-relative-path-cwd`) fails both
   ways. The agent stops once public tests pass, or before running them.
   Candidates: require a passing validation after the last edit
   (`--require-validation-after-edit` exists but is off), and prompt for
   edge-case tests beyond the visible ones.
2. **No Anthropic prompt caching.** Cache hit rate 0.000 against 0.959 on
   DeepSeek, and input is about 86% of Opus spend. Sending `cache_control`
   on the system prompt, tools, and conversation prefix is the largest
   single cost lever (#375).
3. **Usage lost on errored turns.** An errored turn's usage becomes
   "unknown", so `--max-run-cost-usd` can no longer be enforced, and the
   harness skips every remaining task (fail closed). One 402 cost 16 skipped
   attempts. Partial usage from the stream should be kept (#376).
4. **Transient transport failures were fatal** (fixed in #374).
5. **Task spec gaps.** `forbidden_paths` should be stated in the prompt, or
   added tests allowed; see `l2-pipeline-shutdown` (#377).
6. **Statistical power.** 5–12 tasks per level give intervals of ±10–25
   points. Distinguishing the models, or measuring an improvement, needs
   more L3/L4 tasks rather than more repetitions.

## Limitations

- One run per configuration, on one runner type. Wall times include
  `cargo` builds on a 4-CPU runner.
- L1/L2 and L3/L4 come from different commits (see [Setup](#setup)); the
  differences are harness-only.
- Costs are nominal list prices from `prices.toml`, not invoiced amounts.
- The L4 corpus is 5 tasks; one task drives most of the L4 gap.
- Gemini was not run: the account had no prepaid credit (HTTP 402); #283.
- No live security measurement (see above).

## Reproduce

Re-run a cell (GitHub Actions, `eval-live` environment secrets):

```console
$ gh workflow run eval-benchmark.yml --ref <commit> -f max_run_cost_usd=5 \
    -f targets=deepseek:deepseek-flash -f suites="l3 l4" -f repetitions=3
$ gh workflow run eval-benchmark.yml --ref <commit> -f max_run_cost_usd=50 \
    -f targets=anthropic:claude-opus-5-5 -f suites="l3 l4" -f repetitions=3
```

Locally, with a provider key set:

```console
$ crom eval run --suite l3 --provider deepseek --model deepseek-flash \
    --repetitions 3 --max-run-cost-usd 5 --allow-failures
```

Regenerate the tables from the archived results:

```console
$ mkdir -p /tmp/m17 && tar --zstd -xf docs/benchmarks/data/m17-baseline-results.tar.zst -C /tmp/m17
$ crom eval scoreboard --runs-dir /tmp/m17/runs --format md     # main tables
$ crom eval scoreboard --runs-dir /tmp/m17/repair --format md   # repair on
```

## Raw results

- `docs/benchmarks/data/m17-baseline-results.tar.zst` (1.0 MiB): the 8
  published run directories under `runs/` and the repair-on run under
  `repair/` (`run.json` where present, `summary.json`,
  per-attempt `result.json`, `verification.json`, `patch.diff`, transcripts,
  and events), without workspaces or build output. The L1/L2 run
  directories lack `run.json`; each `result.json` carries the same
  provenance under `repro`.
- Full CI artifacts, including workspaces, are attached to the workflow runs
  above until 2027-02-14 (runs on `de00616`) and 2027-02-16 (re-runs).
