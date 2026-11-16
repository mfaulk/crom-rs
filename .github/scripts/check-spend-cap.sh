#!/usr/bin/env bash
# Fail-closed spend-cap gate for the live eval lanes (#295).
# Usage: check-spend-cap.sh <cap-usd> <ceiling-usd>
# Exits non-zero unless 0 < cap <= ceiling and cap is a plain decimal.
set -euo pipefail

cap="${1:-}"
ceiling="${2:-}"

if [[ -z "$cap" ]]; then
  echo "::error::a spend cap (USD) is required for live eval runs" >&2
  exit 1
fi
if ! [[ "$cap" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  echo "::error::spend cap '$cap' is not a plain decimal USD amount" >&2
  exit 1
fi
if ! [[ "$ceiling" =~ ^[0-9]+(\.[0-9]+)?$ ]]; then
  echo "::error::ceiling '$ceiling' is not a plain decimal USD amount" >&2
  exit 1
fi
if ! awk -v c="$cap" -v m="$ceiling" 'BEGIN { exit !(c > 0 && c <= m) }'; then
  echo "::error::spend cap \$$cap must be > 0 and <= \$$ceiling" >&2
  exit 1
fi
echo "spend cap: \$$cap (ceiling \$$ceiling)"
