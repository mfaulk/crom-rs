#!/usr/bin/env bash
# Runnable, offline demo of the M10 provider/auth/model surface.
#
# Builds the CLI if the binary is missing, then exercises the user-facing
# commands against the in-process fake provider. No network is required.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BIN="$ROOT/target/debug/code-agent"

if [[ ! -x "$BIN" ]]; then
  echo "[demo] building code-agent (debug)..."
  (cd "$ROOT" && cargo build -p code-agent-cli >/dev/null)
fi

cd "$HERE"

# Always start from a clean .agent/auth.json so the demo is reproducible.
rm -f .agent/auth.json
rm -rf .agent/secrets

run() {
  echo
  echo "[demo] $*"
  echo "----------------------------------------"
  "$BIN" "$@" || true
}

run provider list
run login
run login fake --api-key=demo --allow-file-store
run auth status --allow-file-store
run model list
run model use fake/fake-coding-model
run auth logout fake --allow-file-store
run auth status --allow-file-store

echo
echo "[demo] done."
