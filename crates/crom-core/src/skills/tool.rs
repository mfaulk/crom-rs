//! The built-in `Skill` tool.
//!
//! When invoked, the tool resolves a registered Skill by name, returns its
//! full instructions and a list of resource summaries, marks it as loaded
//! for the current run, and never executes anything from the Skill.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::errors::ToolError;
use crate::permission::PermissionKind;
use crate::tool::{Tool, ToolContent, ToolContext, ToolResult};

use super::definition::SkillSource;
use super::registry::{SkillDiscoveryOptions, SkillRegistry};
use super::resources::SkillResourceSummary;
use super::state::LoadedSkillsState;

pub const SKILL_TOOL_NAME: &str = "Skill";

/// Input schema accepted by the [`SkillTool`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillToolInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
}

/// Result returned to the model after a successful Skill invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillToolResult {
    pub name: String,
    pub source: SkillSource,
    pub instructions: String,
    pub resources: Vec<SkillResourceSummary>,
    pub loaded_files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash_command: Option<String>,
    #[serde(default)]
    pub argument_hint: Option<String>,
}

/// Tool implementation for the `Skill` built-in.
pub struct SkillTool {
    registry: Arc<SkillRegistry>,
    options: SkillDiscoveryOptions,
    state: LoadedSkillsState,
}

impl SkillTool {
    pub fn new(
        registry: Arc<SkillRegistry>,
        options: SkillDiscoveryOptions,
        state: LoadedSkillsState,
    ) -> Self {
        Self {
            registry,
            options,
            state,
        }
    }

