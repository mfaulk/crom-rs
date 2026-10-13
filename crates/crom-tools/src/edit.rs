use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use similar::TextDiff;

use crom_core::{PermissionKind, Tool, ToolContext, ToolError, ToolResult};

use crate::limits::ToolLimits;
use crate::path_sandbox::resolve_within;

#[derive(Debug, Deserialize)]
struct Input {
    file_path: String,
    old_string: String,
    new_string: String,
}

pub struct EditTool {
    limits: Arc<ToolLimits>,
}

impl EditTool {
    pub fn new(limits: Arc<ToolLimits>) -> Self {
        Self { limits }
    }
}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Replace exactly one occurrence of `old_string` with `new_string` inside a UTF-8 file. Errors if `old_string` is missing or appears more than once."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"}
            },
            "required": ["file_path", "old_string", "new_string"]
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
        format!("Edit {p}")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("Edit input: {e}")))?;
        if parsed.old_string.is_empty() {
            return Err(ToolError::InvalidInput(
                "old_string must not be empty".into(),
            ));
        }
        let path = resolve_within(&ctx.cwd, &parsed.file_path, true)?;
        let bytes = tokio::fs::read(&path).await.map_err(ToolError::Io)?;
        if bytes.len() > self.limits.max_read_bytes {
            return Err(ToolError::LimitExceeded(format!(
                "file is {} bytes, exceeds max_read_bytes {}",
                bytes.len(),
                self.limits.max_read_bytes
            )));
        }
        let original = String::from_utf8(bytes)
            .map_err(|_| ToolError::InvalidInput("file is not valid UTF-8".into()))?;

        let occurrences = original.matches(&parsed.old_string).count();
        if occurrences == 0 {
            return Err(ToolError::InvalidInput(
                "old_string not found in file".into(),
            ));
        }
        if occurrences > 1 {
            return Err(ToolError::InvalidInput(format!(
                "old_string occurs {occurrences} times; provide a more specific string"
            )));
        }

        let updated = original.replacen(&parsed.old_string, &parsed.new_string, 1);
        tokio::fs::write(&path, updated.as_bytes())
            .await
            .map_err(ToolError::Io)?;

        let diff = TextDiff::from_lines(&original, &updated);
        let mut summary = String::new();
        for change in diff.iter_all_changes() {
            let sign = match change.tag() {
                similar::ChangeTag::Delete => "-",
                similar::ChangeTag::Insert => "+",
                similar::ChangeTag::Equal => " ",
            };
            if matches!(
                change.tag(),
                similar::ChangeTag::Delete | similar::ChangeTag::Insert
            ) {
                summary.push_str(sign);
                summary.push_str(change.value());
            }
        }
        let summary = crate::truncate_with_marker(&summary, self.limits.max_output_bytes);
        let metadata = json!({
            "path": parsed.file_path,
            "replacements": 1,
            "diff": summary,
        });
        Ok(ToolResult {
            content: vec![crom_core::ToolContent::Text(format!(
                "Edited {} (1 replacement)\n{}",
                parsed.file_path, summary
            ))],
            is_error: false,
            metadata,
        })
    }
}
