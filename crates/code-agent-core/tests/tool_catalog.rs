use std::sync::Arc;

use async_trait::async_trait;
use code_agent_core::{
    LoadedToolState, McpTransportKind, PermissionKind, PermissionMode, Tool, ToolCatalogBuilder,
    ToolContext, ToolError, ToolLoadPolicy, ToolLoadingMode, ToolResult, ToolRule,
    ToolSearchOptions, ToolSearchQuery, ToolSearchStrategy, ToolSourceFilter, ToolVisibility,
};
use serde_json::json;

struct FakeTool {
    name: &'static str,
    description: &'static str,
    kind: PermissionKind,
    schema: serde_json::Value,
}

#[async_trait]
impl Tool for FakeTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        self.description
    }

    fn input_schema(&self) -> serde_json::Value {
        self.schema.clone()
    }

    fn permission_kind(&self) -> PermissionKind {
        self.kind
    }

    async fn execute(
        &self,
        _input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::ok_text("ok"))
    }
}

fn local_tool(name: &'static str, description: &'static str) -> Arc<dyn Tool> {
    Arc::new(FakeTool {
        name,
        description,
        kind: PermissionKind::ReadOnly,
        schema: json!({"type": "object", "properties": {"query": {"type": "string", "description": "search query"}}}),
    })
}

fn mcp_tool(name: &'static str, description: &'static str) -> Arc<dyn Tool> {
    Arc::new(FakeTool {
        name,
        description,
        kind: PermissionKind::External,
        schema: json!({"type": "object", "properties": {"repository": {"type": "string", "description": "GitHub repository"}, "issue": {"type": "integer", "description": "issue number"}}}),
    })
}

#[test]
// Catalog ingestion classifies local tools as eager and MCP tools as deferred by default.
fn catalog_ingests_local_and_mcp_tools_with_default_load_policy() {
    let options = ToolSearchOptions::default();
    let catalog = ToolCatalogBuilder::new(options)
        .ingest_tool(local_tool("Read", "Read a workspace file"))
        .ingest_tool(mcp_tool(
            "mcp__github__list_issues",
            "List GitHub repository issues",
        ))
        .mcp_transport("github", McpTransportKind::Stdio)
        .build();

    assert_eq!(catalog.len(), 2);
    assert_eq!(
        catalog.get("Read").unwrap().load_policy,
        ToolLoadPolicy::Always
    );
    assert_eq!(
        catalog.get("mcp__github__list_issues").unwrap().load_policy,
        ToolLoadPolicy::Deferred
    );
    assert!(catalog.get("Read").unwrap().estimated_tokens > 0);
    assert!(
        catalog
            .get("mcp__github__list_issues")
            .unwrap()
            .estimated_tokens
            > 0
    );
}

#[test]
// Catalog ordering is deterministic by agent tool name.
fn catalog_ordering_is_deterministic() {
    let catalog = ToolCatalogBuilder::new(ToolSearchOptions::default())
        .ingest_tool(mcp_tool("mcp__github__search_issues", "Search issues"))
        .ingest_tool(local_tool("Read", "Read a file"))
        .ingest_tool(mcp_tool("mcp__docs__search", "Search docs"))
        .build();

    assert_eq!(
        catalog.names(),
        vec![
            "Read".to_string(),
            "mcp__docs__search".to_string(),
            "mcp__github__search_issues".to_string()
        ]
    );
}

#[test]
// Disabled patterns hide matching tools from search.
fn disabled_tools_do_not_appear_in_search() {
    let options = ToolSearchOptions {
        disabled: vec!["mcp__github__delete_*".into()],
        ..ToolSearchOptions::default()
    };
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(mcp_tool(
            "mcp__github__search_issues",
            "Search GitHub issues",
        ))
        .ingest_tool(mcp_tool(
            "mcp__github__delete_issue",
            "Delete a GitHub issue",
        ))
        .build();
    let visibility = options.visibility_context(
        PermissionMode::Default,
        vec![ToolRule::try_tool_glob("mcp__github__*").unwrap()],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );

    let result = catalog
        .search(
            ToolSearchQuery {
                query: "github issue".into(),
                limit: 10,
                source_filter: None,
                include_risky: true,
            },
            ToolSearchStrategy::Hybrid,
            &visibility,
        )
        .unwrap();

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].tool_name, "mcp__github__search_issues");
}

#[test]
// Hybrid search ranks issue-related GitHub tools ahead of unrelated tools.
fn hybrid_search_finds_relevant_github_issue_tools() {
    let options = ToolSearchOptions::default();
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(mcp_tool(
            "mcp__github__list_issues",
            "List open GitHub issues for a repository",
        ))
        .ingest_tool(mcp_tool(
            "mcp__github__get_issue",
            "Get one GitHub issue by number",
        ))
        .ingest_tool(mcp_tool(
            "mcp__slack__search_messages",
            "Search Slack channel messages",
        ))
        .build();
    let visibility = options.visibility_context(
        PermissionMode::Default,
        vec![ToolRule::try_tool_glob("mcp__*").unwrap()],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );

    let result = catalog
        .search(
            ToolSearchQuery {
                query: "github issue bug".into(),
                limit: 2,
                source_filter: Some(ToolSourceFilter::McpServer("github".into())),
                include_risky: true,
            },
            ToolSearchStrategy::Hybrid,
            &visibility,
        )
        .unwrap();

    assert_eq!(
        result
            .matches
            .iter()
            .map(|m| m.tool_name.as_str())
            .collect::<Vec<_>>(),
        vec!["mcp__github__get_issue", "mcp__github__list_issues"]
    );
    assert!(result.matches.iter().all(|m| m.score > 0.0));
}

