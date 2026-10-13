//! `TodoWrite` types: a structured progress list the agent can update during
//! multi-step work. Persisted in session JSONL and emitted as agent events
//! so CLIs and TUIs can render progress.

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

impl TodoStatus {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pending" => Some(Self::Pending),
            "in_progress" | "in-progress" | "inprogress" => Some(Self::InProgress),
            "completed" | "done" => Some(Self::Completed),
            "cancelled" | "canceled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoList {
    pub todos: Vec<TodoItem>,
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, todos: Vec<TodoItem>) {
        self.todos = todos;
    }

    pub fn completed_count(&self) -> usize {
        self.todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count()
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        let total = self.todos.len();
        let done = self.completed_count();
        s.push_str(&format!("[todo] {}/{} complete\n", done, total));
        for t in &self.todos {
            let mark = match t.status {
                TodoStatus::Completed => "✓",
                TodoStatus::InProgress => "→",
                TodoStatus::Pending => "·",
                TodoStatus::Cancelled => "x",
            };
            s.push_str(&format!("  {} {}\n", mark, t.content));
        }
        s
    }
}

#[derive(Debug, Error)]
pub enum TodoError {
    #[error("invalid status: {0}")]
    InvalidStatus(String),
    #[error("invalid input: {0}")]
    InvalidInput(String),
}
