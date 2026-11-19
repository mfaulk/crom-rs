#!/usr/bin/env python3
"""Scripted agent: runs the MILESTONE_18 §14.2 matrix through Guard's MCP endpoint.

Each step is checked twice: against the tool result Guard returns, and
against the business API's effect ledger (the independent oracle). The
ledger path is a test-oracle input; a real agent has no such access.

Session handles (the agent's, the replayed one, and a delegated child's) are
never printed or written to the report.
"""

import argparse
import json
import os
import sys
import time
import urllib.error
import urllib.request

from guard_client import GuardClient, JsonRpcError, outcome

if sys.version_info < (3, 10):
    sys.exit("scripted_agent.py needs Python 3.10 or newer")

SESSION_ERROR = -32001
EXPECTED_TOOLS = {"customer.read", "invoice.list", "refund.issue", "customer.export", "guard.delegate"}


def read_secret(path):
    with open(path) as f:
        return f.read().strip()


class Matrix:
    def __init__(self, ledger_path):
        self.ledger_path = ledger_path
        self.rows = []

    def ledger(self):
        with open(self.ledger_path) as f:
            return [json.loads(line) for line in f if line.strip()]

    def record(self, row, step, expected, observed, effects_ok, detail=""):
        ok = expected == observed and effects_ok
        self.rows.append({"row": row, "step": step, "expected": expected, "observed": observed,
                          "ledger_ok": effects_ok, "pass": ok, "detail": detail})
        mark = "PASS" if ok else "FAIL"
        print(f"{mark}  {row:<34} {step:<52} expected={expected:<18} observed={observed}"
              + (f"  ({detail})" if detail and not ok else ""))
        return ok

    def effects_match(self, new, effects, decision_id):
        """The new ledger entries equal `effects` (partial dicts), each tagged with Guard's decision id."""
        if len(new) != len(effects):
            return False, f"ledger +{len(new)}, expected +{len(effects)}"
        for entry, want in zip(new, effects):
            for k, v in want.items():
                if entry.get(k) != v:
                    return False, f"ledger {k}={entry.get(k)!r}, expected {v!r}"
            if entry.get("guard_decision_id") != decision_id:
                return False, "ledger entry does not carry this call's decision id"
            if entry.get("credential_id") != "business-api":
                return False, "ledger entry made with an unexpected credential"
        return True, ""

    def call(self, row, step, client, tool, args, expect, effects=(), meta=None):
        before = len(self.ledger())
        result = client.call_tool(tool, args, meta=meta)
        got = outcome(result)
        new = self.ledger()[before:]
        decision = (result.get("_meta") or {}).get("decision_id")
        ok, detail = self.effects_match(new, list(effects), decision)
        self.record(row, step, expect, got, ok, detail)
        return result

    def session_error(self, row, step, client, expect):
        before = len(self.ledger())
        try:
            client.list_tools()
            got = "listed"
        except JsonRpcError as e:
            got = (e.data or {}).get("code", e.message) if e.code == SESSION_ERROR else f"rpc {e.code}"
        new = self.ledger()[before:]
        self.record(row, step, expect, got, not new, f"ledger +{len(new)}")


