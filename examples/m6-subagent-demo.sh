#!/usr/bin/env bash
# M6 subagent orchestration demo. Builds the CLI, then walks through:
#   1. `code-agent agents list`     — show registered subagents
#   2. `code-agent agents validate` — structural-validate definitions
#   3. `code-agent agents show ...` — print one definition
#   4. (optional) live `code-agent run` if ANTHROPIC_API_KEY is set
#
# Live runs are skipped when ANTHROPIC_API_KEY is unset so this script is
# safe to run in CI.

set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
CRATE_DIR="$HERE/subagent-demo"

cd "$HERE/.."
echo "[build] cargo build --bin code-agent --quiet"
cargo build --bin code-agent --quiet

CODE_AGENT="$HERE/../target/debug/code-agent"

echo
echo "=== code-agent agents list ==="
"$CODE_AGENT" agents list --cwd "$CRATE_DIR"

echo
echo "=== code-agent agents validate ==="
"$CODE_AGENT" agents validate --cwd "$CRATE_DIR"

echo
echo "=== code-agent agents show security-reviewer ==="
"$CODE_AGENT" agents show security-reviewer --cwd "$CRATE_DIR"

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
    echo
    echo "[skipped] live 'code-agent run' because ANTHROPIC_API_KEY is unset."
    echo "          set ANTHROPIC_API_KEY and re-run to exercise the parent agent."
    exit 0
fi

echo
echo "=== live: code-agent run with subagents ==="
"$CODE_AGENT" run \
    "Review this crate for security, tests, and API design. Use subagents where helpful." \
    --cwd "$CRATE_DIR" \
    --allowed-tools "Read,Grep,Glob,Bash,Agent" \
    --agents "$CRATE_DIR/.agent/agents" \
    --show-subagent-events
