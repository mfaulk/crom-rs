#!/usr/bin/env bash
#
# End-to-end demo of Milestone 1 (Headless Rust Coding Agent Alpha).
#
# Exercises every M1 surface in one run:
#   * `crom run`     — Read, Edit, Bash under the `dontAsk` permission mode
#   * `crom sessions list`
#   * `crom resume`  — Write, Glob, Grep on a recovered session
#   * AGENTS.md project memory (per-task scope for the agent)
#   * JSONL session persistence under .agent/sessions/
#
# `dontAsk` is used so the script runs unattended; both phases pre-list the
# exact tools the agent is allowed to call via `--allowed-tools`.
#
# Requires:
#   * cargo / rustc on PATH
#   * ANTHROPIC_API_KEY exported
#   * a clean working tree under examples/bugfix-demo (the script will
#     `git checkout` the demo's src/lib.rs to restore the buggy state)

set -euo pipefail

bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
cyan()  { printf '\033[36m%s\033[0m\n' "$*"; }
gray()  { printf '\033[90m%s\033[0m\n' "$*"; }
section() { printf '\n'; bold "============================================================"; bold "$*"; bold "============================================================"; }

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "error: ANTHROPIC_API_KEY is not set." >&2
    echo "       export it first: export ANTHROPIC_API_KEY=sk-ant-..." >&2
    exit 2
fi

REPO_ROOT="$(git rev-parse --show-toplevel)"
DEMO_DIR="$REPO_ROOT/examples/bugfix-demo"

if [[ ! -d "$DEMO_DIR" ]]; then
    echo "error: $DEMO_DIR not found" >&2
    exit 2
fi

# Resolve the workspace target directory via cargo metadata so this script
# works under CARGO_TARGET_DIR or a [build] target-dir override in
# .cargo/config.toml. Falls back to the conventional location.
TARGET_DIR="$(
    cargo metadata --format-version 1 --no-deps \
        --manifest-path "$REPO_ROOT/Cargo.toml" 2>/dev/null \
        | grep -oE '"target_directory":"[^"]*"' \
        | head -n 1 \
        | cut -d'"' -f4 \
        || true
)"
TARGET_DIR="${TARGET_DIR:-$REPO_ROOT/target}"
BIN="$TARGET_DIR/debug/crom"

# ---------------------------------------------------------------------------
section "Step 0 — Build the crom CLI"
# Debug build: this demo is API-bound, so the cold-start savings outweigh any
# release-mode gains.
cargo build -p crom-cli --manifest-path "$REPO_ROOT/Cargo.toml"
gray "binary: $BIN"
if [[ ! -x "$BIN" ]]; then
    echo "error: expected binary at $BIN was not produced" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
section "Step 1 — Reset the demo to a clean, buggy starting state"

cd "$DEMO_DIR"
# Restore the deliberately buggy lib.rs in case a prior demo run fixed it.
# The bugfix-demo crate is excluded from the workspace, so its tracked files
# are managed by git directly.
if git diff --quiet -- src/lib.rs && git diff --cached --quiet -- src/lib.rs; then
    gray "src/lib.rs is unmodified vs HEAD; no reset needed."
else
    gray "Restoring src/lib.rs to its committed (buggy) state."
    git checkout HEAD -- src/lib.rs
fi

# Wipe any prior demo sessions so `sessions list` only shows what this run
# produced.
rm -rf "$DEMO_DIR/.agent"
# Also remove any prior follow-up artifact.
rm -f "$DEMO_DIR/SUMMARY.md"

cyan "Confirming the test suite is currently failing:"
set +e
cargo test --quiet 2>&1 | tail -n 15
TEST_EXIT=${PIPESTATUS[0]}
set -e
if [[ "$TEST_EXIT" -eq 0 ]]; then
    echo "error: tests pass before the agent runs — the demo is misconfigured." >&2
    exit 3
fi
gray "(cargo test exited non-zero, as expected)"

# ---------------------------------------------------------------------------
section "Step 2 — Run the agent: fix the bug (Read + Edit + Bash)"

cyan "Command:"
gray "  crom run \"Run the tests, find the bug, fix it, and re-run the tests.\" \\"
gray "      --cwd . --allowed-tools Read,Glob,Grep,Edit,Bash \\"
gray "      --permission-mode dontAsk --max-turns 8"

