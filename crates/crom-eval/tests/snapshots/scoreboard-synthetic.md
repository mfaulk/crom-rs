# Eval scoreboard

group by: model; since: all; models: all; levels: all
Rates are resolved / (resolved + failed) with Wilson 95% intervals; excluded = provider_error (pe), infra_error (ie), skipped (sk), unscorable (un).

## deepseek/deepseek-chat

runs: 1, attempts: 1

### Capability by level

| row | tasks | resolved/n | rate | wilson95 | pass@1 | pass@k | excluded |
|---|---|---|---|---|---|---|---|
| capability | 1 | 0/1 | 0.000 | [0.000, 0.793] | 0.000 | - | pe=0 ie=0 sk=0 un=0 |
| L1 | 1 | 0/1 | 0.000 | [0.000, 0.793] | 0.000 | - | pe=0 ie=0 sk=0 un=0 |

### Capability by category

| row | tasks | resolved/n | rate | wilson95 | pass@1 | pass@k | excluded |
|---|---|---|---|---|---|---|---|
| bugfix | 1 | 0/1 | 0.000 | [0.000, 0.793] | 0.000 | - | pe=0 ie=0 sk=0 un=0 |

### Efficiency (capability attempts)

| metric | value |
|---|---|
| attempts | 1 |
| resolved | 0 |
| total spend | unknown (0 of 1 attempts priced) |
| cost / attempt | unknown |
| cost / resolved | unknown |
| median wall ms | 100 |
| mean tool calls | 3.00 |
| mean round trips | 0.00 |
| usage reported | 0/1 |
| tokens in/out | -/- |
| cache read/write | -/- |
| reasoning tokens | - |
| cache hit rate | - |

### Reliability (all attempts)

| metric | value |
|---|---|
| attempts | 1 |
| timeouts | 0 |
| provider errors | 0 |
| infra errors | 0 |
| unscorable | 0 |
| tool loops | 0 |
| flaky / repeated tasks | 0/0 |
| mean task variance | - |
| repair recovered | 0/0 (-) |
| repeated-failure stops | 0 |
| unvalidated completions | 0 |
| unreliable | no |

## fake/fake

runs: 3, attempts: 17

### Capability by level

| row | tasks | resolved/n | rate | wilson95 | pass@1 | pass@k | excluded |
|---|---|---|---|---|---|---|---|
| capability | 4 | 6/9 | 0.667 | [0.354, 0.879] | 0.667 | 1.000 (k=3, n=3) | pe=1 ie=1 sk=1 un=1 |
| L1 | 2 | 3/6 | 0.500 | [0.188, 0.812] | 0.500 | 1.000 (k=3, n=3) | pe=0 ie=0 sk=0 un=0 |
| L2 | 2 | 3/3 | 1.000 | [0.439, 1.000] | 1.000 | 1.000 (k=3, n=3) | pe=1 ie=1 sk=1 un=1 |

### Capability by category

| row | tasks | resolved/n | rate | wilson95 | pass@1 | pass@k | excluded |
|---|---|---|---|---|---|---|---|
| bugfix | 4 | 6/9 | 0.667 | [0.354, 0.879] | 0.667 | 1.000 (k=3, n=3) | pe=1 ie=1 sk=1 un=1 |

### Harness and unclassified (not capability)

| row | tasks | resolved/n | rate | wilson95 | pass@1 | pass@k | excluded |
|---|---|---|---|---|---|---|---|
| harness (L0) | 1 | 2/2 | 1.000 | [0.342, 1.000] | 1.000 | 1.000 (k=2, n=2) | pe=0 ie=0 sk=0 un=0 |
| unclassified | 1 | 0/1 | 0.000 | [0.000, 0.793] | 0.000 | - | pe=0 ie=0 sk=0 un=0 |

### Efficiency (capability attempts)

| metric | value |
|---|---|
| attempts | 12 |
| resolved | 6 |
| total spend | $0.2200 (11 of 12 attempts priced) |
| cost / attempt | $0.0200 |
| cost / resolved | $0.0367 |
| median wall ms | 1500 |
| mean tool calls | 1.33 |
| mean round trips | 1.92 |
| usage reported | 12/12 |
| tokens in/out | 12000/2400 |
| cache read/write | 6000/0 |
| reasoning tokens | - |
| cache hit rate | 0.333 |

### Reliability (all attempts)

| metric | value |
|---|---|
| attempts | 16 |
| timeouts | 1 |
| provider errors | 1 |
| infra errors | 1 |
| unscorable | 1 |
| tool loops | 0 |
| flaky / repeated tasks | 2/4 |
| mean task variance | 0.111 |
| repair recovered | 1/2 (0.500) |
| repeated-failure stops | 1 |
| unvalidated completions | 1 |
| unreliable | YES |

### Security (separate; never folded into capability)

| row | tasks | resolved/n | rate | wilson95 | audited | tool calls | blocked | executed | audit coverage |
|---|---|---|---|---|---|---|---|---|---|
| security | 1 | 1/1 | 1.000 | [0.207, 1.000] | 1/1 | 3 | 2 | 1 | 1.000 |

