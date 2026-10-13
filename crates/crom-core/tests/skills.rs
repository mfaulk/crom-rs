//! Integration tests for M7 Skills + slash commands. These exercise the
//! full SDK surface — registry discovery, the `Skill` tool, slash
//! dispatch, and manual compaction — without making any model calls.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use crom_core::{
    query, AgentEvent, AgentOptions, ModelEvent, ModelProvider, ModelRequest, PermissionMode,
    ProviderError, ToolRegistry,
};
use futures::stream::{self, BoxStream, StreamExt};
use tempfile::TempDir;

#[derive(Clone)]
struct ScriptedProvider {
    turns: Arc<Mutex<Vec<Vec<ModelEvent>>>>,
}

impl ScriptedProvider {
    fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
        }
    }
}

#[async_trait]
impl ModelProvider for ScriptedProvider {
    async fn stream_turn(
        &self,
        _req: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        let mut turns = self.turns.lock().unwrap();
        let events = if turns.is_empty() {
            vec![ModelEvent::MessageStop {
                stop_reason: Some("end_turn".into()),
                usage: None,
            }]
        } else {
            turns.remove(0)
        };
        Ok(Box::pin(stream::iter(events.into_iter().map(Ok))))
    }
}

fn write_skill(root: &std::path::Path, name: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

#[tokio::test]
// SystemInit carries Skill summaries (not full bodies) when Skills are
// registered.
async fn system_init_includes_skill_summaries() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    write_skill(
        &skills_dir,
        "release-pr",
        "---\ndescription: Create a release PR summary\n---\nskill body do not leak\n",
    );
    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: skills_dir,
    }];
    let registry = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();

    let provider = Arc::new(ScriptedProvider::new(vec![vec![ModelEvent::MessageStop {
        stop_reason: Some("end_turn".into()),
        usage: None,
    }]])) as Arc<dyn ModelProvider>;

    let options = AgentOptions {
        cwd: tmp.path().to_path_buf(),
        model: "test".into(),
        allowed_tools: vec!["Skill".into()],
        permission_mode: PermissionMode::DontAsk,
        skills: registry,
        ..AgentOptions::default()
    };

    let stream = query("hello", options, provider, ToolRegistry::new())
        .await
        .unwrap();
    let events: Vec<AgentEvent> = stream.collect().await;

    let init = events
        .iter()
        .find_map(|e| match e {
            AgentEvent::SystemInit(i) => Some(i),
            _ => None,
        })
        .expect("system init should fire");
    assert_eq!(init.skills.len(), 1);
    assert_eq!(init.skills[0].name, "release-pr");
    // The full body must not be in the summary.
    let json = serde_json::to_string(&init.skills).unwrap();
    assert!(!json.contains("do not leak"));
}

#[tokio::test]
// The Skill registry-loaded event fires when Skills are registered AND
// the parent has been granted the Skill tool.
async fn skill_registry_loaded_event_fires() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    write_skill(&skills_dir, "alpha", "---\ndescription: d\n---\nbody\n");
    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: skills_dir,
    }];
    let registry = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();

    let provider = Arc::new(ScriptedProvider::new(vec![vec![ModelEvent::MessageStop {
        stop_reason: Some("end_turn".into()),
        usage: None,
    }]])) as Arc<dyn ModelProvider>;
    let options = AgentOptions {
        cwd: tmp.path().to_path_buf(),
        model: "test".into(),
        allowed_tools: vec!["Skill".into()],
        permission_mode: PermissionMode::DontAsk,
        skills: registry,
        ..AgentOptions::default()
    };
    let events: Vec<AgentEvent> = query("hi", options, provider, ToolRegistry::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::SkillRegistryLoaded(_))),
        "missing SkillRegistryLoaded"
    );
}

