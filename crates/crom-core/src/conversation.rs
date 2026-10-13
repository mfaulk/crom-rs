use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_use_id: String,
    pub content: Vec<ToolResultContent>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultContent {
    Text { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

impl Conversation {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, m: Message) {
        self.messages.push(m);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssistantContent {
    Text { text: String },
    ToolUse(ToolCall),
}

impl AssistantMessage {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|b| match b {
            AssistantContent::ToolUse(c) => Some(c),
            _ => None,
        })
    }

    pub fn text(&self) -> String {
        let mut s = String::new();
        for block in &self.content {
            if let AssistantContent::Text { text } = block {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(text);
            }
        }
        s
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInit {
    pub session_id: crate::options::SessionId,
    pub cwd: std::path::PathBuf,
    pub model: String,
    pub tools: Vec<String>,
    pub permission_mode: crate::permission::PermissionMode,
    #[serde(default)]
    pub mcp_servers: Vec<crom_mcp::McpServerStatus>,
    /// Compact summaries of registered Skills (M7). Full Skill bodies are
    /// loaded on demand through the `Skill` tool — the initial context only
    /// sees these summaries.
    #[serde(default)]
    pub skills: Vec<crate::skills::SkillSummary>,
    /// Compact summaries of registered slash commands (M7).
    #[serde(default)]
    pub slash_commands: Vec<crate::slash::SlashCommandSummary>,
    /// Compact summaries of installed plugins (M8). Empty when the
    /// plugin system is disabled.
    #[serde(default)]
    pub plugins: Vec<crate::plugins::PluginSummary>,
}
