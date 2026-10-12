use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::definition::{AgentDefinition, AgentDefinitionSummary};
use super::errors::AgentDefinitionError;

/// Where a particular [`AgentDefinition`] came from. The registry tracks
/// origin so deterministic-merge rules can be applied without re-parsing
/// definitions, and so the CLI can show a provenance column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentDefinitionOrigin {
    /// A built-in starter definition shipped with the binary.
    Builtin,
    /// Registered via the SDK builder (`AgentClientBuilder::agent`).
    Programmatic,
    /// Loaded from a `.agent/agents/<name>.md` file.
    Filesystem,
}

/// In-memory map of registered subagent definitions.
#[derive(Default, Debug, Clone)]
pub struct AgentRegistry {
    agents: BTreeMap<String, RegistryEntry>,
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    definition: AgentDefinition,
    origin: AgentDefinitionOrigin,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of registered definitions.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Register a definition. Errors on duplicate names by default. Use
    /// [`Self::register_with_origin`] for finer control over precedence.
    pub fn register(&mut self, definition: AgentDefinition) -> Result<(), AgentDefinitionError> {
        self.register_with_origin(definition, AgentDefinitionOrigin::Programmatic, false)
    }

    /// Register a built-in starter definition. Built-ins do not displace
    /// other origins.
    pub fn register_builtin(
        &mut self,
        definition: AgentDefinition,
    ) -> Result<(), AgentDefinitionError> {
        self.register_with_origin(definition, AgentDefinitionOrigin::Builtin, false)
    }

    /// Register a filesystem definition. Allows overriding a built-in by
    /// the same name; refuses to silently displace a programmatic definition.
    pub fn register_filesystem(
        &mut self,
        definition: AgentDefinition,
    ) -> Result<(), AgentDefinitionError> {
        self.register_with_origin(definition, AgentDefinitionOrigin::Filesystem, false)
    }

