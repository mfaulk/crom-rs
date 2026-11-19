"""Unit tests for guard_client: JSON-RPC framing and the HTTP exchange."""

import ast
import http.server
import json
import os
import sys
import threading
import unittest

import guard_client
from guard_client import GuardClient, JsonRpcError, decode_response, encode_request


class FramingTest(unittest.TestCase):
    def test_request_has_jsonrpc_id_method_params(self):
        """A request carries jsonrpc 2.0, its id, method and params, and nothing else."""
        msg = json.loads(encode_request("tools/call", {"name": "x"}, 7))
        self.assertEqual(msg, {"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": "x"}})

    def test_notification_has_no_id(self):
        """A notification (id None) omits the id member and empty params."""
        msg = json.loads(encode_request("notifications/initialized", None, None))
        self.assertEqual(msg, {"jsonrpc": "2.0", "method": "notifications/initialized"})

    def test_decode_json_result(self):
        """A JSON response with a matching id yields its result."""
        body = b'{"jsonrpc":"2.0","id":3,"result":{"tools":[]}}'
        self.assertEqual(decode_response(body, "application/json", 3), {"tools": []})

    def test_decode_json_error(self):
        """A JSON-RPC error response raises JsonRpcError with code, message and data."""
        body = b'{"jsonrpc":"2.0","id":3,"error":{"code":-32001,"message":"no_grant","data":{"code":"no_grant"}}}'
        with self.assertRaises(JsonRpcError) as cm:
            decode_response(body, "application/json; charset=utf-8", 3)
        self.assertEqual(cm.exception.code, -32001)
        self.assertEqual(cm.exception.message, "no_grant")
        self.assertEqual(cm.exception.data, {"code": "no_grant"})

    def test_decode_rejects_mismatched_id(self):
        """A response for another request id is a protocol error."""
        with self.assertRaises(guard_client.ProtocolError):
            decode_response(b'{"jsonrpc":"2.0","id":4,"result":{}}', "application/json", 3)

    def test_decode_rejects_wrong_version_and_ambiguous_body(self):
        """A response without jsonrpc 2.0, or with both or neither of result and error, is a protocol error."""
        for body in (
            b'{"jsonrpc":"1.0","id":3,"result":{}}',
            b'{"jsonrpc":"2.0","id":3}',
            b'{"jsonrpc":"2.0","id":3,"result":{},"error":{"code":1,"message":"x"}}',
            b"not json",
            b"[]",
        ):
            with self.subTest(body=body):
                with self.assertRaises(guard_client.ProtocolError):
                    decode_response(body, "application/json", 3)

    def test_decode_sse_picks_matching_message(self):
        """An event-stream response yields the data event whose id matches, skipping others."""
        body = (
            b"event: message\n"
            b'data: {"jsonrpc":"2.0","method":"notifications/progress","params":{}}\n\n'
            b"event: message\n"
            b'data: {"jsonrpc":"2.0",\n'
            b'data: "id":5,"result":{"ok":true}}\n\n'
        )
        self.assertEqual(decode_response(body, "text/event-stream", 5), {"ok": True})

    def test_decode_rejects_other_content_types(self):
        """A response that is neither JSON nor an event stream is a protocol error."""
        with self.assertRaises(guard_client.ProtocolError):
            decode_response(b"<html>", "text/html", 1)


class _Stub(http.server.BaseHTTPRequestHandler):
    """Records each request and answers like a stateless Streamable HTTP server."""

    seen = []

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        _Stub.seen.append((self.headers, body))
        if "id" not in body:
            self.send_response(202)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        result = {"initialize": {"protocolVersion": "2025-06-18", "serverInfo": {"name": "stub"}, "capabilities": {}},
                  "tools/list": {"tools": [{"name": "customer.read"}]},
                  "tools/call": {"isError": False, "structuredContent": {"ok": 1}, "_meta": {"decision_id": "dec-1"}}}
        out = json.dumps({"jsonrpc": "2.0", "id": body["id"], "result": result[body["method"]]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *args):
        pass


class ClientTest(unittest.TestCase):
    def setUp(self):
        _Stub.seen = []
        self.server = http.server.HTTPServer(("127.0.0.1", 0), _Stub)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/mcp"

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()

    def test_session_sends_bearer_accept_and_protocol_version(self):
        """initialize, notifications/initialized, tools/list and tools/call go out with the handle as bearer and Streamable HTTP headers."""
        c = GuardClient(self.url, "gsh_secret_value")
        info = c.initialize()
        self.assertEqual(info["serverInfo"]["name"], "stub")
        self.assertEqual([t["name"] for t in c.list_tools()], ["customer.read"])
        r = c.call_tool("customer.read", {"customer_id": "c"}, meta={"approval_id": "ap_1"})
        self.assertEqual(r["_meta"]["decision_id"], "dec-1")
        methods = [b.get("method") for _, b in _Stub.seen]
        self.assertEqual(methods, ["initialize", "notifications/initialized", "tools/list", "tools/call"])
        for headers, _ in _Stub.seen:
            self.assertEqual(headers["Authorization"], "Bearer gsh_secret_value")
            self.assertIn("application/json", headers["Accept"])
            self.assertIn("text/event-stream", headers["Accept"])
        self.assertIsNone(_Stub.seen[0][0].get("MCP-Protocol-Version"))
        self.assertEqual(_Stub.seen[2][0]["MCP-Protocol-Version"], "2025-06-18")
        call = _Stub.seen[3][1]["params"]
        self.assertEqual(call, {"name": "customer.read", "arguments": {"customer_id": "c"}, "_meta": {"approval_id": "ap_1"}})

    def test_repr_hides_handle(self):
        """The client's repr never contains the session handle."""
        self.assertNotIn("gsh_secret_value", repr(GuardClient(self.url, "gsh_secret_value")))


class StdlibOnlyTest(unittest.TestCase):
    def test_client_scripts_import_only_the_standard_library(self):
        """Every client script imports only standard-library modules or sibling client modules."""
        here = os.path.dirname(os.path.abspath(__file__))
        local = {f[:-3] for f in os.listdir(here) if f.endswith(".py")}
        for name in sorted(local):
            with open(os.path.join(here, name + ".py")) as f:
                tree = ast.parse(f.read())
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    mods = [a.name for a in node.names]
                elif isinstance(node, ast.ImportFrom):
                    mods = [node.module or ""]
                else:
                    continue
                for mod in mods:
                    top = mod.split(".")[0]
                    with self.subTest(file=name, module=mod):
                        self.assertTrue(top in sys.stdlib_module_names or top in local, mod)


if __name__ == "__main__":
    unittest.main()
