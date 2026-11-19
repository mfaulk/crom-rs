#!/usr/bin/env bash
# Runs the Python reference client against a real Guard (#358).
#
# Starts crom-guard-support (Guard + the testkit business API), issues grants
# through the admin socket as the trusted app, runs scripted_agent.py with a
# scrubbed environment, then checks that the agent never held the business-API
# credential and that the audit log verifies.
#
# Env: PYTHON (default python3), CROM_GUARD_SUPPORT_BIN (skip the cargo build),
# GUARD_CLIENT_OUT (artifact dir, default target/guard-python-client).
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
HERE="$ROOT/examples/guard-support"
CLIENT="$HERE/clients/python"
OUT=${GUARD_CLIENT_OUT:-$ROOT/target/guard-python-client}
PY=$(command -v "${PYTHON:-python3}")
"$PY" -c 'import sys; sys.exit(sys.version_info < (3, 10))' \
  || { echo "error: Python 3.10 or newer is required" >&2; exit 1; }

if [[ -z "${CROM_GUARD_SUPPORT_BIN:-}" ]]; then
  cargo build --locked -q -p crom-guard-testkit --example crom-guard-support \
    --manifest-path "$ROOT/Cargo.toml"
  target=$(cargo metadata --format-version 1 --no-deps --manifest-path "$ROOT/Cargo.toml" \
    | "$PY" -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
  CROM_GUARD_SUPPORT_BIN="$target/debug/examples/crom-guard-support"
fi
BIN=$CROM_GUARD_SUPPORT_BIN

umask 077
STATE=$(mktemp -d)    # Guard only: config, admin token, credentials, audit log
AGENT=$(mktemp -d)    # the agent's files: its handles and the approval inbox
ORACLE=$(mktemp -d)   # business-API ledger, read by the scripted agent as a test oracle
GUARD_PID=
APPROVER_PID=
cleanup() {
  [[ -n "$APPROVER_PID" ]] && kill "$APPROVER_PID" 2>/dev/null || true
  if [[ -n "$GUARD_PID" ]]; then
    kill -TERM "$GUARD_PID" 2>/dev/null || true
    wait "$GUARD_PID" 2>/dev/null || true
  fi
  rm -rf "$STATE" "$AGENT" "$ORACLE"
}
trap cleanup EXIT

free_port() {
  "$PY" -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'
}
AGENT_PORT=$(free_port)
API_PORT=$(free_port)

cp "$HERE/guard.toml" "$STATE/"
cp -r "$HERE/policies" "$STATE/"
"$PY" -c 'import secrets; print(secrets.token_urlsafe(32))' >"$STATE/admin.token"
SECRET="sk-business-$("$PY" -c 'import secrets; print(secrets.token_hex(16))')"
"$PY" - "$STATE/credentials.json" "$SECRET" <<'EOF'
import json, sys
actions = ["customer.read", "invoice.list", "refund.issue", "customer.export"]
with open(sys.argv[1], "w") as f:
    json.dump({"credentials": [{"id": "business-api", "secret": sys.argv[2], "actions": actions}]}, f)
EOF

CROM_GUARD_SUPPORT_CREDENTIALS="$STATE/credentials.json" \
CROM_GUARD_SUPPORT_API_LISTEN="127.0.0.1:$API_PORT" \
CROM_GUARD_SUPPORT_LEDGER="$ORACLE/ledger.jsonl" \
  "$BIN" serve --config "$STATE/guard.toml" --agent-listen "127.0.0.1:$AGENT_PORT" \
  >"$STATE/guard.log" 2>&1 &
GUARD_PID=$!

admin() { "$PY" "$CLIENT/trusted_app.py" --admin "unix:$STATE/admin.sock" --token-file "$STATE/admin.token" "$@"; }
for _ in $(seq 1 100); do
  admin health >/dev/null 2>&1 && break
  kill -0 "$GUARD_PID" 2>/dev/null || { cat "$STATE/guard.log" >&2; echo "error: Guard exited" >&2; exit 1; }
  sleep 0.1
done
admin health

# An earlier run's grant, revoked: its handle is what a replay attack presents.
old=$(admin issue --spec "$HERE/grants/acme-agent-operator.json" --run-id run-previous \
  --handle-out "$AGENT/previous.handle")
admin revoke "$("$PY" -c 'import json, sys; print(json.loads(sys.argv[1])["grant_id"])' "$old")" >/dev/null
admin issue --spec "$HERE/grants/acme-agent-operator.json" --run-id run-python-client \
  --handle-out "$AGENT/session.handle" >/dev/null

mkdir -p "$AGENT/inbox/requests" "$AGENT/inbox/approved"
admin approve-pending --inbox "$AGENT/inbox" --count 1 --expect-action refund.issue \
  --approver-id acme-supervisor --approver-role supervisor --approver-limit 100000 \
  >"$STATE/approver.log" 2>&1 &
APPROVER_PID=$!

# The agent gets an empty environment, its own directory, and no path into $STATE.
(
  cd "$AGENT"
  exec env -i PATH=/usr/bin:/bin HOME="$AGENT" "$PY" -E -s "$CLIENT/scripted_agent.py" \
    --url "http://127.0.0.1:$AGENT_PORT/mcp" \
    --handle-file "$AGENT/session.handle" \
    --replayed-handle-file "$AGENT/previous.handle" \
    --api-url "http://127.0.0.1:$API_PORT" \
    --ledger "$ORACLE/ledger.jsonl" \
    --approval-inbox "$AGENT/inbox" \
    --report "$AGENT/report.json"
) >"$AGENT/agent.log" 2>&1 &
AGENT_PID=$!
# The business-API secret is never in the agent's environment.
if [[ -r "/proc/$AGENT_PID/environ" ]] && grep -qaF "$SECRET" "/proc/$AGENT_PID/environ"; then
  echo "error: business-API credential found in the agent's environment" >&2; exit 1
fi
status=0
wait "$AGENT_PID" || status=$?
cat "$AGENT/agent.log"
wait "$APPROVER_PID" || { cat "$STATE/approver.log" >&2; status=1; }
APPROVER_PID=

fail() { echo "error: $*" >&2; status=1; }
# Nothing the agent holds or printed contains the business-API credential.
grep -rqaF "$SECRET" "$AGENT" "$ORACLE" && fail "business-API credential found in the agent's files"
# Secrets never reach any log.
token=$(cat "$STATE/admin.token")
for f in "$AGENT/agent.log" "$STATE/guard.log" "$STATE/approver.log"; do
  for s in "$SECRET" "$token" "$(cat "$AGENT/session.handle")" "$(cat "$AGENT/previous.handle")"; do
    grep -qaF "$s" "$f" && fail "a secret appears in $(basename "$f")"
  done
done
"$BIN" audit verify "$STATE/audit.jsonl" || fail "audit log did not verify"

mkdir -p "$OUT"
cp "$AGENT/report.json" "$ORACLE/ledger.jsonl" "$STATE/audit.jsonl" "$AGENT/agent.log" "$OUT/" 2>/dev/null || true
echo "artifacts: $OUT"
exit "$status"
