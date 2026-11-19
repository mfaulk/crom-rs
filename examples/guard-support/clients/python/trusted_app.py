#!/usr/bin/env python3
"""The trusted application's side of the reference deployment: Guard's admin API.

Runs with the admin token, which the agent never sees. Standard library only.

  trusted_app.py --admin unix:PATH --token-file F health
  trusted_app.py ... issue --spec grant.json [--run-id R] --handle-out H
  trusted_app.py ... revoke GRANT_ID
  trusted_app.py ... approve-pending --inbox DIR --count N --expect-action A \\
      --approver-id ID --approver-role ROLE --approver-limit CENTS

`issue` writes the session handle to a new 0600 file and prints only the
grant id. `approve-pending` plays the human approver: it waits for the agent
to drop `<inbox>/requests/<digest>`, fetches what Guard evaluated, checks the
action, approves, and writes the approval id to `<inbox>/approved/<digest>`.
"""

import argparse
import http.client
import json
import os
import socket
import sys
import time

if sys.version_info < (3, 10):
    sys.exit("trusted_app.py needs Python 3.10 or newer")


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, path, timeout=30.0):
        super().__init__("localhost", timeout=timeout)
        self._path = path

    def connect(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        sock.connect(self._path)
        self.sock = sock


class Admin:
    def __init__(self, address, token):
        self._address = address
        self._token = token

    def __repr__(self):
        return f"Admin(address={self._address!r}, token=<redacted>)"

    def _conn(self):
        if self._address.startswith("unix:"):
            return UnixHTTPConnection(self._address[5:])
        host, _, port = self._address.rpartition(":")
        return http.client.HTTPConnection(host, int(port), timeout=30.0)

    def call(self, method, path, body=None):
        conn = self._conn()
        try:
            headers = {"Authorization": f"Bearer {self._token}"}
            data = None
            if body is not None:
                data = json.dumps(body).encode()
                headers["Content-Type"] = "application/json"
            conn.request(method, path, body=data, headers=headers)
            resp = conn.getresponse()
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
        finally:
            conn.close()

    def expect(self, status, method, path, body=None):
        got, payload = self.call(method, path, body)
        if got != status:
            error = payload.get("error") if isinstance(payload, dict) else None
            raise SystemExit(f"trusted_app: {method} {path}: HTTP {got} ({error})")
        return payload


def write_secret(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as f:
        f.write(value)


def cmd_issue(admin, args):
    with open(args.spec) as f:
        spec = json.load(f)
    if args.run_id:
        spec["run_id"] = args.run_id
    issued = admin.expect(201, "POST", "/admin/grants", spec)
    write_secret(args.handle_out, issued["handle"])
    print(json.dumps({"grant_id": issued["grant"]["grant_id"]}))


def cmd_revoke(admin, args):
    print(json.dumps(admin.expect(200, "POST", f"/admin/grants/{args.grant_id}/revoke")))


def cmd_health(admin, _args):
    print(json.dumps(admin.expect(200, "GET", "/admin/health")))


def cmd_approve_pending(admin, args):
    requests = os.path.join(args.inbox, "requests")
    approved = os.path.join(args.inbox, "approved")
    done = set()
    deadline = time.monotonic() + args.timeout
    while len(done) < args.count:
        if time.monotonic() > deadline:
            raise SystemExit(f"trusted_app: approved {len(done)} of {args.count} before the timeout")
        for digest in sorted(set(os.listdir(requests)) - done):
            pending = admin.expect(200, "GET", f"/admin/approvals/{digest}")
            # What a human reviewer would be shown; approve only the expected action.
            if pending["action"] != args.expect_action:
                raise SystemExit(f"trusted_app: refusing to approve action {pending['action']!r}")
            approval = admin.expect(201, "POST", "/admin/approvals", {
                "action_digest": digest,
                "approver": {
                    "id": args.approver_id,
                    "role": args.approver_role,
                    "attrs": {"refund_limit_cents": {"long": args.approver_limit}},
                },
                "approver_tenant": pending["tenant"],
                "ttl_seconds": 300,
            })
            tmp = os.path.join(approved, f".{digest}.tmp")
            write_secret(tmp, approval["approval_id"])
            os.rename(tmp, os.path.join(approved, digest))
            done.add(digest)
            print(json.dumps({"approved": digest, "action": pending["action"], "args": pending["args"]}))
        time.sleep(0.1)


def main():
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--admin", required=True, help="unix:PATH or 127.0.0.1:PORT")
    p.add_argument("--token-file", required=True)
    sub = p.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("issue")
    s.add_argument("--spec", required=True)
    s.add_argument("--run-id")
    s.add_argument("--handle-out", required=True)
    s = sub.add_parser("revoke")
    s.add_argument("grant_id")
    sub.add_parser("health")
    s = sub.add_parser("approve-pending")
    s.add_argument("--inbox", required=True)
    s.add_argument("--count", type=int, default=1)
    s.add_argument("--timeout", type=float, default=60.0)
    s.add_argument("--expect-action", required=True)
    s.add_argument("--approver-id", required=True)
    s.add_argument("--approver-role", required=True)
    s.add_argument("--approver-limit", type=int, required=True)
    args = p.parse_args()
    with open(args.token_file) as f:
        admin = Admin(args.admin, f.read().strip())
    {"issue": cmd_issue, "revoke": cmd_revoke, "health": cmd_health,
     "approve-pending": cmd_approve_pending}[args.cmd](admin, args)


if __name__ == "__main__":
    main()
