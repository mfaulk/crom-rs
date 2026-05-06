//! Slash-command registry and dispatcher.

use std::collections::BTreeMap;

use super::commands::{
    builtin_command_summary, BuiltInSlashCommand, SlashCommandHandler, SlashCommandSource,
    SlashCommandSummary,
};
use super::custom::CustomSlashCommandDefinition;
use super::errors::SlashCommandError;
use super::parser::ParsedSlashCommand;
use crate::skills::SkillRegistry;

/// Result of resolving a parsed slash command against the registry.
#[derive(Debug, Clone)]
pub struct SlashCommandDispatch {
    pub summary: SlashCommandSummary,
    pub handler: SlashCommandHandler,
    pub parsed: ParsedSlashCommand,
}

/// In-memory registry of slash commands. Holds:
/// - the set of enabled built-ins,
/// - custom command definitions loaded from `.agent/commands/*.md`,
/// - a reference (by name) to the project Skills (so `/release-pr` can
///   resolve to a Skill the user has registered).
#[derive(Debug, Clone, Default)]
pub struct SlashCommandRegistry {
    builtins_enabled: bool,
    custom: BTreeMap<String, CustomSlashCommandDefinition>,
    /// Snapshot of skill names available at registry-construction time.
    /// Used to deterministically expose Skill-backed slash commands.
    skill_names: Vec<String>,
    /// Compact summaries for Skill descriptions (so `/help` can display
    /// them without re-reading the SkillRegistry).
    skill_descriptions: BTreeMap<String, (String, Option<String>)>,
}

impl SlashCommandRegistry {
    pub fn new(builtins_enabled: bool) -> Self {
        Self {
            builtins_enabled,
            custom: BTreeMap::new(),
            skill_names: Vec::new(),
            skill_descriptions: BTreeMap::new(),
        }
    }

    /// Register all custom command definitions.
    pub fn with_custom(mut self, defs: Vec<CustomSlashCommandDefinition>) -> Self {
        for def in defs {
            self.custom.insert(def.full_name(), def);
        }
        self
    }

    /// Snapshot Skill names + descriptions so `/help` and dispatch can see them.
    pub fn with_skills(mut self, registry: &SkillRegistry) -> Self {
        for (name, def) in registry.iter() {
            self.skill_names.push(name.to_string());
            self.skill_descriptions.insert(
                name.to_string(),
                (def.description.clone(), def.argument_hint.clone()),
            );
        }
        self.skill_names.sort();
        self
    }

    /// Returns true if the registry has any custom commands.
    pub fn has_custom(&self) -> bool {
        !self.custom.is_empty()
    }

    pub fn builtins_enabled(&self) -> bool {
        self.builtins_enabled
    }

    pub fn custom(&self, full_name: &str) -> Option<&CustomSlashCommandDefinition> {
        self.custom.get(full_name)
    }

    /// Sorted summaries of every command known to the registry.
    pub fn list(&self) -> Vec<SlashCommandSummary> {
        let mut out: Vec<SlashCommandSummary> = Vec::new();
        if self.builtins_enabled {
            for b in BuiltInSlashCommand::all() {
                out.push(builtin_command_summary(*b));
            }
        }
        // Custom commands.
        for def in self.custom.values() {
            out.push(SlashCommandSummary {
                name: def.name.clone(),
                namespace: def.namespace.clone(),
                description: def.description.clone(),
                source: SlashCommandSource::CustomFile {
                    path: def.source_path.clone(),
                },
                argument_hint: def.argument_hint.clone(),
            });
        }
        // Skill-backed commands. Skip names already taken by a built-in
        // or a custom command — those win.
        let custom_names: std::collections::BTreeSet<String> =
            self.custom.keys().cloned().collect();
        let builtin_names: std::collections::BTreeSet<String> = if self.builtins_enabled {
            BuiltInSlashCommand::all()
                .iter()
                .map(|b| b.name().to_string())
                .collect()
        } else {
            Default::default()
        };
        for name in &self.skill_names {
            if custom_names.contains(name) || builtin_names.contains(name) {
                continue;
            }
            let (desc, hint) = self
                .skill_descriptions
                .get(name)
                .cloned()
                .unwrap_or_else(|| (String::new(), None));
            out.push(SlashCommandSummary {
                name: name.clone(),
                namespace: None,
                description: desc,
                source: SlashCommandSource::Skill {
                    skill_name: name.clone(),
                },
                argument_hint: hint,
            });
        }
        out.sort_by(|a, b| {
            let an = match &a.namespace {
                Some(ns) => format!("{ns}:{}", a.name),
                None => a.name.clone(),
            };
            let bn = match &b.namespace {
                Some(ns) => format!("{ns}:{}", b.name),
                None => b.name.clone(),
            };
            an.cmp(&bn)
        });
        out
    }

