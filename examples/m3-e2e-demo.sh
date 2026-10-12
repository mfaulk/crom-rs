#!/usr/bin/env bash
#
# End-to-end demo of Milestone 3 (Workspace Checkpointing + Rewind Alpha).
#
# Builds on examples/m2-e2e-demo.sh by exercising the M3-specific surface:
#
#   * Start-of-run checkpoint (`create_on_user_message: true`).
#   * Per-mutation snapshots emitted by `Edit`/`Write` (`[checkpoint] captured
#     <path> before change`), driven by the agent loop.
#   * Bash `allow_with_warning` policy emitting `[warning]` events for shell
#     commands that are not file-tracked.
#   * Post-run validation gate (`--validate "cargo test"`,
#     `--validation-timeout`).
#   * Manual `crom checkpoint` plus `crom checkpoints list|show|
#     diff` for inspection.
#   * `crom rewind --dry-run` and `crom rewind --yes` to restore
#     workspace files (conversation history is intentionally NOT rewound).
#   * `--rewind-on-validation-failure`: a deliberately-failing post-run gate
#     drives an automatic rewind to the start-of-run checkpoint.
#   * On-disk content-addressed layout under
#     `<cwd>/.agent/checkpoints/<session>/{checkpoints.jsonl, manifests/, blobs/}`.
#
# `examples/checkpoint-demo` is the sandbox: a one-test crate whose
# `tokens_are_trimmed` test fails by design, with a `.agent/settings.json`
# pre-configuring `checkpointing.enabled = true`,
# `bash_policy = allow_with_warning`, and a `cargo test` validation command.
#
# Requires:
#   * cargo / rustc on PATH
#   * ANTHROPIC_API_KEY exported or set in the repo-root .env
#   * a clean working tree under examples/checkpoint-demo (the script
#     restores tracked files from HEAD and wipes generated artifacts on
#     every run)

set -euo pipefail

bold()  { printf '\033[1m%s\033[0m\n' "$*"; }
cyan()  { printf '\033[36m%s\033[0m\n' "$*"; }
green() { printf '\033[32m%s\033[0m\n' "$*"; }
yellow(){ printf '\033[33m%s\033[0m\n' "$*"; }
gray()  { printf '\033[90m%s\033[0m\n' "$*"; }
section() { printf '\n'; bold "============================================================"; bold "$*"; bold "============================================================"; }

REPO_ROOT="$(git rev-parse --show-toplevel)"
DEMO_DIR="$REPO_ROOT/examples/checkpoint-demo"

