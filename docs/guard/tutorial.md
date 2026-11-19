# Crom Guard tutorial

[#364](https://github.com/mfaulk/crom-rs/issues/364). Spec: [MILESTONE_18](../milestones/MILESTONE_18.md) §14 and §20. This walkthrough runs offline and needs no API key. You will:

1. build Guard with the support reference operations;
2. run the scripted reference demo and read its matrix;
3. read the audit log and the business ledger, join them, and run `audit verify` (and watch it reject a tampered log);
4. start Guard yourself, act as the trusted app and as the agent with `curl`, and trigger allowed and denied calls;
5. write a new Cedar policy, load it, and see it change Guard's decisions;
6. run the `crom-guard check` isolation preflight, and see it fail on a weaker agent configuration.

The pieces: Guard and the synthetic business API run in one process, `crom-guard-support` ([reference-deployment.md](reference-deployment.md)). The trusted application talks to Guard's admin API on a unix socket. The agent talks MCP Streamable HTTP to Guard's agent endpoint and never holds the business-API credential. The business API writes an effect ledger, which is the oracle for "did anything happen".

## Before you start

- Linux with landlock ABI 3 or newer (kernel 6.2+) and unprivileged user namespaces. The scripted demo runs Crom's Bash tool with `sandbox.mode = "required"` and fails closed without them ([sandbox.md](../security/sandbox.md)). Steps 4–6 do not need the sandbox.
- Rust 1.89 or newer (`rust-version` in the root `Cargo.toml`), `git`, `curl`, and `jq`.
- Free loopback ports 8470 (Guard's agent endpoint, from `examples/guard-support/guard.toml`) and 8480 (the business API's default) for steps 4–6.

Run every block in one `bash` shell, in order, from the repository root: later blocks use variables that earlier blocks set. The first build takes a few minutes. A few blocks end with `test …` or `grep -q …` lines: they check the result, print nothing when it holds, and are what makes the CI run below fail when the tutorial stops matching Guard.

**Kept in sync by CI.** `scripts/guard-tutorial.sh` extracts every `sh` block on this page, except the two marked as skipped in the page source (cloning, and the paid live run at the end), runs them in order under `bash -euo pipefail`, and then checks the results (the agent-visible codes and audit log of step 4, the policy change of step 5, the `check` results of step 6). It fails on any other kind of code block, or a different number of skipped ones. CI runs it in the `guard / reference app (linux)` job. `scripts/guard-tutorial.sh --print` shows the script it runs.

**Secrets stay off command lines.** A process's arguments are visible to every local user (`ps`, `/proc/<pid>/cmdline`), so the admin token, the business-API secret and the session handle below live in 0600 files and reach `curl` and `jq` through `-H @file` or stdin, never as arguments.

## 1. Get the code and build Guard

<!-- tutorial: skip -->
```sh
git clone https://github.com/mfaulk/crom-rs.git
cd crom-rs
```

The stock `crom-guard` binary registers no protected operations ([server.md](server.md)). `crom-guard-support` is the same CLI with the support operations (`customer.read`, `invoice.list`, `refund.issue`, `customer.export`, `guard.delegate`) and the business API compiled in:

```sh
cargo build --locked -p crom-guard-testkit --example crom-guard-support
GUARD="$(cargo metadata --format-version 1 --no-deps | jq -r .target_directory)/debug/examples/crom-guard-support"
ls -l "$GUARD"
```

`GUARD` follows `CARGO_TARGET_DIR` if you set it. Its subcommands are `serve`, `policy validate`, `audit verify` and `check`.

## 2. Run the scripted demo

```sh
cargo run --manifest-path examples/guard-support/Cargo.toml
```

One command runs `crom-guard check`, starts Guard and the business API from a private 0700 state directory, opens a support ticket as the trusted app (a grant for the acme operator, customer `acme-cus-000`, refunds up to 100.00 with a 50.00 operator limit), and drives a Crom agent with a scripted model through the MILESTONE_18 §14.2 matrix. It exits 0 only if every row and every evidence check passes. Details: [reference-app.md](reference-app.md).

```sh
DEMO=target/guard-demo
cat "$DEMO/matrix.txt"
```

Each row is one attempted action: who made it (`crom` is the agent, `support-app` the trusted app), what it was, what was expected, what Guard (or Crom, or the sandbox) did, and the ledger effect. The rows to look at first:

| Row | Observed | Meaning |
|---|---|---|
| `invoice.list globex-cus-000` | `resource_not_found` | Another tenant's customer looks exactly like a customer that does not exist |
| `invoice.list acme-cus-001 (not in grant)` | `policy_deny` | Same tenant, but the grant names only `acme-cus-000` |
| `refund.issue acme-inv-001 7500 (supervisor approves)` | `approval_required->executed` | Above the operator's 50.00 limit; executed only with a supervisor approval the app issued for that exact call |
| `customer.export acme-cus-000` | `policy_deny` | The `no-bulk-export` forbid |
| `invoice.list with tenant_id=globex` | `invalid_arguments` | The schema has no tenant field; a forged one is rejected before anything is resolved |
| `Bash: read Guard's credential file` | `blocked` | The OS sandbox, not Guard, stops this |
| `Crom run with the closed ticket's handle` | `refused(grant_revoked)` | The app revoked the grant when the ticket closed |

The evidence checks the driver ran:

```sh
jq -r '.checks[] | "\(if .pass then "PASS" else "FAIL" end)  \(.check)"' "$DEMO/report.json"
```

## 3. Read the audit log and the ledger

Guard writes one hash-chained `guard-audit/v1` record per decision, and one more per outcome when a call executes ([audit.md](audit.md)). The business API writes one ledger line per effect, carrying the Guard decision id it was called with.

```sh
jq -c '{seq, record, action, resource: .resource.id, decision, reason: .reason.reason, outcome}' "$DEMO/audit.jsonl"
jq -c '{seq, kind, guard_decision_id}' "$DEMO/ledger.jsonl"
```

Join the refund in the ledger to Guard's records for it. The decision record says why it was allowed (`determining_policies`), and the outcome record says it executed:

```sh
REFUND_ID=$(jq -r 'select(.kind == "refund") | .guard_decision_id' "$DEMO/ledger.jsonl" | head -n 1)
jq -c --arg id "$REFUND_ID" \
  'select(.decision_id == $id) | {seq, record, action, args_logged, decision, determining_policies, outcome}' \
  "$DEMO/audit.jsonl"
```

The approved refund names its approver, copied from the app's admin call, never from the agent:

```sh
jq -c 'select(.approver) | {record, action, args_logged, approver, outcome}' "$DEMO/audit.jsonl"
```

Verify the chain. `audit verify` exits 0 and prints the head:

```sh
"$GUARD" audit verify "$DEMO/audit.jsonl"
```

Now delete one record from a copy and verify again. The deletion breaks the `seq`/`prev_hash` chain, and `audit verify` exits 1 naming the line:

```sh
WORK=$(mktemp -d)
sed '2d' "$DEMO/audit.jsonl" >"$WORK/tampered.jsonl"
status=0
"$GUARD" audit verify "$WORK/tampered.jsonl" 2>"$WORK/verify.err" || status=$?
cat "$WORK/verify.err"
echo "exit status: $status"
test "$status" -eq 1
grep -q 'audit log line 2: expected seq 2, found 3' "$WORK/verify.err"
```

`WORK` is a scratch directory for the rest of the tutorial. The chain is tamper evidence, not tamper proofing: a process with Guard's uid could rewrite the whole chain consistently. Only a head hash kept elsewhere (`--anchor <seq>:<hash>`) detects that ([audit.md](audit.md)).

## 4. Run Guard yourself and trigger denials

Set up a private state directory the way the deployment profile wants it: 0700, with a 0600 admin token and a 0600 credentials file. The agent never sees either file. The random secrets are generated into files and piped through `jq` on stdin:

```sh
STATE="$WORK/state"
mkdir -m 0700 "$STATE"
cp examples/guard-support/guard.toml "$STATE/"
cp -r examples/guard-support/policies "$STATE/"
(umask 077; head -c 24 /dev/urandom | base64 | tr '+/' '-_' >"$STATE/admin.token")
(umask 077; head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' |
  jq -Rn '{credentials: [{id: "business-api", secret: ("sk-business-" + input),
    actions: ["customer.read", "invoice.list", "refund.issue", "customer.export"]}]}' \
  >"$STATE/credentials.json")
ls -l "$STATE"
```

Start Guard in the background. It also starts the business API on 127.0.0.1:8480, which accepts only the secret in `credentials.json`:

```sh
CROM_GUARD_SUPPORT_CREDENTIALS="$STATE/credentials.json" \
CROM_GUARD_SUPPORT_LEDGER="$WORK/ledger-1.jsonl" \
  "$GUARD" serve --config "$STATE/guard.toml" >"$WORK/guard-1.log" 2>&1 &
GUARD_PID=$!
APP="$WORK/app"
mkdir -m 0700 "$APP"
(umask 077; printf 'Authorization: Bearer %s\n' "$(cat "$STATE/admin.token")" >"$APP/admin.header")
admin() {
  curl -sS --unix-socket "$STATE/admin.sock" -H @"$APP/admin.header" "$@"
}
for _ in $(seq 50); do admin http://localhost/admin/health 2>/dev/null && break; sleep 0.2; done
echo
```

The health check prints `{"status":"ok","operations":5}`. `admin` is the trusted application: it holds the admin token (in a 0600 header file; `printf` is a shell builtin, so the token is never an argument) and reaches the admin API through the unix socket.

As the trusted app, open a ticket: issue the grant in `examples/guard-support/grants/acme-agent-operator.json` and hand the session handle (`gsh_…`) to the agent, as a 0600 header file in the agent's own directory. The handle is the agent's only credential:

```sh
AGENT="$WORK/agent"
mkdir -m 0700 "$AGENT"
GRANT=$(admin -H 'Content-Type: application/json' \
  --data @examples/guard-support/grants/acme-agent-operator.json http://localhost/admin/grants)
echo "$GRANT" | jq '.grant | {grant_id, tenant, delegator, actions, constraints, expires_at}'
(umask 077; echo "$GRANT" | jq -r '"Authorization: Bearer " + .handle' >"$AGENT/handle.header")
GRANT_ID=$(echo "$GRANT" | jq -r .grant.grant_id)
```

Now be the agent. Any MCP client works; `curl` is enough, because the endpoint is stateless MCP Streamable HTTP with the handle as a bearer token. `guard_call` sends one `tools/call` with the header file named by `AGENT_HEADER`, prints `executed` or the stable deny code with the decision id, and also appends that line to `$AGENT/calls.log`. It fails if Guard returns no `result` (a JSON-RPC error):

```sh
AGENT_HEADER="$AGENT/handle.header"
guard_call() {
  curl -sS http://127.0.0.1:8470/mcp \
    -H @"$AGENT_HEADER" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    --data "$(jq -cn --arg tool "$1" --argjson args "$2" \
      '{jsonrpc: "2.0", id: 1, method: "tools/call", params: {name: $tool, arguments: $args}}')" |
    jq -ec '(.result // error("no result: \(tojson)")) | {decision_id: ._meta.decision_id} +
      (if .isError then .structuredContent else {status: "executed"} end)' |
    tee -a "$AGENT/calls.log"
}
```

Allowed: the granted customer's invoices.

```sh
guard_call invoice.list '{"customer_id": "acme-cus-000"}'
```

Denied, each for a different reason:

```sh
guard_call invoice.list '{"customer_id": "globex-cus-000"}'                         # other tenant: resource_not_found
guard_call invoice.list '{"customer_id": "acme-cus-001"}'                          # not in the grant: policy_deny
guard_call invoice.list '{"customer_id": "acme-cus-000", "tenant_id": "globex"}'   # forged tenant: invalid_arguments
guard_call customer.export '{"customer_id": "acme-cus-000"}'                       # no-bulk-export: policy_deny
guard_call refund.issue '{"invoice_id": "acme-inv-000", "amount_cents": 10001}'    # more than the invoice: policy_deny
guard_call refund.issue '{"invoice_id": "acme-inv-001", "amount_cents": 7500}'     # above 50.00: approval_required
guard_call invoice.lst '{"customer_id": "acme-cus-000"}'                           # unknown tool: action_not_granted
printf 'Authorization: Bearer gsh_not-a-real-handle\n' >"$AGENT/fake.header"
AGENT_HEADER="$AGENT/fake.header" guard_call invoice.list '{"customer_id": "acme-cus-000"}'  # no_grant
```

`approval_required` also returns the `action_digest` the app would approve; [approvals.md](approvals.md) and the demo in step 2 show the full approval round trip.

Allowed: a 30.00 refund, within the 50.00 operator limit and the 100.00 grant cap.

```sh
guard_call refund.issue '{"invoice_id": "acme-inv-000", "amount_cents": 3000}'
```

Try to go around Guard. The business API refuses a call without Guard's credential, and the session handle is not that credential:

```sh
REFUND_URL=http://127.0.0.1:8480/tenants/acme/invoices/acme-inv-000/refunds
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  --data '{"amount_cents": 100}' "$REFUND_URL")
echo "no credential: $code"
test "$code" = 401
code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' \
  -H @"$AGENT/handle.header" --data '{"amount_cents": 100}' "$REFUND_URL")
echo "session handle as bearer: $code"
test "$code" = 401
```

Both print `401`. Here the agent runs unsandboxed on your uid, so it could read `credentials.json` itself; keeping it from doing that is the isolation profile's job (step 6, [deployment.md](deployment.md)).

Close the ticket. Every later call with the handle is `grant_revoked`:

```sh
admin -X POST "http://localhost/admin/grants/$GRANT_ID/revoke"; echo
guard_call invoice.list '{"customer_id": "acme-cus-000"}'
```

Read what happened. Every call above has a decision record; only the two executed calls have an outcome record and a ledger entry, and the ledger entries carry their decision ids:

```sh
jq -c '{seq, record, action, decision, reason: .reason.reason, outcome}' "$STATE/audit.jsonl"
jq -c '{kind, guard_decision_id}' "$WORK/ledger-1.jsonl"
"$GUARD" audit verify "$STATE/audit.jsonl"
```

Denials of unauthenticated or unknown calls are recorded too, with no action (Guard never resolved one). The misspelled tool is `unknown_action` in the audit log, while the agent saw only `action_not_granted`, so a session cannot probe which operations exist. The direct API calls have no record and no ledger entry: they never reached an operation.

## 5. Write a new policy and load it

Suppose the business wants agents to refund at most 20.00 per call, with no approval route above that. Add a `forbid` to the policy set. A `forbid` overrides every `permit`, including the supervisor-approval permit, so above 20.00 the answer becomes `policy_deny` rather than `approval_required`.

First, see Guard refuse a policy that does not validate. `context.amount` is a typo for `context.amount_cents`; strict validation against the schema rejects it, and `serve` would refuse to start with it:

```sh
cp "$STATE/policies/support.cedar" "$WORK/typo.cedar"
cat >>"$WORK/typo.cedar" <<'EOF'

@id("small-refunds-only")
forbid (principal, action == Support::Action::"refund.issue", resource)
when { context.amount > 2000 };
EOF
status=0
"$GUARD" policy validate --namespace Support \
  --schema "$STATE/policies/support.cedarschema" --policies "$WORK/typo.cedar" 2>"$WORK/typo.err" || status=$?
cat "$WORK/typo.err"
echo "exit status: $status"
test "$status" -eq 1
grep -q 'strict validation failed: .*attribute `amount` in context for Support::Action::' "$WORK/typo.err"
```

Now the correct policy, appended to the policy file Guard loads:

```sh
cat >>"$STATE/policies/support.cedar" <<'EOF'

@id("small-refunds-only")
forbid (principal, action == Support::Action::"refund.issue", resource)
when { context.amount_cents > 2000 };
EOF
"$GUARD" policy validate --namespace Support \
  --schema "$STATE/policies/support.cedarschema" --policies "$STATE/policies/support.cedar"
```

Guard loads policies once, at start; there is no hot reload ([policies.md](policies.md)). Restart it. Grants live in memory, so a restart also invalidates every handle (fails closed, [grants.md](grants.md#restart-semantics-decided)); the business API's data resets too, and this run gets its own ledger file:

```sh
kill "$GUARD_PID"; wait "$GUARD_PID" || true
CROM_GUARD_SUPPORT_CREDENTIALS="$STATE/credentials.json" \
CROM_GUARD_SUPPORT_LEDGER="$WORK/ledger-2.jsonl" \
  "$GUARD" serve --config "$STATE/guard.toml" >"$WORK/guard-2.log" 2>&1 &
GUARD_PID=$!
for _ in $(seq 50); do admin http://localhost/admin/health 2>/dev/null && break; sleep 0.2; done
echo
guard_call invoice.list '{"customer_id": "acme-cus-000"}'   # the old handle: no_grant
```

Open a new ticket and repeat the refunds from step 4:

```sh
GRANT=$(admin -H 'Content-Type: application/json' \
  --data @examples/guard-support/grants/acme-agent-operator.json http://localhost/admin/grants)
(umask 077; echo "$GRANT" | jq -r '"Authorization: Bearer " + .handle' >"$AGENT/handle.header")
guard_call refund.issue '{"invoice_id": "acme-inv-000", "amount_cents": 3000}'   # was executed, now policy_deny
guard_call refund.issue '{"invoice_id": "acme-inv-001", "amount_cents": 7500}'   # was approval_required, now policy_deny
guard_call refund.issue '{"invoice_id": "acme-inv-000", "amount_cents": 1500}'   # still executed
```

The audit log continued the same chain across the restart. Every record of a Cedar evaluation names the hash of the policy set that made it, so the log shows which records the new set decided, and the denied refunds name the new policy:

```sh
jq -r 'select(.policy_set_hash) | .policy_set_hash' "$STATE/audit.jsonl" | uniq -c
jq -c 'select(.action == "refund.issue" and .record == "decision") | {seq, decision, reason: .reason.reason, determining_policies}' \
  "$STATE/audit.jsonl" | tail -n 3
"$GUARD" audit verify "$STATE/audit.jsonl"
```

To change the reference policy set itself, edit `examples/guard-support/policies/support.cedar`, run `scripts/validate-guard-policies.sh` (needs `cedar-policy-cli` 4.15.0, see [examples/guard-support/README.md](../../examples/guard-support/README.md#validate-the-policies)), and rerun step 2. Rows whose outcome changes fail the demo until you accept them with `--update-snapshot`.

## 6. Check the deployment: `crom-guard check`

Guard enforces only the path it mediates. If agent Bash can read `credentials.json` or reach the business API, a policy does not help. `crom-guard check` is the static preflight for the supported isolation profile ([deployment.md](deployment.md)). For agent (a), Crom on Guard's uid with sandboxed Bash, it takes Crom's settings file, which must be the file Crom loads (`settings.json` in `$CROM_CONFIG_HOME`), and the agent's workspace:

```sh
CROM_HOME="$WORK/crom-config"
mkdir -m 0700 "$CROM_HOME" "$WORK/workspace"
cp examples/guard-support/agent/crom-settings.json "$CROM_HOME/settings.json"
jq . "$CROM_HOME/settings.json"
CROM_CONFIG_HOME="$CROM_HOME" "$GUARD" check --config "$STATE/guard.toml" \
  --crom-settings "$CROM_HOME/settings.json" --crom-workspace "$WORK/workspace" | tee "$WORK/check-ok.txt"
```

Every line is `ok: <id>: …`, and it exits 0. The settings give Bash `sandbox.mode = "required"` with no network, make Guard the only MCP server, and stop plugins from adding servers.

Give Bash network access and check again. `check` exits 1 and names the failing condition:

```sh
jq '.sandbox.network = true' examples/guard-support/agent/crom-settings.json >"$CROM_HOME/settings.json"
status=0
CROM_CONFIG_HOME="$CROM_HOME" "$GUARD" check --config "$STATE/guard.toml" \
  --crom-settings "$CROM_HOME/settings.json" --crom-workspace "$WORK/workspace" >"$WORK/check-network.txt" || status=$?
grep '^FAIL' "$WORK/check-network.txt"
echo "exit status: $status"
test "$status" -eq 1
```

`check` reads files and metadata only and checks only the conditions listed in [deployment.md](deployment.md). It is a point-in-time check and does not prove the kernel enforces the sandbox; the bypass tests do that against a live Guard (`cargo test -p crom-guard-testkit --test bypass`, [deployment.md](deployment.md#bypass-tests)). For an agent under its own uid, use `--agent-user <name>` instead; `docs/guard/deployment.md` has the full setup with a dedicated `crom-guard` user.

## Clean up

```sh
kill "$GUARD_PID"; wait "$GUARD_PID" || true
rm -rf "$WORK"
```

## Next

- The adversarial scenarios G1–G18 and experiments E1–E5: `cargo test -p crom-guard-testkit --test guard_adversarial` and `--test guard_experiments` ([scenarios.md](scenarios.md), [experiments.md](experiments.md)).
- The measured results, what they show and what they do not: [docs/benchmarks/M18-guard.md](../benchmarks/M18-guard.md).
- A live model instead of the scripted one. This costs money and is never run in CI. It needs the provider's key and a spend cap; the run stops at the cap:

<!-- tutorial: skip -->
```sh
DEEPSEEK_API_KEY=... cargo run --manifest-path examples/guard-support/Cargo.toml -- \
  --agent live --provider deepseek --max-run-cost-usd 0.25 --supervisor deny
```
