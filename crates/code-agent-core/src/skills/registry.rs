use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::definition::{SkillDefinition, SkillSource, SkillSummary};
use super::errors::SkillError;

/// In-memory map of registered Skill definitions.
#[derive(Default, Debug, Clone)]
pub struct SkillRegistry {
    skills: BTreeMap<String, RegistryEntry>,
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    definition: SkillDefinition,
    /// Origin tracked separately because users may register the same Skill
    /// from multiple sources; project always wins over user, builtin
    /// always loses to filesystem.
    origin: SkillSource,
}

/// Knobs for the registry/loader: which paths to read, what limits to
/// enforce, and how to behave when duplicates appear.
#[derive(Debug, Clone)]
pub struct SkillDiscoveryOptions {
    pub allow_filesystem_skills: bool,
    pub compat_claude_skills: bool,
    pub max_skill_md_bytes: usize,
    pub max_resource_bytes_per_skill: usize,
    pub max_loaded_skills_per_run: usize,
    pub load_resources_on_demand: bool,
    pub enforce_skill_allowed_tools: bool,
}

impl Default for SkillDiscoveryOptions {
    fn default() -> Self {
        Self {
            allow_filesystem_skills: true,
            compat_claude_skills: false,
            max_skill_md_bytes: super::loader::DEFAULT_MAX_SKILL_MD_BYTES,
            max_resource_bytes_per_skill: 1_048_576,
            max_loaded_skills_per_run: 8,
            load_resources_on_demand: true,
            enforce_skill_allowed_tools: false,
        }
    }
}

/// Where the registry should look for Skills. Loaded in order; later
/// sources can override earlier ones according to deterministic merge
/// rules (project > user > compat-claude > builtin).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDiscoveryPath {
    pub source: SkillSource,
    pub path: std::path::PathBuf,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.skills.len()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.skills.contains_key(name)
    }

    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name).map(|e| &e.definition)
    }

    /// Sorted summaries.
    pub fn list(&self) -> Vec<SkillSummary> {
        self.skills
            .values()
            .map(|e| e.definition.summary())
            .collect()
    }

    /// Sorted definition iter.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SkillDefinition)> {
        self.skills.iter().map(|(k, v)| (k.as_str(), &v.definition))
    }

    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Drop every registered Skill. Used by the `--no-skills` flag and tests.
    pub fn clear(&mut self) {
        self.skills.clear();
    }

    /// Register a Skill. Errors on duplicate names by default.
    pub fn register(&mut self, definition: SkillDefinition) -> Result<(), SkillError> {
        let origin = definition.source;
        self.register_with_origin(definition, origin, false)
    }

    /// Generic registration. `allow_override` lets callers explicitly
    /// replace an existing entry (useful for the CLI's `--force` flag).
    pub fn register_with_origin(
        &mut self,
        definition: SkillDefinition,
        origin: SkillSource,
        allow_override: bool,
    ) -> Result<(), SkillError> {
        definition.validate_structural()?;
        if let Some(existing) = self.skills.get(&definition.name) {
            if existing.origin == origin && !allow_override {
                return Err(SkillError::DuplicateName {
                    name: definition.name,
                });
            }
            // Merge precedence: project > user > compat > builtin.
            // Lower rank = more trusted. Reject silent demotions (a less-
            // trusted source displacing a more-trusted one) unless
            // `allow_override`.
            let new_rank = source_rank(origin);
            let existing_rank = source_rank(existing.origin);
            if new_rank > existing_rank && !allow_override {
                return Err(SkillError::DuplicateName {
                    name: definition.name,
                });
            }
        }
        self.skills.insert(
            definition.name.clone(),
            RegistryEntry { definition, origin },
        );
        Ok(())
    }

    /// Discover Skills from the configured paths.
    pub fn discover(
        paths: &[SkillDiscoveryPath],
        options: &SkillDiscoveryOptions,
    ) -> Result<Self, SkillError> {
        let mut registry = SkillRegistry::new();
        if !options.allow_filesystem_skills {
            return Ok(registry);
        }
        for path_def in paths {
            if matches!(path_def.source, SkillSource::CompatibilityClaude)
                && !options.compat_claude_skills
            {
                continue;
            }
            let (defs, errors) = super::loader::load_filesystem_skills(
                &path_def.path,
                path_def.source,
                options.max_skill_md_bytes,
            );
            for e in errors {
                tracing::warn!(error = %e, "skipping invalid skill");
            }
            for d in defs {
                if let Err(e) = registry.register_with_origin(d, path_def.source, false) {
                    tracing::warn!(error = %e, "skipping conflicting skill");
                }
            }
        }
        Ok(registry)
    }
}

/// Lower number = higher precedence (project is most trusted).
fn source_rank(s: SkillSource) -> u8 {
    match s {
        SkillSource::Project => 0,
        SkillSource::User => 1,
        SkillSource::BuiltIn => 2,
        SkillSource::CompatibilityClaude => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn def(name: &str, source: SkillSource) -> SkillDefinition {
        SkillDefinition {
            name: name.into(),
            description: "d".into(),
            instructions: "p".into(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            resources: Vec::new(),
            metadata: serde_json::Value::Null,
            source,
            root_dir: PathBuf::from("/tmp/skill"),
        }
    }

    #[test]
    // Registration and lookup work as expected.
    fn register_and_lookup() {
        let mut r = SkillRegistry::new();
        r.register(def("alpha", SkillSource::Project)).unwrap();
        assert!(r.contains("alpha"));
        assert_eq!(r.len(), 1);
    }

    #[test]
    // Duplicate same-origin skills are rejected.
    fn duplicate_same_origin_rejected() {
        let mut r = SkillRegistry::new();
        r.register(def("alpha", SkillSource::Project)).unwrap();
        let err = r.register(def("alpha", SkillSource::Project)).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateName { .. }));
    }

    #[test]
    // A higher-precedence source can silently override a lower one.
    fn project_overrides_user() {
        let mut r = SkillRegistry::new();
        let mut user = def("alpha", SkillSource::User);
        user.description = "user".into();
        r.register(user).unwrap();
        let mut proj = def("alpha", SkillSource::Project);
        proj.description = "project".into();
        r.register(proj).unwrap();
        assert_eq!(r.get("alpha").unwrap().description, "project");
    }

    #[test]
    // A lower-precedence source cannot silently displace a higher one.
    fn user_does_not_displace_project() {
        let mut r = SkillRegistry::new();
        let proj = def("alpha", SkillSource::Project);
        r.register(proj).unwrap();
        let user = def("alpha", SkillSource::User);
        let err = r.register(user).unwrap_err();
        assert!(matches!(err, SkillError::DuplicateName { .. }));
    }

    #[test]
    // list() returns deterministic, sorted output.
    fn list_is_sorted() {
        let mut r = SkillRegistry::new();
        r.register(def("zulu", SkillSource::Project)).unwrap();
        r.register(def("alpha", SkillSource::Project)).unwrap();
        r.register(def("mike", SkillSource::Project)).unwrap();
        let names: Vec<_> = r.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }
}
