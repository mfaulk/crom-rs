use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    let mode = std::env::var("FAKE_MCP_MODE").unwrap_or_default();
    if mode == "invalid-json" {
        println!("not json");
        return;
    }
    if mode == "sleep" {
        std::thread::sleep(std::time::Duration::from_secs(10));
        return;
    }

    eprintln!("fake server stderr token={}", std::env::var("FAKE_SECRET").unwrap_or_default());
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if request.get("id").is_none() {
            continue;
        }
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {
                        "tools": {},
                        "resources": {},
                        "prompts": {}
                    },
                    "serverInfo": {"name": "fake-mcp-server", "version": "0.1.0"},
                    "instructions": "deterministic fake server"
                }
            }),
            "tools/list" => {
                let cursor = request
                    .get("params")
                    .and_then(|p| p.get("cursor"))
                    .and_then(Value::as_str);
                if cursor == Some("page2") {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "search_docs",
                                "description": "Search deterministic docs",
                                "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}
                            }]
                        }
                    })
                } else {
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [
                                {
                                    "name": "list_issues",
                                    "description": "List deterministic issues",
                                    "inputSchema": {"type": "object", "additionalProperties": false}
                                },
                                {
                                    "name": "get_issue",
                                    "description": "Get deterministic issue",
                                    "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
                                }
                            ],
                            "nextCursor": "page2"
                        }
                    })
                }
            }
            "tools/call" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                match name {
                    "list_issues" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "[{\"id\":1,\"title\":\"Parser trims too late\"}]"}],
                            "structuredContent": [{"id": 1, "title": "Parser trims too late"}],
                            "isError": false
                        }
                    }),
                    "get_issue" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "Issue 1: Parser trims too late"}],
                            "structuredContent": {"id": 1, "title": "Parser trims too late"},
                            "isError": false
                        }
                    }),
                    "search_docs" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "resource_link", "uri": "fake://docs/parser", "name": "Parser docs"}],
                            "isError": false
                        }
                    }),
                    "fail" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32000, "message": "fake failure"}
                    }),
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "unknown tool"}],
                            "isError": true
                        }
                    }),
                }
            }
            "resources/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resources": [{
                        "uri": "fake://docs/parser",
                        "name": "Parser docs",
                        "mimeType": "text/plain"
                    }]
                }
            }),
            "resources/read" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "contents": [{
                        "uri": "fake://docs/parser",
                        "text": "Parser docs stay offline.",
                        "mimeType": "text/plain"
                    }]
                }
            }),
            "prompts/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "prompts": [{
                        "name": "summarize_issue",
                        "description": "Summarize one fake issue",
                        "arguments": [{"name": "id", "required": true}]
                    }]
                }
            }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": "method not found"}
            }),
        };
        writeln!(stdout, "{response}").unwrap();
        stdout.flush().unwrap();
    }
}