load_anthropic_key_from_dotenv() {
    local env_file="$REPO_ROOT/.env"
    local line value first last

    [[ -z "${ANTHROPIC_API_KEY:-}" ]] || return 0
    [[ -f "$env_file" ]] || return 0

    while IFS= read -r line || [[ -n "$line" ]]; do
        line="${line%$'\r'}"
        [[ "$line" =~ ^[[:space:]]*(export[[:space:]]+)?ANTHROPIC_API_KEY=(.*)$ ]] || continue

        value="${BASH_REMATCH[2]}"
        value="${value#"${value%%[![:space:]]*}"}"
        value="${value%"${value##*[![:space:]]}"}"

        if [[ "$value" != \"* && "$value" != \'* ]]; then
            value="${value%%#*}"
            value="${value%"${value##*[![:space:]]}"}"
        fi

        if [[ "${#value}" -ge 2 ]]; then
            first="${value:0:1}"
            last="${value: -1}"
            if [[ "$first" == "$last" && ( "$first" == '"' || "$first" == "'" ) ]]; then
                value="${value:1:${#value}-2}"
            fi
        fi

        if [[ -n "$value" ]]; then
            export ANTHROPIC_API_KEY="$value"
            gray "Loaded ANTHROPIC_API_KEY from $env_file."
        fi
        return 0
    done < "$env_file"
}

load_anthropic_key_from_dotenv

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    echo "error: ANTHROPIC_API_KEY is not set." >&2
    echo "       export it first or add ANTHROPIC_API_KEY=sk-ant-... to $REPO_ROOT/.env" >&2
    exit 2
fi

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

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# ---------------------------------------------------------------------------
section "Step 0 — Build the crom CLI"
cargo build -p crom-cli --manifest-path "$REPO_ROOT/Cargo.toml"
gray "binary: $BIN"
if [[ ! -x "$BIN" ]]; then
    echo "error: expected binary at $BIN was not produced" >&2
    exit 2
fi

# ---------------------------------------------------------------------------
section "Step 1 — Reset checkpoint-demo to a clean, buggy starting state"

cd "$DEMO_DIR"

# `checkpoint-demo` is excluded from the workspace; its tracked files are
# managed by git directly. Restore every tracked file under this crate
# from HEAD so the agent can't leak edits (to e.g. Cargo.toml or
# AGENTS.md) between runs, and drop every untracked non-ignored file.
# `.agent/sessions/` and `.agent/checkpoints/` are gitignored, so
# `git clean` will not touch them — wipe them explicitly below.
gray "Restoring tracked demo files from HEAD and removing untracked artifacts."
git checkout HEAD -- .
git clean -fd -- . >/dev/null 2>&1 || true

rm -rf "$DEMO_DIR/.agent/sessions" "$DEMO_DIR/.agent/checkpoints"
rm -f "$DEMO_DIR/SUMMARY.md"

# Content-hash src/lib.rs so we can prove the rewind step in Step 8
# returns it to byte-for-byte the same content. `git hash-object` is
# preferred over `sha256sum` because it is portable across Linux and
# macOS (macOS does not ship `sha256sum` by default) and `git` is
# already a hard requirement of this script.
hash_file() { git hash-object "$1"; }
LIB_HASH_BROKEN="$(hash_file "$DEMO_DIR/src/lib.rs")"
gray "src/lib.rs git-hash (buggy state): $LIB_HASH_BROKEN"

# ---------------------------------------------------------------------------
section "Step 2 — Show the M3 policy that gates this demo"

cyan ".agent/settings.json (note checkpointing + validation + bash_policy):"
cat "$DEMO_DIR/.agent/settings.json"
printf '\n'
cyan "AGENTS.md:"
cat "$DEMO_DIR/AGENTS.md"

# ---------------------------------------------------------------------------
section "Step 3 — Confirm the test suite is currently failing"

set +e
cargo test --quiet 2>&1 | tail -n 15
TEST_EXIT=${PIPESTATUS[0]}
set -e
if [[ "$TEST_EXIT" -eq 0 ]]; then
    echo "error: tests pass before the agent runs — demo is misconfigured." >&2
    exit 3
fi
gray "(cargo test exited non-zero, as expected)"

# ---------------------------------------------------------------------------
section "Step 4 — Fix the bug under checkpointing + validation gate"

cyan "Command:"
gray "  crom run \"...\" --cwd . \\"
gray "      --checkpointing on \\"
gray "      --permission-mode acceptEdits \\"
gray "      --validate \"cargo test\" --validation-timeout 90 \\"
gray "      --max-turns 12 --show-usage --verbose"

FIX_LOG="$WORK_DIR/fix.log"

"$BIN" run \
    "Fix the failing parse_tokens test in src/lib.rs by trimming whitespace from each token. Then add a new test in tests/ that covers an additional case (for example, tokens with internal spaces or empty input). Re-run cargo test to confirm the suite passes. Stay within the rules in AGENTS.md and .agent/settings.json — under no circumstances modify any .env file." \
    --cwd "$DEMO_DIR" \
    --checkpointing on \
    --permission-mode acceptEdits \
    --validate "cargo test" \
    --validation-timeout 90 \
    --max-turns 12 \
    --show-usage \
    --verbose 2>&1 | tee "$FIX_LOG"

FIX_SESSION="$(grep -oE '\[init\] session [^[:space:]]+' "$FIX_LOG" | head -n 1 | awk '{print $3}' || true)"
if [[ -z "${FIX_SESSION:-}" ]]; then
    echo "error: failed to parse session id from agent output." >&2
    exit 4
fi
cyan "Captured fix session id: $FIX_SESSION"

# ---------------------------------------------------------------------------
section "Step 5 — Verify checkpointing + validation events fired"

# The start-of-run checkpoint must have been created before the first
# tool call, regardless of which order the agent did things in.
START_CKPT_LINE="$(grep -E '^\[checkpoint\] created [^ ]+ before user request' "$FIX_LOG" | head -n 1 || true)"
if [[ -z "$START_CKPT_LINE" ]]; then
    echo "error: missing '[checkpoint] created <id> before user request'." >&2
    echo "       checkpointing did not run at start of session." >&2
    exit 5
fi
START_CKPT_ID="$(printf '%s\n' "$START_CKPT_LINE" | awk '{print $3}')"
green "Start-of-run checkpoint id: $START_CKPT_ID"

# Per-mutation snapshots must have fired for at least one Edit/Write call.
SNAP_HITS="$(grep -cE '^\[checkpoint\] (captured|tracked missing file)' "$FIX_LOG" || true)"
if [[ "$SNAP_HITS" -lt 1 ]]; then
    echo "error: no per-mutation snapshots captured." >&2
    echo "       expected '[checkpoint] captured <path> before change' lines." >&2
    exit 5
fi
green "Per-mutation snapshots captured: $SNAP_HITS"

# Bash `allow_with_warning` should have emitted at least one warning when
# the agent ran cargo test. The warning explains that Bash-driven file
# changes are NOT covered by checkpointing.
BASH_WARN_HITS="$(grep -cE '^\[warning\] ' "$FIX_LOG" || true)"
if [[ "$BASH_WARN_HITS" -lt 1 ]]; then
    yellow "note: no '[warning]' lines from Bash policy — agent may have skipped Bash."
else
    green "Bash policy warnings emitted: $BASH_WARN_HITS"
fi

# Validation must have run and passed. The CLI prints one
# `[validation] <command> -> exit=Some(<n>) timed_out=<bool> (<ms>ms)` per
# command, followed by `[validation] passed` or `[validation] failed`.
# (`[validation] running ...` is only emitted via CheckpointEvent rendering,
# which the post-run validation gate does not fire.)
if ! grep -qE '^\[validation\] .* -> exit=' "$FIX_LOG"; then
    echo "error: no per-command '[validation] ... -> exit=' line in fix log." >&2
    exit 5
fi
if ! grep -qE '^\[validation\] passed$' "$FIX_LOG"; then
    echo "error: validation did not pass." >&2
    exit 5
fi
green "Validation gate ran and passed."

# Sanity: the test suite is now passing on disk too.
cargo test --quiet 2>&1 | tail -n 5
green "cargo test passes after the agent's fix."

# ---------------------------------------------------------------------------
section "Step 6 — Inspect the checkpoint store (list / show / diff)"

cyan "checkpoints list --cwd . --session $FIX_SESSION:"
"$BIN" checkpoints list --cwd "$DEMO_DIR" --session "$FIX_SESSION"

CKPT_LIST_FILE="$WORK_DIR/ckpt-list.txt"
"$BIN" checkpoints list --cwd "$DEMO_DIR" --session "$FIX_SESSION" > "$CKPT_LIST_FILE"
CKPT_COUNT="$(wc -l < "$CKPT_LIST_FILE")"
gray "checkpoint count for session: $CKPT_COUNT"
if [[ "$CKPT_COUNT" -lt 1 ]]; then
    echo "error: checkpoint store is empty for session $FIX_SESSION" >&2
    exit 6
fi

cyan "checkpoints show $START_CKPT_ID (manifest, head):"
"$BIN" checkpoints show "$START_CKPT_ID" --cwd "$DEMO_DIR" --session "$FIX_SESSION" | head -n 30
gray "  ... (truncated)"

cyan "checkpoints diff $START_CKPT_ID (start-of-run vs current; should show the agent's edits):"
"$BIN" checkpoints diff "$START_CKPT_ID" --cwd "$DEMO_DIR" --session "$FIX_SESSION" | head -n 60
gray "  ... (truncated if longer)"

# ---------------------------------------------------------------------------
section "Step 7 — Create a manual checkpoint marking the post-fix state"

MANUAL_OUT="$WORK_DIR/manual.out"
"$BIN" checkpoint "post-fix manual checkpoint" \
    --cwd "$DEMO_DIR" --session "$FIX_SESSION" | tee "$MANUAL_OUT"
MANUAL_CKPT_ID="$(grep -oE '^\[checkpoint\] created [^[:space:]]+' "$MANUAL_OUT" | awk '{print $3}' | head -n 1)"
if [[ -z "$MANUAL_CKPT_ID" ]]; then
    echo "error: failed to parse manual checkpoint id from output." >&2
    exit 7
fi
green "Manual checkpoint id: $MANUAL_CKPT_ID"

# ---------------------------------------------------------------------------
section "Step 8 — Dry-run rewind (preview), then real rewind to the broken state"

cyan "rewind --dry-run $START_CKPT_ID (preview restore actions):"
"$BIN" rewind "$START_CKPT_ID" --cwd "$DEMO_DIR" --dry-run

LIB_HASH_AFTER_FIX="$(hash_file "$DEMO_DIR/src/lib.rs")"
gray "src/lib.rs git-hash (post-fix): $LIB_HASH_AFTER_FIX"
if [[ "$LIB_HASH_AFTER_FIX" == "$LIB_HASH_BROKEN" ]]; then
    echo "error: src/lib.rs unchanged after the fix run; rewind would be a no-op." >&2
    exit 8
fi

cyan "rewind --yes $START_CKPT_ID (actually restore):"
"$BIN" rewind "$START_CKPT_ID" --cwd "$DEMO_DIR" --yes

LIB_HASH_AFTER_REWIND="$(hash_file "$DEMO_DIR/src/lib.rs")"
gray "src/lib.rs git-hash (post-rewind): $LIB_HASH_AFTER_REWIND"
if [[ "$LIB_HASH_AFTER_REWIND" != "$LIB_HASH_BROKEN" ]]; then
    echo "error: rewind did not restore src/lib.rs to the buggy starting state." >&2
    echo "       expected $LIB_HASH_BROKEN, got $LIB_HASH_AFTER_REWIND" >&2
    exit 8
fi
green "Rewind restored src/lib.rs byte-for-byte to the buggy starting state."

# Confirm the failing test is back: rewind reverted the agent's fix.
set +e
cargo test --quiet 2>&1 | tail -n 5
TEST_EXIT_AFTER_REWIND=${PIPESTATUS[0]}
set -e
if [[ "$TEST_EXIT_AFTER_REWIND" -eq 0 ]]; then
    echo "error: tests still pass after rewind; the fix was not actually reverted." >&2
    exit 8
fi
green "cargo test fails again after rewind — workspace is back to the start."

# ---------------------------------------------------------------------------
section "Step 9 — Auto-rewind on validation failure (--rewind-on-validation-failure)"

cyan "Command:"
gray "  crom run \"...add a top-of-file comment...\" --cwd . \\"
gray "      --checkpointing on \\"
gray "      --permission-mode acceptEdits \\"
gray "      --validate \"false\" --rewind-on-validation-failure \\"
gray "      --max-turns 4 --show-usage"

LIB_HASH_PRE_AUTO="$(hash_file "$DEMO_DIR/src/lib.rs")"

AUTO_LOG="$WORK_DIR/auto.log"
# `--validate "false"` makes the post-run gate fail unconditionally
# regardless of what the agent did, so the auto-rewind path is exercised
# deterministically. The agent is asked to make any small Edit so the
# rewind has work to undo (otherwise the start-of-run checkpoint and
# current files match and the rewind is a no-op).
set +e
"$BIN" run \
    "Edit src/lib.rs to add the comment '// reviewed' as the very first line, but DO NOT change any other code, and DO NOT run cargo. Stop after that single Edit." \
    --cwd "$DEMO_DIR" \
    --checkpointing on \
    --permission-mode acceptEdits \
    --validate "false" \
    --rewind-on-validation-failure \
    --max-turns 4 \
    --show-usage 2>&1 | tee "$AUTO_LOG"
AUTO_EXIT=${PIPESTATUS[0]}
set -e

# The CLI exit code is non-zero on validation failure; that's expected.
gray "crom exited with $AUTO_EXIT (non-zero is expected when validation fails)."

if ! grep -qE '^\[validation\] failed$' "$AUTO_LOG"; then
    echo "error: '--validate \"false\"' did not produce '[validation] failed'." >&2
    exit 9
fi
if ! grep -qE '^\[rewind\] validation failed; rewinding to ' "$AUTO_LOG"; then
    echo "error: auto-rewind did not trigger after validation failure." >&2
    exit 9
fi
green "Validation failed and auto-rewind fired."

LIB_HASH_POST_AUTO="$(hash_file "$DEMO_DIR/src/lib.rs")"
if [[ "$LIB_HASH_POST_AUTO" != "$LIB_HASH_PRE_AUTO" ]]; then
    echo "error: src/lib.rs not restored by auto-rewind." >&2
    echo "       expected $LIB_HASH_PRE_AUTO, got $LIB_HASH_POST_AUTO" >&2
    exit 9
fi
green "src/lib.rs unchanged after the failed run — auto-rewind reverted the agent's edit."

# ---------------------------------------------------------------------------
section "Step 10 — On-disk content-addressed checkpoint layout"

CKPT_ROOT="$DEMO_DIR/.agent/checkpoints"
if [[ ! -d "$CKPT_ROOT" ]]; then
    echo "error: expected $CKPT_ROOT to exist." >&2
    exit 10
fi

cyan "Top-level checkpoint dirs (one per session):"
ls -1 "$CKPT_ROOT"

# Find any session dir that has a manifests/ + blobs/ pair. Multiple
# sessions may exist (the auto-rewind run created its own session).
for SESSION_DIR in "$CKPT_ROOT"/*/; do
    [[ -d "$SESSION_DIR" ]] || continue
    SESSION_NAME="$(basename "$SESSION_DIR")"
    INDEX="$SESSION_DIR/checkpoints.jsonl"
    MANIFESTS="$SESSION_DIR/manifests"
    BLOBS="$SESSION_DIR/blobs"
    [[ -f "$INDEX" ]] || continue

    cyan "Session $SESSION_NAME:"
    INDEX_LINES="$(wc -l < "$INDEX")"
    MANIFEST_COUNT="$(find "$MANIFESTS" -maxdepth 1 -name '*.json' 2>/dev/null | wc -l)"
    BLOB_COUNT="$(find "$BLOBS" -maxdepth 1 -type f 2>/dev/null | wc -l)"
    # Use `wc -c` to sum blob bytes portably; `du -sb` is GNU-only
    # (BSD/macOS `du` rejects `-b`).
    if [[ "$BLOB_COUNT" -gt 0 ]]; then
        BLOB_BYTES="$(find "$BLOBS" -maxdepth 1 -type f -exec wc -c {} + 2>/dev/null \
            | awk 'END{print $1+0}')"
    else
        BLOB_BYTES=0
    fi
    gray "  checkpoints.jsonl: $INDEX_LINES record(s)"
    gray "  manifests/: $MANIFEST_COUNT manifest(s)"
    gray "  blobs/: $BLOB_COUNT content-addressed blob(s) (${BLOB_BYTES} bytes)"
done

# ---------------------------------------------------------------------------
section "Done"
cat <<EOF
M3 demo complete. Summary of what just happened:

  * Built the crom CLI (debug profile).
  * Loaded $DEMO_DIR/.agent/settings.json — checkpointing.enabled=true,
    bash_policy=allow_with_warning, validation.commands=["cargo test"].
  * Confirmed cargo test failed in the buggy starting state.
  * Ran the agent under --checkpointing on with a --validate "cargo test"
    gate; observed:
      - start-of-run checkpoint created before any tool call ($START_CKPT_ID),
      - per-mutation snapshots captured before Edit/Write tool calls
        ($SNAP_HITS event(s)),
      - $BASH_WARN_HITS bash_policy warning(s) for shell calls,
      - validation ran cargo test and passed.
  * Inspected the store via 'checkpoints list / show / diff' and saw
    $CKPT_COUNT checkpoint(s) for session $FIX_SESSION.
  * Created a manual checkpoint marking the post-fix state ($MANUAL_CKPT_ID).
  * Previewed a rewind via --dry-run, then rewound to the start-of-run
    checkpoint and verified src/lib.rs returned byte-for-byte to the buggy
    starting state (git-hash $LIB_HASH_BROKEN). Conversation history was
    intentionally NOT rewound — only files were restored.
  * Drove the auto-rewind path: --validate "false"
    --rewind-on-validation-failure caused the post-run gate to fail and
    the workspace to be rewound automatically; src/lib.rs was unchanged
    after the failed run.
  * Inspected the on-disk content-addressed layout under
    $CKPT_ROOT.

To replay from scratch, just run this script again — Step 1 resets state.
EOF
