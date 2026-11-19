"""Minimal MCP Streamable HTTP client for Crom Guard. Standard library only.

The session handle is a bearer secret: it is sent only in the Authorization
header and never appears in repr(), exceptions, or log output.
"""

import json
import urllib.error
import urllib.request

PROTOCOL_VERSION = "2025-06-18"
CLIENT_INFO = {"name": "guard-python-client", "version": "0.1.0"}


class ProtocolError(Exception):
    """The server's reply is not a valid JSON-RPC response to our request."""


class JsonRpcError(Exception):
    """A JSON-RPC error response (for Guard: -32001 session error, -32002 rate limited)."""

    def __init__(self, code, message, data=None):
        super().__init__(f"JSON-RPC error {code}: {message}")
        self.code = code
        self.message = message
        self.data = data


def encode_request(method, params, request_id):
    """One JSON-RPC 2.0 message; `request_id` None makes it a notification."""
    msg = {"jsonrpc": "2.0"}
    if request_id is not None:
        msg["id"] = request_id
    msg["method"] = method
    if params is not None:
        msg["params"] = params
    return json.dumps(msg, separators=(",", ":")).encode()


def _sse_messages(text):
    data = []
    for line in text.splitlines() + [""]:
        if line == "":
            if data:
                yield "\n".join(data)
                data = []
        elif line.startswith("data:"):
            data.append(line[5:].removeprefix(" "))


def _check(msg, request_id):
    if not isinstance(msg, dict) or msg.get("jsonrpc") != "2.0":
        raise ProtocolError("not a JSON-RPC 2.0 message")
    if msg.get("id") != request_id:
        raise ProtocolError(f"response id {msg.get('id')!r} does not match request id {request_id!r}")
    if ("result" in msg) == ("error" in msg):
        raise ProtocolError("response must carry exactly one of result and error")
    if "error" in msg:
        err = msg["error"]
        if not isinstance(err, dict) or not isinstance(err.get("code"), int):
            raise ProtocolError("malformed error object")
        raise JsonRpcError(err["code"], err.get("message", ""), err.get("data"))
    return msg["result"]


def decode_response(body, content_type, request_id):
    """The result of the response to `request_id`, from a JSON or SSE body."""
    media = (content_type or "").split(";")[0].strip().lower()
    try:
        text = body.decode("utf-8")
    except UnicodeDecodeError as e:
        raise ProtocolError("response is not UTF-8") from e
    if media == "application/json":
        try:
            return _check(json.loads(text), request_id)
        except json.JSONDecodeError as e:
            raise ProtocolError("response is not JSON") from e
    if media == "text/event-stream":
        for data in _sse_messages(text):
            try:
                msg = json.loads(data)
            except json.JSONDecodeError as e:
                raise ProtocolError("event data is not JSON") from e
            if isinstance(msg, dict) and "id" in msg and ("result" in msg or "error" in msg):
                if msg.get("id") == request_id:
                    return _check(msg, request_id)
        raise ProtocolError("event stream carried no response to the request")
    raise ProtocolError(f"unexpected content type {media!r}")


class GuardClient:
    """One agent session against Guard's `POST /mcp`, authenticated by a session handle."""

    def __init__(self, url, handle, timeout=30.0):
        self._url = url
        self._handle = handle
        self._timeout = timeout
        self._next_id = 0
        self._protocol_version = None

    def __repr__(self):
        return f"GuardClient(url={self._url!r}, handle=<redacted>)"

    def _post(self, payload):
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
            "Authorization": f"Bearer {self._handle}",
        }
        if self._protocol_version:
            headers["MCP-Protocol-Version"] = self._protocol_version
        req = urllib.request.Request(self._url, data=payload, headers=headers, method="POST")
        try:
            with urllib.request.urlopen(req, timeout=self._timeout) as resp:
                return resp.status, resp.headers.get("Content-Type", ""), resp.read()
        except urllib.error.HTTPError as e:
            # The bearer is in the request, never in the error; keep only the status.
            raise ProtocolError(f"HTTP {e.code} from the agent endpoint") from None

    def request(self, method, params=None):
        self._next_id += 1
        rid = self._next_id
        _, ctype, body = self._post(encode_request(method, params, rid))
        return decode_response(body, ctype, rid)

    def notify(self, method, params=None):
        status, _, _ = self._post(encode_request(method, params, None))
        if status not in (200, 202, 204):
            raise ProtocolError(f"notification answered with HTTP {status}")

    def initialize(self):
        result = self.request(
            "initialize",
            {"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": CLIENT_INFO},
        )
        self._protocol_version = result.get("protocolVersion", PROTOCOL_VERSION)
        self.notify("notifications/initialized")
        return result

    def list_tools(self):
        tools, cursor = [], None
        while True:
            result = self.request("tools/list", {"cursor": cursor} if cursor else {})
            tools.extend(result.get("tools", []))
            cursor = result.get("nextCursor")
            if not cursor:
                return tools

    def call_tool(self, name, arguments, meta=None):
        """The raw CallToolResult; Guard's outcome is in structuredContent and isError."""
        params = {"name": name, "arguments": arguments}
        if meta:
            params["_meta"] = meta
        return self.request("tools/call", params)


def outcome(result):
    """`executed`, a deny or failure code, or `approval_required`, from a CallToolResult."""
    if not result.get("isError"):
        return "executed"
    sc = result.get("structuredContent") or {}
    if sc.get("status") == "approval_required":
        return "approval_required"
    return sc.get("code", "unknown")