    /// Convenience for callers (e.g. SDK / slash dispatcher) to load a
    /// Skill without going through the tool surface.
    pub fn load(
        registry: &SkillRegistry,
        options: &SkillDiscoveryOptions,
        state: &LoadedSkillsState,
        name: &str,
    ) -> Result<SkillToolResult, super::errors::SkillError> {
        let def = registry
            .get(name)
            .ok_or_else(|| super::errors::SkillError::NotFound {
                name: name.to_string(),
            })?;

        // Enforce the per-run loaded-skill cap. Re-loads of the same skill
        // do not count toward the cap.
        if !state.is_loaded(name) && state.count() >= options.max_loaded_skills_per_run {
            return Err(super::errors::SkillError::LoadedLimitExceeded {
                name: name.to_string(),
                max: options.max_loaded_skills_per_run,
            });
        }

        let mut budget = options.max_resource_bytes_per_skill;
        let mut resources = Vec::new();
        let mut loaded_files = Vec::new();
        for path in &def.resources {
            match super::resources::load_resource_summary(
                &def.root_dir,
                path,
                &def.name,
                &mut budget,
            ) {
                Ok(summary) => {
                    if summary.loaded {
                        loaded_files.push(summary.path.clone());
                    }
                    resources.push(summary);
                }
                Err(e) => {
                    tracing::warn!(skill = %def.name, error = %e, "skipping skill resource");
                }
            }
        }
        state.mark_loaded(name);
        Ok(SkillToolResult {
            name: def.name.clone(),
            source: def.source,
            instructions: def.instructions.clone(),
            resources,
            loaded_files,
            slash_command: Some(format!("/{}", def.name)),
            argument_hint: def.argument_hint.clone(),
        })
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        SKILL_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Load a project Skill: returns the Skill's instructions and a summary of its resources \
         (files inside the Skill directory). Skills are not executed; their scripts must still be \
         invoked through the normal Bash tool with full approval."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Registered skill name." },
                "reason": { "type": "string", "description": "Why the model chose this skill." },
                "arguments": {
                    "type": "object",
                    "description": "Optional structured arguments for the skill.",
                    "additionalProperties": true
                }
            },
            "required": ["name"]
        })
    }

    fn permission_kind(&self) -> PermissionKind {
        // Treat as an orchestration action: not auto-approved by acceptEdits;
        // explicit allow rules are required.
        PermissionKind::Orchestration
    }

    fn approval_summary(&self, input: &serde_json::Value) -> String {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("<unknown>");
        format!("Skill({})", name)
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        _ctx: ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let parsed: SkillToolInput = serde_json::from_value(input)
            .map_err(|e| ToolError::InvalidInput(format!("invalid Skill input: {e}")))?;
        let name = parsed.name.trim();
        if name.is_empty() {
            return Err(ToolError::InvalidInput("skill name required".to_string()));
        }
        let result = Self::load(&self.registry, &self.options, &self.state, name)
            .map_err(|e| ToolError::ExecutionFailed(format!("skill {name}: {e}")))?;
        let body = serde_json::to_string(&result)
            .map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;
        Ok(ToolResult {
            content: vec![ToolContent::Text(body)],
            is_error: false,
            metadata: serde_json::json!({
                "skill": result.name,
                "source": result.source.as_str(),
                "loaded_resources": result.loaded_files.len(),
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::SessionId;
    use crate::skills::definition::SkillDefinition;
    use std::path::PathBuf;

    fn make(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            description: "d".into(),
            instructions: "do the thing".into(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            resources: Vec::new(),
            metadata: serde_json::Value::Null,
            source: SkillSource::Project,
            root_dir: PathBuf::from("/tmp"),
        }
    }

    #[tokio::test]
    // Skill tool returns instructions for a registered skill.
    async fn loads_registered_skill() {
        let mut reg = SkillRegistry::new();
        reg.register(make("alpha")).unwrap();
        let opts = SkillDiscoveryOptions::default();
        let state = LoadedSkillsState::new();
        let tool = SkillTool::new(Arc::new(reg), opts, state.clone());
        let out = tool
            .execute(
                serde_json::json!({"name": "alpha"}),
                ToolContext::new_minimal(PathBuf::from("/tmp"), SessionId::new()),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(state.is_loaded("alpha"));
    }

    #[tokio::test]
    // Skill tool reports unknown skill clearly.
    async fn unknown_skill_errors() {
        let reg = SkillRegistry::new();
        let opts = SkillDiscoveryOptions::default();
        let state = LoadedSkillsState::new();
        let tool = SkillTool::new(Arc::new(reg), opts, state);
        let err = tool
            .execute(
                serde_json::json!({"name": "missing"}),
                ToolContext::new_minimal(PathBuf::from("/tmp"), SessionId::new()),
            )
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("missing"));
    }

    #[tokio::test]
    // Skill tool enforces the per-run cap.
    async fn enforces_loaded_cap() {
        let mut reg = SkillRegistry::new();
        reg.register(make("a")).unwrap();
        reg.register(make("b")).unwrap();
        let opts = SkillDiscoveryOptions {
            max_loaded_skills_per_run: 1,
            ..SkillDiscoveryOptions::default()
        };
        let state = LoadedSkillsState::new();
        let tool = SkillTool::new(Arc::new(reg), opts, state.clone());
        let _ = tool
            .execute(
                serde_json::json!({"name": "a"}),
                ToolContext::new_minimal(PathBuf::from("/tmp"), SessionId::new()),
            )
            .await
            .unwrap();
        let err = tool
            .execute(
                serde_json::json!({"name": "b"}),
                ToolContext::new_minimal(PathBuf::from("/tmp"), SessionId::new()),
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("maximum loaded skills"));
    }

    #[tokio::test]
    // Re-loading the same skill is idempotent and doesn't count against the cap.
    async fn reload_does_not_recount() {
        let mut reg = SkillRegistry::new();
        reg.register(make("a")).unwrap();
        let opts = SkillDiscoveryOptions {
            max_loaded_skills_per_run: 1,
            ..SkillDiscoveryOptions::default()
        };
        let state = LoadedSkillsState::new();
        let tool = SkillTool::new(Arc::new(reg), opts, state.clone());
        for _ in 0..3 {
            tool.execute(
                serde_json::json!({"name": "a"}),
                ToolContext::new_minimal(PathBuf::from("/tmp"), SessionId::new()),
            )
            .await
            .unwrap();
        }
        assert_eq!(state.count(), 1);
        assert_eq!(state.invocation_count(), 3);
    }

    #[test]
    // The Skill tool's permission kind is Orchestration (not auto-approved
    // by acceptEdits).
    fn permission_kind_is_orchestration() {
        let reg = SkillRegistry::new();
        let opts = SkillDiscoveryOptions::default();
        let state = LoadedSkillsState::new();
        let tool = SkillTool::new(Arc::new(reg), opts, state);
        assert!(matches!(
            tool.permission_kind(),
            PermissionKind::Orchestration
        ));
    }
}
