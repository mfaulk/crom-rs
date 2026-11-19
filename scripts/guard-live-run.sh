#!/usr/bin/env bash
# One provider's live reference-app run for .github/workflows/guard-live.yml.
# Usage: guard-live-run.sh <provider> <key-env-var>
# Needs PER_RUN_COST_USD; MODEL is optional. The caller's environment should
# hold only this provider's key.
set -euo pipefail

provider="$1"
key_var="$2"
if [ -z "${!key_var:-}" ]; then
  echo "::warning::no API key secret for $provider ($key_var); skipped"
  exit 0
fi
: "${PER_RUN_COST_USD:?PER_RUN_COST_USD is not set}"

args=(--agent live --provider "$provider" --supervisor deny
      --max-run-cost-usd "$PER_RUN_COST_USD" --out "target/guard-live/$provider")
if [ -n "${MODEL:-}" ]; then
  args+=(--model "$MODEL")
fi
mkdir -p target/guard-live
exec cargo run --locked --manifest-path examples/guard-support/Cargo.toml -- "${args[@]}"
