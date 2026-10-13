use std::sync::Mutex;

use async_trait::async_trait;
use crom_core::{
    PermissionKind, TodoItem, TodoList, TodoStatus, Tool, ToolContext, ToolError, ToolResult,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct InputItem {
    id: String,
    content: String,
    status: String,
}

#[derive(Debug, Deserialize)]
struct Input {
    todos: Vec<InputItem>,
}

/// `TodoWrite` updates the agent's task list. The tool is idempotent: each
/// call replaces the entire list with a new snapshot. Validates that every
/// status is a known value.
pub struct TodoWriteTool {
    state: Mutex<TodoList>,
}

impl TodoWriteTool {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(TodoList::new()),
        }
    }

    pub fn snapshot(&self) -> TodoList {
        self.state.lock().unwrap().clone()
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn name(&self) -> &str {
        "TodoWrite"
    }

    fn description(&self) -> &str {
        "Replace the agent's todo list with a new snapshot. Use to keep the user informed of multi-step progress. Each item has id, content, and status (pending|in_progress|completed|cancelled)."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string"},
                            "content": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]}
                        },
                        "required": ["id", "content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        // TodoWrite mutates only internal todo state, never the workspace or
        // filesystem, so it is intentionally classified ReadOnly to avoid
        // approval prompts.
        PermissionKind::ReadOnly
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let n = input
            .get("todos")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        format!("TodoWrite ({n} items)")
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: Input = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("TodoWrite input: {e}")))?;
        let mut items: Vec<TodoItem> = Vec::with_capacity(parsed.todos.len());
        for raw in parsed.todos {
            let status = TodoStatus::from_str_lenient(&raw.status).ok_or_else(|| {
                ToolError::InvalidInput(format!("invalid todo status: {}", raw.status))
            })?;
            if raw.id.is_empty() {
                return Err(ToolError::InvalidInput("todo id must be non-empty".into()));
            }
            if raw.content.is_empty() {
                return Err(ToolError::InvalidInput(
                    "todo content must be non-empty".into(),
                ));
            }
            items.push(TodoItem {
                id: raw.id,
                content: raw.content,
                status,
            });
        }

        let list = {
            let mut g = self.state.lock().unwrap();
            g.replace(items);
            g.clone()
        };

        let metadata = json!({
            "todo_list": list,
        });
        Ok(ToolResult {
            content: vec![crom_core::ToolContent::Text(list.render())],
            is_error: false,
            metadata,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crom_core::SessionId;
    use std::path::PathBuf;

    fn ctx() -> ToolContext {
        ToolContext::new_minimal(PathBuf::from("."), SessionId::from("s".to_string()))
    }

    #[tokio::test]
    // TodoWrite accepts a valid list and stores the snapshot.
    async fn accepts_valid_todos() {
        let tool = TodoWriteTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "todos": [
                        {"id": "1", "content": "a", "status": "pending"},
                        {"id": "2", "content": "b", "status": "in_progress"},
                        {"id": "3", "content": "c", "status": "completed"}
                    ]
                }),
                ctx(),
            )
            .await
            .unwrap();
        assert!(!r.is_error);
        assert_eq!(tool.snapshot().todos.len(), 3);
    }

    #[tokio::test]
    // TodoWrite rejects unknown status values.
    async fn rejects_invalid_status() {
        let tool = TodoWriteTool::new();
        let err = tool
            .execute(
                serde_json::json!({
                    "todos": [{"id": "1", "content": "a", "status": "wat"}]
                }),
                ctx(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidInput(_)));
    }

    #[tokio::test]
    // Tool result metadata carries the new todo list snapshot.
    async fn metadata_carries_list() {
        let tool = TodoWriteTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "todos": [{"id": "1", "content": "a", "status": "pending"}]
                }),
                ctx(),
            )
            .await
            .unwrap();
        let list: TodoList = serde_json::from_value(r.metadata["todo_list"].clone()).unwrap();
        assert_eq!(list.todos.len(), 1);
    }
}