#[tokio::test]
// AgentClient::compact summarizes older turns and emits a CompactBoundary.
async fn agent_client_compact_emits_boundary() {
    use crom_core::AgentClient;
    let tmp = TempDir::new().unwrap();
    let provider = Arc::new(ScriptedProvider::new(vec![])) as Arc<dyn ModelProvider>;
    let mut client = AgentClient::builder()
        .cwd(tmp.path().to_path_buf())
        .model("test")
        .provider(provider)
        .initial_history(vec![
            crom_core::Message::user_text("turn 1"),
            crom_core::Message::user_text("turn 2"),
            crom_core::Message::user_text("turn 3"),
            crom_core::Message::user_text("turn 4"),
            crom_core::Message::user_text("turn 5"),
            crom_core::Message::user_text("turn 6"),
            crom_core::Message::user_text("turn 7"),
            crom_core::Message::user_text("turn 8"),
        ])
        .build()
        .await
        .unwrap();

    let options = crom_core::CompactionOptions {
        preserve_recent_turns: 2,
        ..crom_core::CompactionOptions::default()
    };
    let boundary = client.compact(options).await.unwrap();
    assert!(boundary.pre_messages > boundary.post_messages);
    assert_eq!(boundary.preserved_recent_turns, 2);
    assert!(matches!(
        boundary.trigger,
        crom_core::CompactionTrigger::ManualSlashCommand
    ));
    assert!(boundary.summary_preview.contains("compacted"));
}

#[test]
// The slash command parser + dispatcher resolves a Skill name and a
// custom command from a project layout.
fn slash_dispatch_resolves_skill_and_custom_command() {
    let tmp = TempDir::new().unwrap();
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    write_skill(
        &skills_dir,
        "release-pr",
        "---\ndescription: rpr\n---\nbody\n",
    );
    let commands_dir = tmp.path().join("commands");
    std::fs::create_dir_all(&commands_dir).unwrap();
    std::fs::write(
        commands_dir.join("review-changes.md"),
        "---\ndescription: rc\n---\nReview $1\n",
    )
    .unwrap();

    let opts = crom_core::SkillDiscoveryOptions::default();
    let paths = vec![crom_core::SkillDiscoveryPath {
        source: crom_core::SkillSource::Project,
        path: skills_dir,
    }];
    let skills = crom_core::SkillRegistry::discover(&paths, &opts).unwrap();
    let (cmds, _) = crom_core::load_custom_commands(&commands_dir);
    let reg = crom_core::SlashCommandRegistry::new(true)
        .with_custom(cmds)
        .with_skills(&skills);

    let parsed = crom_core::parse_slash_command("/release-pr v0.4.0").unwrap();
    let d = reg.dispatch(parsed).unwrap();
    match d.handler {
        crom_core::SlashCommandHandler::Skill(n) => assert_eq!(n, "release-pr"),
        other => panic!("expected Skill handler, got {other:?}"),
    }

    let parsed = crom_core::parse_slash_command("/review-changes main").unwrap();
    let d = reg.dispatch(parsed).unwrap();
    assert!(matches!(
        d.handler,
        crom_core::SlashCommandHandler::CustomFile(_)
    ));

    let parsed = crom_core::parse_slash_command("/help").unwrap();
    let d = reg.dispatch(parsed).unwrap();
    assert!(matches!(
        d.handler,
        crom_core::SlashCommandHandler::BuiltIn(_)
    ));

    let parsed = crom_core::parse_slash_command("/no-such-thing").unwrap();
    let err = reg.dispatch(parsed).unwrap_err();
    assert!(matches!(err, crom_core::SlashCommandError::Unknown { .. }));
}

#[test]
// Custom-command bodies cannot widen permissions: project disallowed
// rules win even if the command's frontmatter would permit a tool.
// (The dispatcher returns the dispatch; enforcement happens at execute
// time in the agent loop. We assert structural integrity here.)
fn dispatch_preserves_command_metadata_for_policy_inspection() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().to_path_buf();
    std::fs::write(
        dir.join("review.md"),
        "---\ndescription: d\nallowed-tools:\n  - Read\n  - Bash(git diff *)\ndisallowed-tools:\n  - Edit\n---\nBody\n",
    )
    .unwrap();
    let (defs, errors) = crom_core::load_custom_commands(&dir);
    assert!(errors.is_empty());
    let def = defs.first().unwrap();
    assert!(def
        .allowed_tools
        .iter()
        .any(|r| matches!(r, crom_core::ToolRule::ExactTool(n) if n == "Read")));
    assert!(def
        .allowed_tools
        .iter()
        .any(|r| matches!(r, crom_core::ToolRule::BashCommandRegex(_))));
    assert!(def
        .disallowed_tools
        .iter()
        .any(|r| matches!(r, crom_core::ToolRule::ExactTool(n) if n == "Edit")));
}