    /// Generic registration. `allow_override` lets callers (e.g. tests, the
    /// `--force` CLI flag) explicitly opt into replacing an existing entry.
    pub fn register_with_origin(
        &mut self,
        definition: AgentDefinition,
        origin: AgentDefinitionOrigin,
        allow_override: bool,
    ) -> Result<(), AgentDefinitionError> {
        definition.validate_structural()?;
        if let Some(existing) = self.agents.get(&definition.name) {
            // Deterministic merge rules:
            // - Programmatic always wins over Built-in (so SDK callers can
            //   override starter agents).
            // - Filesystem can override Built-in (project tweaks ship with
            //   the project), but cannot silently displace Programmatic.
            // - Same-origin duplicates are rejected unless `allow_override`.
            match (existing.origin, origin) {
                (a, b) if a == b && !allow_override => {
                    return Err(AgentDefinitionError::DuplicateName {
                        name: definition.name,
                    });
                }
                (AgentDefinitionOrigin::Builtin, _) => { /* override OK */ }
                (AgentDefinitionOrigin::Programmatic, AgentDefinitionOrigin::Filesystem)
                    if !allow_override =>
                {
                    return Err(AgentDefinitionError::DuplicateName {
                        name: definition.name,
                    });
                }
                _ => {}
            }
        }
        self.agents.insert(
            definition.name.clone(),
            RegistryEntry { definition, origin },
        );
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name).map(|e| &e.definition)
    }

    pub fn origin(&self, name: &str) -> Option<AgentDefinitionOrigin> {
        self.agents.get(name).map(|e| e.origin)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.agents.contains_key(name)
    }

    /// Sorted (by name) summaries of all registered agents.
    pub fn list(&self) -> Vec<AgentDefinitionSummary> {
        // BTreeMap iteration is already sorted by key.
        self.agents
            .values()
            .map(|e| e.definition.summary())
            .collect()
    }

    /// Sorted list of agent names. Convenience for CLI output.
    pub fn names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }

    /// Iterate `(name, definition)` pairs in deterministic order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &AgentDefinition)> {
        self.agents.iter().map(|(k, v)| (k.as_str(), &v.definition))
    }

    /// Validate every definition against the supplied set of available tool
    /// names. Returns `Err` on the first mismatch. Useful for the `agents
    /// validate` CLI command.
    pub fn validate_against_tools(
        &self,
        available: &BTreeSet<String>,
    ) -> Result<(), AgentDefinitionError> {
        for (name, def) in self.iter() {
            def.validate_structural()?;
            let mut unknown: Vec<String> = super::definition::referenced_tool_names(def)
                .into_iter()
                .filter(|t| !available.contains(t))
                // The built-in `Agent` tool name is also valid (subagent
                // definitions reject it explicitly via `validate_structural`,
                // so we won't see it here, but be defensive).
                .filter(|t| t != super::tool::SUBAGENT_TOOL_NAME)
                .collect();
            unknown.sort();
            if !unknown.is_empty() {
                return Err(AgentDefinitionError::UnknownTools {
                    name: name.to_string(),
                    tools: unknown.join(", "),
                });
            }
        }
        Ok(())
    }

    /// Drop every registered agent. Mostly used by tests and the CLI's
    /// `--no-agents` flag.
    pub fn clear(&mut self) {
        self.agents.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_rule::ToolRule;

    fn def(name: &str) -> AgentDefinition {
        AgentDefinition {
            name: name.into(),
            description: "d".into(),
            prompt: "p".into(),
            ..Default::default()
        }
    }

    #[test]
    // A registry can register a programmatic definition and look it up by name.
    fn register_and_get() {
        let mut r = AgentRegistry::new();
        r.register(def("alpha")).unwrap();
        assert_eq!(r.get("alpha").map(|d| d.name.clone()), Some("alpha".into()));
        assert_eq!(r.len(), 1);
    }

    #[test]
    // Duplicate same-origin registration produces a DuplicateName error.
    fn register_rejects_duplicate_same_origin() {
        let mut r = AgentRegistry::new();
        r.register(def("alpha")).unwrap();
        let err = r.register(def("alpha")).expect_err("must reject duplicate");
        assert!(matches!(err, AgentDefinitionError::DuplicateName { .. }));
    }

    #[test]
    // Programmatic definitions override built-ins without complaining.
    fn programmatic_overrides_builtin() {
        let mut r = AgentRegistry::new();
        let mut b = def("alpha");
        b.description = "builtin".into();
        r.register_builtin(b).unwrap();
        let mut p = def("alpha");
        p.description = "programmatic".into();
        r.register(p).expect("programmatic overrides builtin");
        assert_eq!(
            r.get("alpha").map(|d| d.description.clone()),
            Some("programmatic".into())
        );
    }

    #[test]
    // Filesystem definitions cannot silently displace programmatic ones.
    fn filesystem_does_not_silently_override_programmatic() {
        let mut r = AgentRegistry::new();
        r.register(def("alpha")).unwrap();
        let err = r
            .register_filesystem(def("alpha"))
            .expect_err("filesystem must not silently win");
        assert!(matches!(err, AgentDefinitionError::DuplicateName { .. }));
    }

    #[test]
    // `list()` returns definitions sorted by name.
    fn list_is_sorted() {
        let mut r = AgentRegistry::new();
        r.register(def("zulu")).unwrap();
        r.register(def("alpha")).unwrap();
        r.register(def("mike")).unwrap();
        let names: Vec<String> = r.list().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    #[test]
    // validate_against_tools surfaces unknown tools cleanly.
    fn validate_reports_unknown_tools() {
        let mut r = AgentRegistry::new();
        let mut d = def("alpha");
        d.tools = Some(vec![
            ToolRule::ExactTool("Read".into()),
            ToolRule::ExactTool("DoesNotExist".into()),
        ]);
        r.register(d).unwrap();
        let mut available = BTreeSet::new();
        available.insert("Read".into());
        let err = r
            .validate_against_tools(&available)
            .expect_err("must flag unknown");
        match err {
            AgentDefinitionError::UnknownTools { name, tools } => {
                assert_eq!(name, "alpha");
                assert!(tools.contains("DoesNotExist"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    // Glob rules in tools/disallowed_tools are not flagged as "unknown" — we
    // only enumerate exact-tool references, since globs may match runtime-
    // discovered MCP tools.
    fn validate_does_not_complain_about_globs() {
        let mut r = AgentRegistry::new();
        let mut d = def("alpha");
        d.tools = Some(vec![ToolRule::try_tool_glob("mcp__*").unwrap()]);
        r.register(d).unwrap();
        let available = BTreeSet::new();
        r.validate_against_tools(&available).unwrap();
    }
}