#[test]
// Regex search returns a structured error for invalid regex patterns.
fn regex_search_invalid_pattern_errors() {
    let catalog = ToolCatalogBuilder::new(ToolSearchOptions::default())
        .ingest_tool(local_tool("Read", "Read a file"))
        .build();
    let visibility = ToolSearchOptions::default().visibility_context(
        PermissionMode::Default,
        vec![],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );
    let err = catalog
        .search(
            ToolSearchQuery {
                query: "[".into(),
                limit: 5,
                source_filter: None,
                include_risky: true,
            },
            ToolSearchStrategy::Regex,
            &visibility,
        )
        .unwrap_err();

    assert!(err.to_string().contains("invalid regex"));
}

#[test]
// Search mode loads always-loaded and discovered tools, not every deferred tool.
fn search_mode_loaded_names_include_discovered_subset() {
    let options = ToolSearchOptions {
        mode: ToolLoadingMode::Search,
        always_loaded: vec!["Read".into(), "SearchTools".into()],
        max_loaded_tools: 5,
        ..ToolSearchOptions::default()
    };
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(local_tool("Read", "Read a file"))
        .ingest_tool(local_tool("SearchTools", "Search tools"))
        .ingest_tool(mcp_tool("mcp__github__list_issues", "List GitHub issues"))
        .ingest_tool(mcp_tool("mcp__docs__search", "Search docs"))
        .build();
    let mut state = LoadedToolState::default();
    state
        .discovered_this_run
        .insert("mcp__github__list_issues".into());

    let loaded = catalog.loaded_tool_names(&state, &options);

    assert!(loaded.contains("Read"));
    assert!(loaded.contains("SearchTools"));
    assert!(loaded.contains("mcp__github__list_issues"));
    assert!(!loaded.contains("mcp__docs__search"));
}

#[test]
// DontAsk visibility returns only tools callable without interactive approval.
fn dont_ask_visibility_hides_tools_requiring_approval() {
    let options = ToolSearchOptions::default();
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(mcp_tool("mcp__github__list_issues", "List GitHub issues"))
        .ingest_tool(local_tool("Read", "Read a file"))
        .build();
    let visibility = options.visibility_context(
        PermissionMode::DontAsk,
        vec![ToolRule::ExactTool("Read".into())],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );

    let result = catalog
        .search(
            ToolSearchQuery {
                query: "github read issue".into(),
                limit: 10,
                source_filter: None,
                include_risky: true,
            },
            ToolSearchStrategy::Hybrid,
            &visibility,
        )
        .unwrap();

    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].tool_name, "Read");
    assert_eq!(result.matches[0].visibility, ToolVisibility::Callable);
}

#[test]
// Auto mode switches to search mode when the catalog exceeds the configured count.
fn auto_mode_switches_to_search_for_large_catalogs() {
    let options = ToolSearchOptions {
        mode: ToolLoadingMode::Auto {
            threshold_percent_of_context: 10,
            min_tool_count: 2,
        },
        ..ToolSearchOptions::default()
    };
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(local_tool("Read", "Read a file"))
        .ingest_tool(mcp_tool("mcp__github__list_issues", "List GitHub issues"))
        .ingest_tool(mcp_tool("mcp__docs__search", "Search docs"))
        .build();

    let decision = catalog.resolve_loading_mode(&options);

    assert_eq!(decision.resolved_mode.to_string(), "search");
    assert!(decision.reason.contains("tool count"));
}

#[test]
// Secret-like metadata and schema values are redacted in descriptors.
fn secret_like_values_are_redacted() {
    let tool = Arc::new(FakeTool {
        name: "mcp__docs__search",
        description: "Search docs",
        kind: PermissionKind::External,
        schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "authorization": {"type": "string", "default": "Bearer abc123"}
            }
        }),
    });
    let catalog = ToolCatalogBuilder::new(ToolSearchOptions::default())
        .ingest_tool(tool)
        .build();
    let schema = &catalog.get("mcp__docs__search").unwrap().input_schema;

    assert_eq!(
        schema
            .pointer("/properties/authorization/default")
            .and_then(|v| v.as_str()),
        Some("[REDACTED]")
    );
}

