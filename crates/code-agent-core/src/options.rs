use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::permission::{PermissionCallback, PermissionMode};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for SessionId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
pub struct AgentOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub allowed_tools: Vec<String>,
    pub permission_mode: PermissionMode,
    pub max_turns: Option<u32>,
    pub session_id: Option<SessionId>,
    pub permission_callback: Option<Arc<dyn PermissionCallback>>,
    pub system_prompt: Option<String>,
    pub project_instructions: Option<String>,
    pub max_tokens: u32,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            model: "claude-sonnet-4-6".to_string(),
            allowed_tools: vec![
                "Read".to_string(),
                "Glob".to_string(),
                "Grep".to_string(),
                "Edit".to_string(),
                "Write".to_string(),
                "Bash".to_string(),
            ],
            permission_mode: PermissionMode::Default,
            max_turns: Some(8),
            session_id: None,
            permission_callback: None,
            system_prompt: None,
            project_instructions: None,
            max_tokens: 4096,
        }
    }
}

impl std::fmt::Debug for AgentOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentOptions")
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("allowed_tools", &self.allowed_tools)
            .field("permission_mode", &self.permission_mode)
            .field("max_turns", &self.max_turns)
            .field("session_id", &self.session_id)
            .field("max_tokens", &self.max_tokens)
            .finish()
    }
}
