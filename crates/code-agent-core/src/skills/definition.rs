use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::tool_rule::ToolRule;

use super::errors::SkillError;

/// Maximum length of the Skill name. Mirrors the agent-name cap so paths
/// stay well under filesystem limits.
pub const MAX_SKILL_NAME_LEN: usize = 64;

/// Where a particular [`SkillDefinition`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    /// Loaded from `<cwd>/.agent/skills/<name>/SKILL.md`.
    Project,
    /// Loaded from the user-level skills directory.
    User,
    /// Loaded from `.claude/skills/<name>/SKILL.md` (compatibility mode).
    CompatibilityClaude,
    /// Registered programmatically (typically a built-in starter).
    BuiltIn,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
            Self::CompatibilityClaude => "compat-claude",
            Self::BuiltIn => "builtin",
        }
    }
}

/// Classification of a Skill resource (textual vs binary etc.). Used to
/// produce safe summaries — the runtime never executes anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillResourceKind {
    Markdown,
    Text,
    Template,
    Script,
    Data,
    Binary,
    Unknown,
}

/// A typed Skill definition. Loaded programmatically or from
/// `.agent/skills/<name>/SKILL.md`.
#[derive(Debug, Clone)]
pub struct SkillDefinition {
    pub name: String,
    pub description: String,
    /// Body of the SKILL.md (the markdown after the frontmatter). Loaded
    /// only when the Skill is actually invoked.
    pub instructions: String,
    pub argument_hint: Option<String>,
    /// Frontmatter `allowed-tools` rules (may narrow visibility when the
    /// skill is active and `enforce_skill_allowed_tools` is configured).
    pub allowed_tools: Vec<ToolRule>,
    pub disallowed_tools: Vec<ToolRule>,
    /// Skill-relative resource paths declared in frontmatter.
    pub resources: Vec<String>,
    /// Free-form metadata (anything from the frontmatter we don't model).
    pub metadata: serde_json::Value,
    pub source: SkillSource,
    /// Absolute, canonicalized path to the directory containing SKILL.md.
    pub root_dir: PathBuf,
}

impl SkillDefinition {
    /// Validate this definition without consulting the live tool catalog.
    pub fn validate_structural(&self) -> Result<(), SkillError> {
        if self.name.is_empty() {
            return Err(SkillError::MissingName);
        }
        if !is_identifier_safe_skill_name(&self.name) {
            return Err(SkillError::UnsafeName {
                name: self.name.clone(),
            });
        }
        if self.description.trim().is_empty() {
            return Err(SkillError::MissingDescription {
                name: self.name.clone(),
            });
        }
        if self.instructions.trim().is_empty() {
            return Err(SkillError::MissingBody {
                name: self.name.clone(),
            });
        }
        Ok(())
    }

    /// Compact projection used for listing and event payloads. Excludes the
    /// full SKILL.md body so it can be safely included in `SystemInit`.
    pub fn summary(&self) -> SkillSummary {
        SkillSummary {
            name: self.name.clone(),
            description: self.description.clone(),
            source: self.source,
            argument_hint: self.argument_hint.clone(),
            resource_count: self.resources.len(),
        }
    }
}

/// Compact projection of a [`SkillDefinition`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    #[serde(default)]
    pub resource_count: usize,
}

/// Returns true iff `name` is safe to use as a CLI argument, filesystem
/// component, and policy key. Allowlist is intentionally tight.
pub fn is_identifier_safe_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_SKILL_NAME_LEN {
        return false;
    }
    if name == "." || name == ".." {
        return false;
    }
    name.chars()
        .all(|c| matches!(c, 'a'..='z' | '0'..='9' | '-' | '_'))
}

/// Classify a resource by extension. Heuristic-only — the loader still does
/// a content sniff to decide if a file is binary.
pub fn resource_kind_for(path: &str) -> SkillResourceKind {
    let lower = path.to_ascii_lowercase();
    let ext = std::path::Path::new(&lower)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    match ext {
        "md" | "markdown" => SkillResourceKind::Markdown,
        "txt" | "log" => SkillResourceKind::Text,
        "tmpl" | "template" | "tpl" | "hbs" => SkillResourceKind::Template,
        "sh" | "bash" | "zsh" | "ps1" | "py" | "rb" | "pl" => SkillResourceKind::Script,
        "json" | "yaml" | "yml" | "toml" | "csv" | "tsv" | "xml" => SkillResourceKind::Data,
        "" => SkillResourceKind::Unknown,
        _ => SkillResourceKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.to_string(),
            description: "desc".to_string(),
            instructions: "you are helpful".to_string(),
            argument_hint: None,
            allowed_tools: Vec::new(),
            disallowed_tools: Vec::new(),
            resources: Vec::new(),
            metadata: serde_json::Value::Null,
            source: SkillSource::Project,
            root_dir: PathBuf::from("/tmp/skill"),
        }
    }

    #[test]
    // A basic, well-formed skill validates cleanly.
    fn structural_validate_passes() {
        make("release-pr").validate_structural().unwrap();
    }

    #[test]
    // Empty names are rejected.
    fn empty_name_rejected() {
        let mut d = make("");
        d.name = String::new();
        let err = d.validate_structural().expect_err("must reject");
        assert!(matches!(err, SkillError::MissingName));
    }

    #[test]
    // Names containing slashes or upper-case letters are rejected.
    fn unsafe_names_rejected() {
        for bad in &["UP", "../etc", "with space", "name/slash", "name;rm", ".."] {
            let d = make(bad);
            let err = d.validate_structural().expect_err("must reject");
            assert!(
                matches!(err, SkillError::UnsafeName { .. }),
                "expected UnsafeName for {bad:?}, got {err:?}"
            );
        }
    }

    #[test]
    // Missing description is rejected.
    fn missing_description_rejected() {
        let mut d = make("ok");
        d.description = String::new();
        let err = d.validate_structural().expect_err("must reject");
        assert!(matches!(err, SkillError::MissingDescription { .. }));
    }

    #[test]
    // Missing instructions body is rejected.
    fn missing_body_rejected() {
        let mut d = make("ok");
        d.instructions = String::new();
        let err = d.validate_structural().expect_err("must reject");
        assert!(matches!(err, SkillError::MissingBody { .. }));
    }

    #[test]
    // The summary omits the full instructions body.
    fn summary_excludes_body() {
        let mut d = make("ok");
        d.instructions = "secret-instructions".to_string();
        let s = d.summary();
        assert_eq!(s.name, "ok");
        // Best-effort: serialize and confirm body is absent.
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("secret-instructions"));
    }

    #[test]
    // Resource kind classification covers the common file extensions.
    fn resource_kind_classification() {
        assert_eq!(resource_kind_for("a.md"), SkillResourceKind::Markdown);
        assert_eq!(resource_kind_for("b.tmpl"), SkillResourceKind::Template);
        assert_eq!(resource_kind_for("c.sh"), SkillResourceKind::Script);
        assert_eq!(resource_kind_for("d.json"), SkillResourceKind::Data);
        assert_eq!(resource_kind_for("e.txt"), SkillResourceKind::Text);
        assert_eq!(resource_kind_for("f.bin"), SkillResourceKind::Unknown);
    }
}