    /// Resolve a parsed slash command to a handler. Resolution order:
    /// 1. Custom command files (project-defined wins).
    /// 2. Built-ins.
    /// 3. Skill-backed commands.
    pub fn dispatch(
        &self,
        parsed: ParsedSlashCommand,
    ) -> Result<SlashCommandDispatch, SlashCommandError> {
        let full = parsed.full_name();
        if let Some(def) = self.custom.get(&full) {
            return Ok(SlashCommandDispatch {
                summary: SlashCommandSummary {
                    name: def.name.clone(),
                    namespace: def.namespace.clone(),
                    description: def.description.clone(),
                    source: SlashCommandSource::CustomFile {
                        path: def.source_path.clone(),
                    },
                    argument_hint: def.argument_hint.clone(),
                },
                handler: SlashCommandHandler::CustomFile(def.source_path.clone()),
                parsed,
            });
        }
        if self.builtins_enabled && parsed.namespace.is_none() {
            if let Some(b) = BuiltInSlashCommand::from_name(&parsed.name) {
                return Ok(SlashCommandDispatch {
                    summary: builtin_command_summary(b),
                    handler: SlashCommandHandler::BuiltIn(b),
                    parsed,
                });
            }
        }
        if parsed.namespace.is_none() && self.skill_names.iter().any(|n| n == &parsed.name) {
            let (desc, hint) = self
                .skill_descriptions
                .get(&parsed.name)
                .cloned()
                .unwrap_or_else(|| (String::new(), None));
            return Ok(SlashCommandDispatch {
                summary: SlashCommandSummary {
                    name: parsed.name.clone(),
                    namespace: None,
                    description: desc,
                    source: SlashCommandSource::Skill {
                        skill_name: parsed.name.clone(),
                    },
                    argument_hint: hint,
                },
                handler: SlashCommandHandler::Skill(parsed.name.clone()),
                parsed,
            });
        }
        Err(SlashCommandError::Unknown { name: full })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::definition::SkillSource;
    use crate::skills::SkillDefinition;
    use std::path::PathBuf;

    fn skill(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            description: format!("desc-{name}"),
            instructions: "body".into(),
            argument_hint: Some("<arg>".into()),
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            resources: Vec::new(),
            metadata: serde_json::Value::Null,
            source: SkillSource::Project,
            root_dir: PathBuf::from("/tmp"),
        }
    }

    #[test]
    // Built-ins resolve first when no custom or Skill collides.
    fn dispatch_resolves_builtin() {
        let reg = SlashCommandRegistry::new(true);
        let parsed = super::super::parser::parse_slash_command("/help").unwrap();
        let d = reg.dispatch(parsed).unwrap();
        assert!(matches!(
            d.handler,
            SlashCommandHandler::BuiltIn(BuiltInSlashCommand::Help)
        ));
    }

    #[test]
    // Skill-backed commands resolve when the name is in the skill registry.
    fn dispatch_resolves_skill() {
        let mut sk = SkillRegistry::new();
        sk.register(skill("release-pr")).unwrap();
        let reg = SlashCommandRegistry::new(true).with_skills(&sk);
        let parsed = super::super::parser::parse_slash_command("/release-pr v0.4.0").unwrap();
        let d = reg.dispatch(parsed).unwrap();
        match d.handler {
            SlashCommandHandler::Skill(n) => assert_eq!(n, "release-pr"),
            other => panic!("expected Skill handler, got {other:?}"),
        }
    }

    #[test]
    // Unknown commands return a structured error.
    fn dispatch_unknown_errors() {
        let reg = SlashCommandRegistry::new(true);
        let parsed = super::super::parser::parse_slash_command("/nope").unwrap();
        let err = reg.dispatch(parsed).unwrap_err();
        assert!(matches!(err, SlashCommandError::Unknown { .. }));
    }

    #[test]
    // Custom commands win over Skills with the same name.
    fn custom_wins_over_skill() {
        let mut sk = SkillRegistry::new();
        sk.register(skill("release-pr")).unwrap();
        let custom = CustomSlashCommandDefinition {
            name: "release-pr".to_string(),
            namespace: None,
            description: "from file".to_string(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            body: "body".to_string(),
            source_path: PathBuf::from("/tmp/release-pr.md"),
        };
        let reg = SlashCommandRegistry::new(true)
            .with_custom(vec![custom])
            .with_skills(&sk);
        let parsed = super::super::parser::parse_slash_command("/release-pr").unwrap();
        let d = reg.dispatch(parsed).unwrap();
        assert!(matches!(d.handler, SlashCommandHandler::CustomFile(_)));
    }
}
