use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    eprintln!("fake MCP server ready");
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
                    "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                    "serverInfo": {"name": "mcp-demo-fake", "version": "0.1.0"}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "list_issues",
                            "description": "List deterministic fake issues",
                            "inputSchema": {"type": "object", "additionalProperties": false}
                        },
                        {
                            "name": "get_issue",
                            "description": "Get one deterministic fake issue",
                            "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}
                        },
                        {
                            "name": "search_docs",
                            "description": "Search deterministic fake docs",
                            "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}}
                        }
                    ]
                }
            }),
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
                            "content": [{"type": "text", "text": "[{\"id\":1,\"title\":\"Parser trims too late\"},{\"id\":2,\"title\":\"Docs search misses parser guide\"}]"}],
                            "structuredContent": [
                                {"id": 1, "title": "Parser trims too late"},
                                {"id": 2, "title": "Docs search misses parser guide"}
                            ],
                            "isError": false
                        }
                    }),
                    "get_issue" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "Issue 1: Parser trims too late. Likely file: src/lib.rs"}],
                            "structuredContent": {"id": 1, "title": "Parser trims too late", "likely_file": "src/lib.rs"},
                            "isError": false
                        }
                    }),
                    "search_docs" => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "Parser docs: trim tokens before splitting."}],
                            "isError": false
                        }
                    }),
                    _ => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{"type": "text", "text": "unknown fake tool"}],
                            "isError": true
                        }
                    }),
                }
            }
            "resources/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resources": [{"uri": "fake://docs/parser", "name": "Parser guide", "mimeType": "text/plain"}]
                }
            }),
            "resources/read" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "contents": [{"uri": "fake://docs/parser", "text": "Trim tokens before splitting.", "mimeType": "text/plain"}]
                }
            }),
            "prompts/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "prompts": [{"name": "summarize_issue", "description": "Summarize a fake issue"}]
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