#[test]
// DontAsk visibility surfaces tools that are only in the simple-name allowlist
// (`AgentOptions::allowed_tools`) — not just those listed by `ExactTool` rules.
// Regression test for the case where the search path hid runtime-allowed tools.
fn dont_ask_visibility_honors_simple_name_allowlist() {
    let options = ToolSearchOptions::default();
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(mcp_tool("mcp__github__list_issues", "List GitHub issues"))
        .ingest_tool(local_tool("Read", "Read a file"))
        .build();
    let visibility = options.visibility_context(
        PermissionMode::DontAsk,
        vec![],
        vec![],
        vec!["mcp__github__list_issues".into()],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );

    let result = catalog
        .search(
            ToolSearchQuery {
                query: "github".into(),
                limit: 10,
                source_filter: None,
                include_risky: true,
            },
            ToolSearchStrategy::Hybrid,
            &visibility,
        )
        .unwrap();

    let names: Vec<&str> = result
        .matches
        .iter()
        .map(|m| m.tool_name.as_str())
        .collect();
    assert!(
        names.contains(&"mcp__github__list_issues"),
        "simple-name allowlist should surface {:?} in DontAsk search",
        names
    );
    let m = result
        .matches
        .iter()
        .find(|m| m.tool_name == "mcp__github__list_issues")
        .unwrap();
    assert_eq!(m.visibility, ToolVisibility::Callable);
}

#[test]
// Visibility honors the runtime MCP external-effect policy — External tools
// reach `RequiresApproval` under `AllowWithApproval`, not the hardcoded
// `Denied` that resulted from `RequireExplicitAllowRule` being baked in.
fn visibility_honors_runtime_mcp_external_effect_policy() {
    let options = ToolSearchOptions::default();
    let catalog = ToolCatalogBuilder::new(options.clone())
        .ingest_tool(mcp_tool("mcp__github__list_issues", "List GitHub issues"))
        .build();
    let visibility = options.visibility_context(
        PermissionMode::Default,
        vec![],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::AllowWithApproval,
    );

    let result = catalog
        .search(
            ToolSearchQuery {
                query: "github".into(),
                limit: 10,
                source_filter: None,
                include_risky: true,
            },
            ToolSearchStrategy::Hybrid,
            &visibility,
        )
        .unwrap();

    let m = result
        .matches
        .iter()
        .find(|m| m.tool_name == "mcp__github__list_issues")
        .expect("AllowWithApproval should surface External MCP tools");
    assert_eq!(m.visibility, ToolVisibility::RequiresApproval);
}

#[test]
// Repeated BM25 searches against the same catalog return identical scores —
// the cached BM25 corpus stats (avg doc length, per-document term counts)
// must produce the same ranking as the original on-demand computation.
fn bm25_search_results_are_stable_across_repeated_queries() {
    let options = ToolSearchOptions::default();
    let mut builder = ToolCatalogBuilder::new(options.clone());
    // A modest corpus so we exercise both common and rare query terms.
    let names = [
        ("mcp__github__list_issues", "List GitHub repository issues"),
        ("mcp__github__get_issue", "Get one GitHub issue by number"),
        (
            "mcp__github__search_issues",
            "Search GitHub issues by query",
        ),
        ("mcp__github__list_pulls", "List GitHub pull requests"),
        (
            "mcp__slack__send_message",
            "Post a Slack message to a channel",
        ),
        ("mcp__slack__search", "Search Slack messages"),
        ("mcp__docs__read", "Read documentation pages"),
        ("Read", "Read a workspace file"),
        ("Grep", "Search workspace text with ripgrep"),
        ("Glob", "Find workspace files matching a glob pattern"),
    ];
    for (n, d) in names {
        let tool: Arc<dyn Tool> = if n.starts_with("mcp__") {
            mcp_tool(n, d)
        } else {
            local_tool(n, d)
        };
        builder = builder.ingest_tool(tool);
    }
    let catalog = builder.build();
    let visibility = options.visibility_context(
        PermissionMode::Default,
        vec![ToolRule::try_tool_glob("mcp__*").unwrap()],
        vec![],
        vec![],
        code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
    );

    let queries = ["github issue", "slack message search", "read file"];
    for q in queries {
        let first = catalog
            .search(
                ToolSearchQuery {
                    query: q.into(),
                    limit: 5,
                    source_filter: None,
                    include_risky: true,
                },
                ToolSearchStrategy::Bm25,
                &visibility,
            )
            .unwrap();
        // Repeat the same search several times — the cache should be
        // stable across calls.
        for _ in 0..5 {
            let again = catalog
                .search(
                    ToolSearchQuery {
                        query: q.into(),
                        limit: 5,
                        source_filter: None,
                        include_risky: true,
                    },
                    ToolSearchStrategy::Bm25,
                    &visibility,
                )
                .unwrap();
            assert_eq!(first.matches.len(), again.matches.len());
            for (a, b) in first.matches.iter().zip(again.matches.iter()) {
                assert_eq!(a.tool_name, b.tool_name);
                assert!(
                    (a.score - b.score).abs() < 1e-6,
                    "BM25 score drifted across repeated queries: {} vs {}",
                    a.score,
                    b.score
                );
            }
        }
    }
}
