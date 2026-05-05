use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use code_agent_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::path_sandbox::resolve_within;

#[derive(Debug, Deserialize)]
struct Input {
    file_path: String,
    content: String,
}

pub struct WriteTool {
    limits: Arc<ToolLimits>,
}

impl WriteTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "Write"
    }

    fn description(&self) -> &'static str {
        "Create or overwrite a UTF-8 text file in the workspace. Paths are resolved relative to the workspace root."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["file_path", "content"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::Mutating
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let p = input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let bytes = input
            .get("content")
            .and_then(|v| v.as_str())
            .map(str::len)
            .unwrap_or(0);
        format!("Write {p} ({bytes} bytes)")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Write input: {e}")))?;
        if parsed.content.len() > self.limits.max_read_bytes {
            return Err(ToolError::LimitExceeded(format!(
                "content is {} bytes, exceeds max_read_bytes {}",
                parsed.content.len(),
                self.limits.max_read_bytes
            )));
        }
        let path = resolve_within(&ctx.cwd, &parsed.file_path, false)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(ToolError::Io)?;
        }
        tokio::fs::write(&path, parsed.content.as_bytes())
            .await
            .map_err(ToolError::Io)?;
        let metadata = json!({
            "path": parsed.file_path,
            "bytes": parsed.content.len(),
        });
        Ok(ToolResult {
            content: vec![code_agent_core::ToolContent::Text(format!(
                "Wrote {} bytes to {}",
                parsed.content.len(),
                parsed.file_path
            ))],
            is_error: false,
            metadata,
        })
    }
}
