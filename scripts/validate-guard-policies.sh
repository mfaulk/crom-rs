#!/usr/bin/env bash
# Strict-mode validation and format check for Guard Cedar policy sets (ADR 0003).
# Guard refuses to start on a policy set that fails strict validation; this
# catches the same failure before merge. Requires the `cedar` binary from
# cedar-policy-cli at the version Guard pins:
#   cargo install --locked cedar-policy-cli --version 4.15.0
#
# Usage: scripts/validate-guard-policies.sh [policy-dir...]
#        (default: examples/guard-support/policies)
# Env:   CEDAR (path to the cedar binary; default: `cedar` on PATH)
set -euo pipefail

cedar="${CEDAR:-cedar}"
want_version="4.15.0"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dirs=("$@")
[[ ${#dirs[@]} -gt 0 ]] || dirs=("$root/examples/guard-support/policies")

if ! command -v "$cedar" >/dev/null; then
  echo "error: '$cedar' not found; install with: cargo install --locked cedar-policy-cli --version $want_version" >&2
  exit 2
fi
have_version="$("$cedar" --version | awk '{print $2}')"
if [[ "$have_version" != "$want_version" ]]; then
  echo "error: cedar-policy-cli $have_version found, $want_version required" >&2
  exit 2
fi

status=0
for dir in "${dirs[@]}"; do
  shopt -s nullglob
  schemas=("$dir"/*.cedarschema)
  policies=("$dir"/*.cedar)
  shopt -u nullglob
  if [[ ${#schemas[@]} -ne 1 || ${#policies[@]} -eq 0 ]]; then
    echo "error: $dir must contain exactly one .cedarschema and at least one .cedar file" >&2
    status=1
    continue
  fi
  for p in "${policies[@]}"; do
    echo "validate (strict): $p against ${schemas[0]}"
    "$cedar" validate --schema "${schemas[0]}" --policies "$p" \
      --validation-mode strict --deny-warnings || status=1
    "$cedar" format --check --policies "$p" >/dev/null \
      || { echo "error: $p is not cedar-formatted (run: cedar format -p $p)" >&2; status=1; }
  done
done
exit "$status"
