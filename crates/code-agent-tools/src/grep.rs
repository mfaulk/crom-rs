use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::Regex;
use serde::Deserialize;
use serde_json::json;

use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::path_sandbox::canonicalize_root;

#[derive(Debug, Deserialize)]
struct Input {
    pattern: String,
    #[serde(default)]
    include: Option<String>,
}

pub struct GrepTool {
    limits: Arc<ToolLimits>,
}

impl GrepTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "Grep"
    }

    fn description(&self) -> &'static str {
        "Search for a regex pattern across workspace files, optionally filtered by an include glob. Honors .gitignore. Returns up to a configured number of matching lines."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "include": {"type": "string"}
            },
            "required": ["pattern"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let p = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
        format!("Grep {p}")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Grep input: {e}")))?;
        let root = canonicalize_root(&ctx.cwd)?;
        let regex = Regex::new(&parsed.pattern)
            .map_err(|e| ToolError::InvalidInput(format!("invalid regex: {e}")))?;
        let include: Option<GlobSet> = match parsed.include {
            Some(p) => {
                let glob = Glob::new(&p)
                    .map_err(|e| ToolError::InvalidInput(format!("invalid include glob: {e}")))?;
                let mut b = GlobSetBuilder::new();
                b.add(glob);
                Some(
                    b.build()
                        .map_err(|e| ToolError::InvalidInput(format!("include glob build: {e}")))?,
                )
            }
            None => None,
        };
        let limit = self.limits.max_grep_results;
        let max_read = self.limits.max_read_bytes;

        let root_clone = root.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<_, ToolError> {
            let mut hits: Vec<String> = Vec::new();
            let mut truncated = false;
            'outer: for entry in WalkBuilder::new(&root_clone).hidden(false).build() {
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
                if let Some(set) = &include {
                    if !set.is_match(rel) {
                        continue;
                    }
                }
                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if (meta.len() as usize) > max_read {
                    continue;
                }
                let bytes = match std::fs::read(entry.path()) {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                let text = match std::str::from_utf8(&bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                for (idx, line) in text.lines().enumerate() {
                    if regex.is_match(line) {
                        if hits.len() >= limit {
                            truncated = true;
                            break 'outer;
                        }
                        hits.push(format!("{}:{}: {}", rel.display(), idx + 1, line));
                    }
                }
            }
            Ok((hits, truncated))
        })
        .await
        .map_err(|e| ToolError::ExecutionFailed(format!("grep join: {e}")))??;

        let (hits, truncated) = result;
        let mut text = if hits.is_empty() {
            "(no matches)".to_string()
        } else {
            hits.join("\n")
        };
        if truncated {
            text.push_str(&format!("\n... (truncated at max_grep_results={})", limit));
        }
        let metadata = json!({
            "count": hits.len(),
            "truncated": truncated,
        });
        Ok(ToolResult {
            content: vec![code_agent_core::ToolContent::Text(text)],
            is_error: false,
            metadata,
        })
    }
}
