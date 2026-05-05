use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

fn main() {
    eprintln!("fake large MCP server ready");
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
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "tool-search-demo-fake", "version": "0.1.0"}
                }
            }),
            "tools/list" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"tools": tool_definitions()}
            }),
            "tools/call" => {
                let name = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": tool_result(name)
                })
            }
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

fn tool_definitions() -> Vec<Value> {
    let mut tools = vec![
        tool(
            "github_list_issues",
            "List GitHub repository issues including parser bug reports",
            &["repository", "state"],
        ),
        tool(
            "github_get_issue",
            "Get one GitHub issue with source file hints and labels",
            &["repository", "issue_number"],
        ),
        tool(
            "github_search_issues",
            "Search GitHub issues by query, bug label, repository, and parser terms",
            &["repository", "query"],
        ),
        tool(
            "github_create_issue",
            "Create a GitHub issue in an external repository",
            &["repository", "title", "body"],
        ),
        tool(
            "github_close_issue",
            "Close a GitHub issue in an external repository",
            &["repository", "issue_number"],
        ),
        tool(
            "github_list_pull_requests",
            "List GitHub pull requests for a repository",
            &["repository", "state"],
        ),
        tool(
            "github_get_pull_request",
            "Get one GitHub pull request and changed files",
            &["repository", "pull_number"],
        ),
        tool(
            "slack_search_messages",
            "Search Slack messages across channels",
            &["workspace", "query"],
        ),
        tool(
            "slack_send_message",
            "Send a Slack message to an external channel",
            &["channel", "text"],
        ),
        tool(
            "sentry_list_errors",
            "List Sentry errors and event groups",
            &["project", "environment"],
        ),
        tool(
            "sentry_get_event",
            "Get a Sentry event with stack trace and tags",
            &["project", "event_id"],
        ),
        tool(
            "docs_search",
            "Search product documentation pages",
            &["query"],
        ),
        tool(
            "docs_fetch_page",
            "Fetch a documentation page by id",
            &["page_id"],
        ),
        tool(
            "grafana_list_dashboards",
            "List Grafana dashboards",
            &["folder"],
        ),
        tool(
            "grafana_get_dashboard",
            "Get a Grafana dashboard by uid",
            &["uid"],
        ),
        tool(
            "splunk_search_logs",
            "Search Splunk logs by query and time range",
            &["query", "earliest", "latest"],
        ),
    ];
    let categories = [
        ("crm", "customer account records"),
        ("billing", "invoice and subscription data"),
        ("ci", "build job and test result metadata"),
        ("deploy", "deployment rollout records"),
        ("feature_flags", "flag configuration metadata"),
        ("pager", "incident and escalation records"),
        ("warehouse", "analytics table metadata"),
        ("calendar", "team calendar metadata"),
    ];
    for i in 0..136 {
        let (prefix, description) = categories[i % categories.len()];
        let verb = match i % 4 {
            0 => "list",
            1 => "get",
            2 => "search",
            _ => "summarize",
        };
        tools.push(tool(
            &format!("{prefix}_{verb}_record_{i:03}"),
            &format!("{verb} deterministic fake {description} for offline tool search demo"),
            &["query", "id", "limit"],
        ));
    }
    tools
}

fn tool(name: &str, description: &str, properties: &[&str]) -> Value {
    let mut props = serde_json::Map::new();
    for property in properties {
        props.insert(
            (*property).to_string(),
            json!({
                "type": if property.contains("number") || *property == "limit" { "integer" } else { "string" },
                "description": format!("{property} argument")
            }),
        );
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": props
        }
    })
}

fn tool_result(name: &str) -> Value {
    match name {
        "github_search_issues" => json!({
            "content": [{"type": "text", "text": "Issue #42: Parser bug drops tokens after whitespace. Likely file: src/parser.rs"}],
            "structuredContent": {"issue": 42, "title": "Parser bug drops tokens after whitespace", "likely_file": "src/parser.rs"},
            "isError": false
        }),
        "github_list_issues" => json!({
            "content": [{"type": "text", "text": "[{\"number\":42,\"title\":\"Parser bug drops tokens after whitespace\"}]"}],
            "structuredContent": [{"number": 42, "title": "Parser bug drops tokens after whitespace"}],
            "isError": false
        }),
        "github_get_issue" => json!({
            "content": [{"type": "text", "text": "Issue #42 points to src/parser.rs and token trimming."}],
            "structuredContent": {"number": 42, "likely_file": "src/parser.rs"},
            "isError": false
        }),
        "docs_search" => json!({
            "content": [{"type": "text", "text": "Parser docs: trim tokens before parse_token."}],
            "isError": false
        }),
        _ => json!({
            "content": [{"type": "text", "text": format!("{name} deterministic fake result")}],
            "isError": false
        }),
    }
}