LOG="$(mktemp)"
trap 'rm -f "$LOG"' EXIT

"$BIN" run \
    "Run the tests, find the bug, fix it, and re-run the tests to confirm everything passes." \
    --cwd "$DEMO_DIR" \
    --allowed-tools Read,Glob,Grep,Edit,Bash \
    --permission-mode dontAsk \
    --max-turns 8 2>&1 | tee "$LOG"

# `|| true` is required: with `set -euo pipefail`, a non-matching grep (or a
# SIGPIPE from `head` closing the pipe early) would abort the script before
# the explicit empty-check below could fire.
SESSION_ID="$(grep -oE '\[init\] session [^[:space:]]+' "$LOG" | head -n 1 | awk '{print $3}' || true)"
if [[ -z "${SESSION_ID:-}" ]]; then
    echo "error: failed to parse session id from agent output." >&2
    exit 4
fi
cyan "Captured session id: $SESSION_ID"

# ---------------------------------------------------------------------------
section "Step 3 — Verify the fix landed and tests now pass"

cyan "Diff vs HEAD on src/lib.rs:"
git --no-pager diff -- src/lib.rs || true

cyan "cargo test:"
cargo test --quiet 2>&1 | tail -n 10

# ---------------------------------------------------------------------------
section "Step 4 — \`crom sessions list\`"

cyan "Command:"
gray "  crom sessions list --cwd ."
"$BIN" sessions list --cwd "$DEMO_DIR"

cyan "JSONL on disk for this session (first few records):"
SESSION_FILE="$DEMO_DIR/.agent/sessions/${SESSION_ID}.jsonl"
if [[ -f "$SESSION_FILE" ]]; then
    gray "$SESSION_FILE"
    head -n 3 "$SESSION_FILE"
    LINES_BEFORE=$(wc -l < "$SESSION_FILE")
    gray "  ... (total $LINES_BEFORE records before resume)"
else
    echo "error: expected session file $SESSION_FILE was not created" >&2
    exit 5
fi

# ---------------------------------------------------------------------------
section "Step 5 — \`crom resume\`: follow-up that uses Write + Glob + Grep"

cyan "Command:"
gray "  crom resume $SESSION_ID 'Write a SUMMARY.md as described in AGENTS.md.' \\"
gray "      --cwd . --allowed-tools Read,Glob,Grep,Write \\"
gray "      --permission-mode dontAsk --max-turns 6"

"$BIN" resume "$SESSION_ID" \
    "Now perform the follow-up task described in AGENTS.md: write SUMMARY.md at the crate root that documents the bug, the fix, and lists every file in this crate that mentions parse_token." \
    --cwd "$DEMO_DIR" \
    --allowed-tools Read,Glob,Grep,Write \
    --permission-mode dontAsk \
    --max-turns 6

# ---------------------------------------------------------------------------
section "Step 6 — Inspect the resume artifacts"

if [[ -f "$DEMO_DIR/SUMMARY.md" ]]; then
    cyan "SUMMARY.md (created by the agent during resume):"
    cat "$DEMO_DIR/SUMMARY.md"
else
    echo "warning: SUMMARY.md was not produced by the resume turn." >&2
fi

LINES_AFTER=$(wc -l < "$SESSION_FILE")
cyan "Session JSONL grew from $LINES_BEFORE to $LINES_AFTER records (resume appended in-place)."

# ---------------------------------------------------------------------------
section "Done"
cat <<EOF
Demo complete. Summary of what just happened:

  * Built the crom CLI (debug profile).
  * Reset examples/bugfix-demo to its buggy state and confirmed cargo test failed.
  * Ran the agent with dontAsk and the Read/Glob/Grep/Edit/Bash tool set;
    it found the bug, edited src/lib.rs, and re-ran the tests.
  * Listed sessions and inspected the JSONL persistence under .agent/sessions/.
  * Resumed the same session with the Read/Glob/Grep/Write tool set and asked
    for the AGENTS.md follow-up; the agent produced SUMMARY.md without
    re-running tests.
  * Verified that resume appended new records to the same JSONL file.

To replay from scratch, just run this script again — Step 1 resets state.
EOF
