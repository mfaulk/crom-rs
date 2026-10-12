//! Skills (M7).
//!
//! A *Skill* is a packaged, reusable workflow described by a
//! `SKILL.md` file with YAML frontmatter and a markdown body. Skills are
//! discovered from `.agent/skills/<name>/` (project), an optional user
//! directory, or — opt-in — `.claude/skills/` for compatibility. The
//! initial model context only sees Skill *summaries*; the full Skill
//! body is loaded lazily through the built-in `Skill` tool or a slash
//! command.
//!
//! This module defines:
//! - [`SkillDefinition`] / [`SkillSummary`] — the typed model.
//! - [`SkillRegistry`] — programmatic + filesystem registration.
//! - [`SkillDiscoveryOptions`] — discovery / safety knobs.
//! - [`SkillTool`] — the model-facing `Skill` built-in.
//! - [`LoadedSkillsState`] — per-run "this Skill is now loaded" tracker.
//! - Filesystem loader and safe resource loading.

pub mod definition;
pub mod errors;
pub mod loader;
pub mod registry;
pub mod resources;
pub mod state;
pub mod tool;

pub use definition::{
    is_identifier_safe_skill_name, resource_kind_for, SkillDefinition, SkillResourceKind,
    SkillSource, SkillSummary, MAX_SKILL_NAME_LEN,
};
pub use errors::SkillError;
pub use loader::{
    compat_claude_skills_dir, filesystem_skills_dir, load_filesystem_skills, parse_skill_md,
    DEFAULT_MAX_SKILL_MD_BYTES,
};
pub use registry::{SkillDiscoveryOptions, SkillDiscoveryPath, SkillRegistry};
pub use resources::{
    is_likely_text, load_resource_summary, resolve_resource_path, SkillResourceSummary,
    MAX_RESOURCE_PREVIEW_BYTES,
};
pub use state::{LoadedSkillsSnapshot, LoadedSkillsState};
pub use tool::{SkillTool, SkillToolInput, SkillToolResult, SKILL_TOOL_NAME};
