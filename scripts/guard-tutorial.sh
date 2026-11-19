#!/usr/bin/env bash
# Runs docs/guard/tutorial.md (#364): every ```sh block, in order, in one
# shell, except a block whose previous non-blank line is
# `<!-- tutorial: skip -->`. Then checks what the tutorial says happens.
#
#   scripts/guard-tutorial.sh           run it (from any directory)
#   scripts/guard-tutorial.sh --print   print the script it would run
#
# The doc is the only copy of the commands, so they cannot drift apart. It
# fails before running anything if the doc has a fenced block that is not
# ```sh, or a number of skipped blocks other than SKIPPED. TUTORIAL_DOC
# overrides the doc path (used to check that a broken copy fails).
# Needs Linux with the Crom Bash sandbox, cargo, curl and jq, and free
# loopback ports 8470 and 8480.
set -euo pipefail
cd "$(dirname "$0")/.."

doc=${TUTORIAL_DOC:-docs/guard/tutorial.md}
# The clone block and the paid live run.
SKIPPED=2

# Prints the runnable blocks; exits 1 on a fence that is not ```sh.
extract() {
  awk '
    !inblock && /^[[:space:]]*```/ {
      if ($0 !~ /^```sh[[:space:]]*$/) { printf "error: line %d: only ```sh blocks are allowed: %s\n", NR, $0 > "/dev/stderr"; bad = 1; exit 1 }
      inblock = 1; run = !skip; skip = 0; n++
      if (run) printf "\n# --- block %d ---\n", n
      else skipped++
      next
    }
    inblock && /^```[[:space:]]*$/ { inblock = 0; next }
    inblock { if (run) print; next }
    /^<!-- tutorial: skip -->[[:space:]]*$/ { skip = 1; next }
    /[^[:space:]]/ { skip = 0 }
    END { if (!bad) printf "\n# skipped blocks: %d\n", skipped }
  ' "$doc"
}

# Checks appended after the doc's blocks. The doc's clean-up block deletes
# $WORK, so script() copies the evidence to $EVIDENCE just before it.
checks() {
  cat <<'EOF'

# --- checks (scripts/guard-tutorial.sh) ---
fail() { echo "tutorial check failed: $*" >&2; exit 1; }
e=$EVIDENCE
# Steps 4 and 5: what the agent saw, call by call.
want='executed
resource_not_found
policy_deny
invalid_arguments
policy_deny
policy_deny
approval_required
action_not_granted
no_grant
executed
grant_revoked
no_grant
policy_deny
policy_deny
executed'
got=$(jq -r '.code // .status' "$e/calls.log")
[[ "$got" == "$want" ]] || fail "agent-visible results differ:
$got"
# Step 4: one decision record per call, in order.
want='invoice.list allow -
invoice.list deny resource_not_found
invoice.list deny policy_deny
invoice.list deny invalid_arguments
customer.export deny policy_deny
refund.issue deny policy_deny
refund.issue approval_required -
- deny unknown_action
- deny no_grant
refund.issue allow -
- deny grant_revoked'
got=$(jq -r 'select(.record == "decision") | "\(.action // "-") \(.decision) \(.reason.reason // "-")"' "$e/audit.jsonl" | head -n 11)
[[ "$got" == "$want" ]] || fail "step 4 decisions differ:
$got"
[[ $(wc -l <"$e/ledger-1.jsonl") -eq 2 ]] || fail "step 4: expected 2 ledger effects"
# Step 5: the old handle is dead, and the new policy decides the refunds.
want='- deny no_grant
refund.issue deny policy_deny small-refunds-only
refund.issue deny policy_deny small-refunds-only
refund.issue allow - refund-within-limits'
got=$(jq -r 'select(.record == "decision") | "\(.action // "-") \(.decision) \(.reason.reason // "-")\(.determining_policies | map(" " + .) | join(""))"' \
  "$e/audit.jsonl" | tail -n 4)
[[ "$got" == "$want" ]] || fail "step 5 decisions differ:
$got"
[[ $(jq -r 'select(.policy_set_hash) | .policy_set_hash' "$e/audit.jsonl" | uniq | wc -l) -eq 2 ]] || fail "step 5: expected two policy set hashes"
[[ $(jq -r .kind "$e/ledger-2.jsonl") == refund ]] || fail "step 5: expected one refund effect"
# Step 6: check passed, then failed on sandbox-network only.
grep -q '^ok: deployment matches the isolation profile' "$e/check-ok.txt" || fail "step 6: check did not pass"
! grep -q '^FAIL' "$e/check-ok.txt" || fail "step 6: FAIL in the passing check"
[[ $(grep '^FAIL' "$e/check-network.txt" | cut -d: -f2 | tr -d ' ' | sort -u | tr '\n' ' ') == "sandbox-network " ]] \
  || fail "step 6: unexpected failing checks: $(grep '^FAIL' "$e/check-network.txt")"
echo "ok: tutorial ran and every check passed"
EOF
}

script() {
  cat <<'EOF'
EVIDENCE=$(mktemp -d)
trap 'kill $(jobs -p) 2>/dev/null || true; rm -rf "$EVIDENCE"' EXIT
EOF
  extract | awk '
    /^rm -rf "\$WORK"$/ { print "cp \"$STATE/audit.jsonl\" \"$AGENT/calls.log\" \"$WORK\"/ledger-*.jsonl \"$WORK\"/check-*.txt \"$EVIDENCE\"/" }
    { print }
  '
  checks
}

extracted=$(extract) || exit 1
blocks=$(grep -c '^# --- block' <<<"$extracted" || true)
skipped=$(sed -n 's/^# skipped blocks: //p' <<<"$extracted")
if [[ "$skipped" != "$SKIPPED" ]]; then
  echo "error: $doc has $skipped skipped sh blocks, expected $SKIPPED" >&2
  exit 1
fi
if ((blocks < 20)); then
  echo "error: only $blocks runnable sh blocks found in $doc" >&2
  exit 1
fi
# shellcheck disable=SC2016 # a literal $WORK
if ! grep -q '^rm -rf "\$WORK"$' <<<"$extracted"; then
  echo "error: $doc has no clean-up block (rm -rf \"\$WORK\")" >&2
  exit 1
fi

if [[ "${1:-}" == "--print" ]]; then
  script
  exit 0
fi
bash -euo pipefail -c "$(script)"
