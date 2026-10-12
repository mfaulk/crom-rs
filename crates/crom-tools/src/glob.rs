use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;

use crom_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::path_sandbox::canonicalize_root;

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
}

pub struct GlobTool {
    limits: Arc<ToolLimits>,
}

impl GlobTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files in the workspace matching a glob pattern (e.g. \"**/*.rs\"). Honors .gitignore. Returns up to a configured number of paths."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"}
            },
            "required": ["pattern"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
        format!("Glob {p}")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Glob input: {e}")))?;
        let root = canonicalize_root(&ctx.cwd)?;
        let limit = self.limits.max_glob_results;

        let glob = Glob::new(&parsed.pattern)
            .map_err(|e| ToolError::InvalidInput(format!("invalid glob: {e}")))?;
        let mut set = GlobSetBuilder::new();
        set.add(glob);
        let set = set
            .build()
            .map_err(|e| ToolError::InvalidInput(format!("glob build: {e}")))?;

        let root_clone = root.clone();
        let results = tokio::task::spawn_blocking(move || {
            let mut results: Vec<String> = Vec::new();
            let mut truncated = false;
            for entry in WalkBuilder::new(&root_clone).hidden(false).build() {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let rel = match entry.path().strip_prefix(&root_clone) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if set.is_match(rel) {
                    if results.len() >= limit {
                        truncated = true;
                        break;
                    }
                    results.push(rel.to_string_lossy().into_owned());
                }
            }
            results.sort();
            (results, truncated)
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("glob join: {e}")))?;

        let (results, truncated) = results;
        let mut text = if results.is_empty() {
            "(no matches)".to_string()
        } else {
            results.join("\n")
        };
        if truncated {
            text.push_str(&format!("\n... (truncated at max_glob_results={})", limit));
        }
        let metadata = json!({
            "count": results.len(),
            "truncated": truncated,
        });
        Ok(ToolResult {
            content: vec![crom_core::ToolContent::Text(text)],
            is_error: false,
            metadata,
        })
    }
}
