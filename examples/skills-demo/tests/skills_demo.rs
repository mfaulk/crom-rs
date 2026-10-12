//! Offline tests for the skills demo. They exercise the M7 SDK paths
//! (Skill discovery, slash command registry, expansion) without making
//! any model calls — all behavior is deterministic.

use std::path::PathBuf;

fn demo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
// The two example Skills are discoverable from .agent/skills.
fn skills_discoverable() {
    let dir = demo_root().join(".agent").join("skills");
    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: dir,
    }];
    let reg = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();
    let names: Vec<_> = reg.names();
    assert!(names.contains(&"release-pr".to_string()), "names={names:?}");
    assert!(
        names.contains(&"rust-api-review".to_string()),
        "names={names:?}"
    );
}

#[test]
// release-pr declares its template resource and we can summarize it
// without executing anything.
fn release_pr_resource_summary() {
    let dir = demo_root().join(".agent").join("skills");
    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: dir,
    }];
    let reg = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();
    let state = crom_core::LoadedSkillsState::new();
    let result = crom_core::SkillTool::load(&reg, &opts, &state, "release-pr").unwrap();
    assert_eq!(result.name, "release-pr");
    assert!(result
        .resources
        .iter()
        .any(|r| r.path.contains("templates/pr-summary.md")));
    assert!(state.is_loaded("release-pr"));
}

#[test]
// Custom commands are discoverable from .agent/commands and include the
// release-pr and review-changes files.
fn commands_discoverable() {
    let dir = demo_root().join(".agent").join("commands");
    let (defs, errors) = crom_core::load_custom_commands(&dir);
    assert!(errors.is_empty(), "errors={errors:?}");
    let names: Vec<_> = defs.iter().map(|d| d.full_name()).collect();
    assert!(names.contains(&"release-pr".to_string()), "names={names:?}");
    assert!(
        names.contains(&"review-changes".to_string()),
        "names={names:?}"
    );
}

#[tokio::test]
// /release-pr v0.4.0 dispatches to the custom command and expands the
// @CHANGELOG.md file reference deterministically (without bash).
async fn release_pr_command_expands_changelog() {
    let dir = demo_root().join(".agent").join("commands");
    let (defs, _errors) = crom_core::load_custom_commands(&dir);
    let reg = crom_core::SlashCommandRegistry::new(true).with_custom(defs);
    let parsed = crom_core::parse_slash_command("/release-pr v0.4.0").unwrap();
    let dispatch = reg.dispatch(parsed.clone()).unwrap();
    let def = match dispatch.handler {
        crom_core::SlashCommandHandler::CustomFile(_) => reg
            .custom(&parsed.full_name())
            .cloned()
            .unwrap(),
        other => panic!("expected CustomFile handler, got {other:?}"),
    };

    struct LocalExec {
        root: PathBuf,
    }
    #[async_trait::async_trait]
    impl crom_core::slash::expansion::SlashExpansionExecutor for LocalExec {
        async fn read_file(
            &self,
            path: &str,
        ) -> Result<String, crom_core::SlashCommandError> {
            std::fs::read_to_string(self.root.join(path))
                .map_err(|e| crom_core::SlashCommandError::ExpansionFailed(e.to_string()))
        }
        async fn run_bash(
            &self,
            _command: &str,
        ) -> Result<String, crom_core::SlashCommandError> {
            // The release-pr command does not use bash; fail loudly if
            // someone changes that without updating the test.
            Err(crom_core::SlashCommandError::ExpansionFailed(
                "test executor refuses bash".into(),
            ))
        }
    }
    let exec = LocalExec { root: demo_root() };
    let opts = crom_core::SlashExpansionOptions::default();
    let result = crom_core::expand_custom_command(&parsed, &def, &opts, &exec)
        .await
        .unwrap();
    assert!(result.expanded_prompt.contains("v0.4.0"));
    assert!(result.expanded_prompt.contains("Greeter"));
    assert_eq!(result.file_refs, vec!["CHANGELOG.md"]);
}

#[test]
// SystemInit-style summaries do not include full Skill bodies.
fn skill_summaries_do_not_leak_body() {
    let dir = demo_root().join(".agent").join("skills");
    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: dir,
    }];
    let reg = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();
    let summaries = reg.list();
    let json = serde_json::to_string(&summaries).unwrap();
    assert!(
        !json.contains("Procedure:"),
        "summary unexpectedly leaked the SKILL.md body"
    );
}
