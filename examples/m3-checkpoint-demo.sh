#!/usr/bin/env bash
# Non-interactive M3 demo: drives examples/checkpoint-demo through the
# full checkpoint + rewind workflow without an Anthropic API key by
# scripting the CLI's checkpoint commands directly. (The agent run
# itself does need ANTHROPIC_API_KEY, so it is gated.)

set -eu

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$SCRIPT_DIR/.."
DEMO="$ROOT/examples/checkpoint-demo"

cd "$DEMO"

# Restore the demo to a clean (intentionally-broken) state.
cat > src/lib.rs <<'EOF'
/// Tokenize a comma-separated input. The current implementation does not
/// trim whitespace from individual tokens; the demo task is to refactor
/// it to trim and to add tests.
pub fn parse_tokens(input: &str) -> Vec<String> {
    input
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Bare tokens with no surrounding whitespace are returned verbatim.
    fn no_whitespace_tokens() {
        assert_eq!(parse_tokens("a,b,c"), vec!["a", "b", "c"]);
    }
}
EOF
rm -rf .agent/sessions .agent/checkpoints

# Step 0: confirm the failing test exists.
echo "[m3-demo] step 0: confirm the failing test"
if cargo test --quiet 2>/dev/null; then
    echo "  unexpected: tokens_are_trimmed should fail on the broken parser"
    exit 1
fi
echo "  ok: tokens_are_trimmed fails as expected"

CODE_AGENT=(cargo run --quiet --manifest-path "$ROOT/Cargo.toml" -p code-agent-cli --)

# Step 1: create a session manually so checkpoint subcommands have something
# to attach to.
SESSION_ID=$(uuidgen | tr -d '-')
mkdir -p .agent/sessions
touch ".agent/sessions/${SESSION_ID}.jsonl"
echo "[m3-demo] step 1: session ${SESSION_ID}"

# Step 2: take a manual checkpoint of the broken state.
echo "[m3-demo] step 2: create a manual checkpoint"
"${CODE_AGENT[@]}" checkpoint "before refactor" --cwd . --session "${SESSION_ID}"
"${CODE_AGENT[@]}" checkpoints list --cwd . --session "${SESSION_ID}"

# Capture the checkpoint id for the rewind dance below.
CKPT_ID=$("${CODE_AGENT[@]}" checkpoints list --cwd . --session "${SESSION_ID}" | awk 'NR==1 {print $1}')
echo "[m3-demo] checkpoint id: ${CKPT_ID}"

# Step 3: simulate an agent edit (it doesn't need the API key — we use
# code-agent itself to capture the snapshot, then mutate the file by
# hand to demonstrate rewind).
echo "[m3-demo] step 3: simulate a mutation and rewind"

# The CLI snapshot path runs only via the agent loop; for the demo we
# manually capture src/lib.rs into the checkpoint using `code-agent`'s
# checkpoint create on the same session. Instead the easier flow is to
# overwrite src/lib.rs and then call `rewind --dry-run` to see what
# rewind WOULD do, then optionally restore.
cp src/lib.rs src/lib.rs.bak
cat > src/lib.rs <<'EOF'
pub fn parse_tokens(input: &str) -> Vec<String> {
    input
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().to_string())
        .collect()
}
EOF

echo "[m3-demo] step 4: diff (should report the mutation if a snapshot was captured)"
"${CODE_AGENT[@]}" checkpoints diff "${CKPT_ID}" --cwd . --session "${SESSION_ID}" || true

echo "[m3-demo] step 5: dry-run rewind"
"${CODE_AGENT[@]}" rewind "${CKPT_ID}" --cwd . --dry-run

# Restore by hand so the example dir doesn't keep the backup file.
mv src/lib.rs.bak src/lib.rs
rm -rf .agent/sessions .agent/checkpoints

echo "[m3-demo] done. For the full agent-driven flow, set ANTHROPIC_API_KEY"
echo "and run: code-agent run \"Refactor parse_tokens to trim\" \\"
echo "  --checkpointing on --permission-mode acceptEdits --validate \"cargo test\""
