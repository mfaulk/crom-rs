#!/usr/bin/env bash
# M6 subagent orchestration demo. Builds the CLI, then walks through:
#   1. `crom agents list`     — show registered subagents
#   2. `crom agents validate` — structural-validate definitions
#   3. `crom agents show ...` — print one definition
#   4. (optional) live `crom run` if ANTHROPIC_API_KEY is set
#
# Live runs are skipped when ANTHROPIC_API_KEY is unset so this script is
# safe to run in CI.

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE_DIR="$HERE/subagent-demo"

cd "$HERE/.."
echo "[build] cargo build --bin crom --quiet"
cargo build --bin crom --quiet

CROM="$HERE/../target/debug/crom"

echo
echo "=== crom agents list ==="
"$CROM" agents list --cwd "$CRATE_DIR"

echo
echo "=== crom agents validate ==="
"$CROM" agents validate --cwd "$CRATE_DIR"

echo
echo "=== crom agents show security-reviewer ==="
"$CROM" agents show security-reviewer --cwd "$CRATE_DIR"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo
    echo "[skipped] live 'crom run' because ANTHROPIC_API_KEY is unset."
    echo "          set ANTHROPIC_API_KEY and re-run to exercise the parent agent."
    exit 0
fi

echo
echo "=== live: crom run with subagents ==="
"$CROM" run \
    "Review this crate for security, tests, and API design. Use subagents where helpful." \
    --cwd "$CRATE_DIR" \
    --allowed-tools "Read,Grep,Glob,Bash,Agent" \
    --agents "$CRATE_DIR/.agent/agents" \
    --show-subagent-events