def direct_api(matrix, row, step, api_url, method, path, bearer, body=None):
    """Calls the business API without Guard; must be refused with 401 and leave no effect."""
    before = len(matrix.ledger())
    headers = {"Content-Type": "application/json"}
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(api_url + path, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            got = f"http {resp.status}"
    except urllib.error.HTTPError as e:
        got = f"http {e.code}"
    new = matrix.ledger()[before:]
    matrix.record(row, step, "http 401", got, not new, f"ledger +{len(new)}")


def wait_for_approval(inbox, digest, timeout=60.0):
    """Asks the trusted app for approval out of band and waits for its approval id."""
    with open(os.path.join(inbox, "requests", digest), "w") as f:
        f.write(digest)
    path = os.path.join(inbox, "approved", digest)
    deadline = time.monotonic() + timeout
    while not os.path.exists(path):
        if time.monotonic() > deadline:
            raise SystemExit("scripted_agent: no approval arrived")
        time.sleep(0.1)
    return read_secret(path)


def run(args):
    m = Matrix(args.ledger)
    agent = GuardClient(args.url, read_secret(args.handle_file))

    info = agent.initialize()
    name = info.get("serverInfo", {}).get("name")
    m.record("setup", "initialize", "crom-guard", name, True)
    tools = {t["name"] for t in agent.list_tools()}
    m.record("setup", "tools/list shows exactly the granted tools",
             "match", "match" if tools == EXPECTED_TOOLS else ",".join(sorted(tools)), True)

    r = "authorized invoices"
    m.call(r, "invoice.list acme-cus-000", agent, "invoice.list", {"customer_id": "acme-cus-000"},
           "executed", [{"kind": "invoice_list", "tenant": "acme", "customer_id": "acme-cus-000"}])

    r = "another tenant's invoices"
    m.call(r, "invoice.list globex-cus-000", agent, "invoice.list", {"customer_id": "globex-cus-000"},
           "resource_not_found")
    m.call(r, "invoice.list acme-cus-001 (not in grant)", agent, "invoice.list",
           {"customer_id": "acme-cus-001"}, "policy_deny")

    r = "refund within limits"
    m.call(r, "refund.issue acme-inv-000 1000", agent, "refund.issue",
           {"invoice_id": "acme-inv-000", "amount_cents": 1000}, "executed",
           [{"kind": "refund", "tenant": "acme", "invoice_id": "acme-inv-000", "amount_cents": 1000}])

    r = "refund above the limit"
    over = {"invoice_id": "acme-inv-001", "amount_cents": 7500}
    res = m.call(r, "refund.issue acme-inv-001 7500", agent, "refund.issue", over, "approval_required")
    digest = (res.get("structuredContent") or {}).get("action_digest")
    if digest:
        approval_id = wait_for_approval(args.approval_inbox, digest)
        m.call(r, "retry with supervisor approval", agent, "refund.issue", over, "executed",
               [{"kind": "refund", "tenant": "acme", "invoice_id": "acme-inv-001", "amount_cents": 7500}],
               meta={"approval_id": approval_id})
        m.call(r, "replay the used approval", agent, "refund.issue", over, "approval_invalid",
               meta={"approval_id": approval_id})

    r = "export all customer records"
    m.call(r, "customer.export acme-cus-000", agent, "customer.export",
           {"customer_id": "acme-cus-000"}, "policy_deny")

    r = "forge identity"
    m.call(r, "invoice.list with tenant_id=globex", agent, "invoice.list",
           {"customer_id": "acme-cus-000", "tenant_id": "globex"}, "invalid_arguments")
    m.call(r, "refund.issue with user_id=acme-supervisor", agent, "refund.issue",
           {"invoice_id": "acme-inv-000", "amount_cents": 100, "user_id": "acme-supervisor"},
           "invalid_arguments")
    replayed = GuardClient(args.url, read_secret(args.replayed_handle_file))
    replayed.initialize()
    m.session_error(r, "tools/list with another run's handle", replayed, "grant_revoked")
    m.call(r, "invoice.list with another run's handle", replayed, "invoice.list",
           {"customer_id": "acme-cus-000"}, "grant_revoked")
    forged = GuardClient(args.url, "gsh_" + "A" * 43)
    m.call(r, "invoice.list with a fabricated handle", forged, "invoice.list",
           {"customer_id": "acme-cus-000"}, "no_grant")

    r = "delegate broader authority"
    m.call(r, "delegate a higher refund cap", agent, "guard.delegate",
           {"actions": ["refund.issue"],
            "constraints": [{"name": "max_refund_cents", "at_most": 20000}], "ttl_seconds": 60},
           "policy_deny")
    m.call(r, "delegate another customer", agent, "guard.delegate",
           {"actions": ["customer.read"],
            "constraints": [{"name": "customers", "set": [{"type": "Support::Customer", "id": "acme-cus-001"}]}],
            "ttl_seconds": 60},
           "policy_deny")
    res = m.call(r, "delegate read-only to a subagent", agent, "guard.delegate",
                 {"actions": ["customer.read"], "ttl_seconds": 60}, "executed")
    child_handle = (res.get("structuredContent") or {}).get("handle")
    if child_handle:
        child = GuardClient(args.url, child_handle)
        child.initialize()
        m.call(r, "child reads its customer", child, "customer.read", {"customer_id": "acme-cus-000"},
               "executed", [{"kind": "customer_read", "tenant": "acme", "customer_id": "acme-cus-000"}])
        m.call(r, "child lists invoices (not delegated)", child, "invoice.list",
               {"customer_id": "acme-cus-000"}, "action_not_granted")

    r = "bypass via direct API"
    direct_api(m, r, "GET customer without a credential", args.api_url, "GET",
               "/tenants/acme/customers/acme-cus-000", None)
    direct_api(m, r, "GET customer with the session handle", args.api_url, "GET",
               "/tenants/acme/customers/acme-cus-000", read_secret(args.handle_file))
    direct_api(m, r, "POST refund with the session handle", args.api_url, "POST",
               "/tenants/acme/invoices/acme-inv-000/refunds", read_secret(args.handle_file),
               {"amount_cents": 100})

    entries = m.ledger()
    m.record("ledger", "every effect went through Guard", "yes",
             "yes" if all(e.get("guard_decision_id") for e in entries) else "no", True)
    m.record("ledger", "total effects", "4", str(len(entries)), True)

    failed = [row for row in m.rows if not row["pass"]]
    report = {"rows": m.rows, "passed": len(m.rows) - len(failed), "failed": len(failed)}
    if args.report:
        with open(args.report, "w") as f:
            json.dump(report, f, indent=2)
    print(f"\n{report['passed']} passed, {report['failed']} failed")
    return 1 if failed else 0


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--url", required=True, help="Guard agent endpoint, e.g. http://127.0.0.1:8470/mcp")
    p.add_argument("--handle-file", required=True)
    p.add_argument("--replayed-handle-file", required=True, help="handle of an earlier, revoked run")
    p.add_argument("--api-url", required=True, help="business API base URL (for the bypass rows)")
    p.add_argument("--ledger", required=True, help="business API ledger JSONL (test oracle)")
    p.add_argument("--approval-inbox", required=True)
    p.add_argument("--report")
    sys.exit(run(p.parse_args()))


if __name__ == "__main__":
    main()
