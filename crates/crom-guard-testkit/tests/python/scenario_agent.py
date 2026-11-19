"""Scripted agent for the #361 Python client lane. Standard library only.

Reads one JSON request per line on stdin ({"url", "handle", "tool",
"arguments", "meta"}), makes the call with the reference GuardClient, and
writes the JSON-RPC result or error as one line on stdout. argv[1] is the
directory holding guard_client.py.
"""

import json
import sys

sys.path.insert(0, sys.argv[1])

from guard_client import GuardClient, JsonRpcError, ProtocolError  # noqa: E402

CLIENT_REQUEST_ID = "scenario-agent.py"


def handle(req):
    client = GuardClient(req["url"], req["handle"])
    try:
        client.initialize()
    except (JsonRpcError, ProtocolError):
        pass  # A session error is reported by the call itself.
    # Untrusted correlation; Guard records it, so the test can tell these
    # calls came from this process.
    meta = dict(req.get("meta") or {}, client_request_id=CLIENT_REQUEST_ID)
    try:
        return {"result": client.call_tool(req["tool"], req["arguments"], meta)}
    except JsonRpcError as e:
        return {"error": {"code": e.code, "message": e.message, "data": e.data}}
    except ProtocolError as e:
        return {"error": {"code": 0, "message": str(e), "data": {"code": "protocol_error"}}}


for line in sys.stdin:
    print(json.dumps(handle(json.loads(line))), flush=True)
