#!/usr/bin/env bash
# Guard scoreboard (#363). docs/guard/scoreboard.md is generated from the
# archived reports in docs/guard/results/ and never edited by hand.
#
#   scripts/guard-scoreboard.sh                  rewrite docs/guard/scoreboard.md
#   scripts/guard-scoreboard.sh --check [FRESH...]
#       fail if the doc differs from a fresh rendering of the archive; with
#       FRESH report files, also fail if their deterministic facts (verdict
#       counts, E1 cells, E2 counts, E4a decisions; not timings) differ from
#       the archive's
set -euo pipefail
cd "$(dirname "$0")/.."

results=docs/guard/results
doc=docs/guard/scoreboard.md
crom=(cargo run --quiet --locked -p crom-cli --)

render() {
  "${crom[@]}" eval scoreboard --guard-report "$results" --format md
}

if [[ "${1:-}" == "--check" ]]; then
  shift
  if ! diff -u "$doc" <(render); then
    echo "error: $doc is stale; run scripts/guard-scoreboard.sh" >&2
    exit 1
  fi
  echo "ok: $doc matches $results"
  if (($#)); then
    args=()
    for f in "$@"; do
      args+=(--guard-report "$f")
    done
    "${crom[@]}" eval scoreboard "${args[@]}" --guard-check "$results"
  fi
else
  render >"$doc"
  echo "wrote $doc"
fi
