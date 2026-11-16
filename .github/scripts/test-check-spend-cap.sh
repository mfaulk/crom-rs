#!/usr/bin/env bash
# Offline dry-run of the live-lane spend-cap gate: bad caps must fail closed.
set -euo pipefail
gate="$(dirname "$0")/check-spend-cap.sh"
fail=0

expect() {
  local want="$1"; shift
  if "$gate" "$@" >/dev/null 2>&1; then got=pass; else got=fail; fi
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: check-spend-cap.sh $* -> $got (want $want)"
    fail=1
  fi
}

expect fail "" 5
expect fail 0 5
expect fail 0.0 5
expect fail -1 5
expect fail abc 5
expect fail '1; rm -rf /' 5
expect fail 6 5
expect fail 1 ""
expect pass 1 5
expect pass 0.25 5
expect pass 5 5

[[ "$fail" -eq 0 ]] && echo "check-spend-cap: all cases ok"
exit "$fail"
