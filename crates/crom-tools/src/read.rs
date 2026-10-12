use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use crom_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::path_sandbox::resolve_within;

#[derive(Debug, Deserialize)]
struct Input {
    file_path: String,
}

pub struct ReadTool {
    limits: Arc<ToolLimits>,
}

impl ReadTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file from the workspace. Returns the file's contents. Paths are resolved relative to the workspace root and must stay inside it."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Path to the file relative to the workspace root."
                }
            },
            "required": ["file_path"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let p = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        format!("Read {p}")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Read input: {e}")))?;
        let path = resolve_within(&ctx.cwd, &parsed.file_path, true)?;
        let meta = tokio::fs::metadata(&path).await.map_err(ToolError::Io)?;
        if !meta.is_file() {
            return Err(ToolError::InvalidInput(format!(
                "{} is not a regular file",
                parsed.file_path
            )));
        }
        if (meta.len() as usize) > self.limits.max_read_bytes {
            return Err(ToolError::LimitExceeded(format!(
                "file is {} bytes, exceeds max_read_bytes {}",
                meta.len(),
                self.limits.max_read_bytes
            )));
        }
        let bytes = tokio::fs::read(&path).await.map_err(ToolError::Io)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput("file is not valid UTF-8".into()))?;

        let metadata = json!({
            "path": parsed.file_path,
            "bytes": meta.len(),
        });
        Ok(ToolResult {
            content: vec![crom_core::ToolContent::Text(text)],
            is_error: false,
            metadata,
        })
    }
}
