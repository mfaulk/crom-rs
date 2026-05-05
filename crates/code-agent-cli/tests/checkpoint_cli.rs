//! Smoke tests for the M3 checkpoint subcommands. These run against the
//! built `code-agent` binary and exercise checkpoint create/list/diff and
//! rewind in a temp workspace. They do NOT need an Anthropic API key.

use std::process::Command;

use tempfile::TempDir;

fn bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for tests in the same crate.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_code-agent"))
}

fn run(args: &[&str], cwd: &std::path::Path) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("failed to spawn code-agent")
}

#[test]
// `checkpoint create` produces a checkpoint id and `checkpoints list`
// reports it.
fn checkpoint_create_then_list() {
    let dir = TempDir::new().unwrap();
    // Need a session to attach a checkpoint to. The simplest way is to
    // touch a session jsonl manually.
    let sessions = dir.path().join(".agent").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("0123456789abcdef.jsonl"), b"").unwrap();

    let out = run(
        &[
            "checkpoint",
            "test description",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--session",
            "0123456789abcdef",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "checkpoint create failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[checkpoint] created ckpt_"),
        "unexpected stdout: {stdout}"
    );

    let out = run(
        &[
            "checkpoints",
            "list",
            "--cwd",
            dir.path().to_str().unwrap(),
            "--session",
            "0123456789abcdef",
        ],
        dir.path(),
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ckpt_") && stdout.contains("test description"),
        "list missing checkpoint: {stdout}"
    );
}

#[test]
// `rewind --dry-run` reports actions without modifying files; the
// confirmation prompt is skipped because --dry-run implies preview-only.
fn rewind_dry_run_does_not_modify() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join(".agent").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("aaaa1111bbbb2222.jsonl"), b"").unwrap();

    // Create a real file we'll snapshot then mutate.
    std::fs::write(dir.path().join("a.txt"), b"orig").unwrap();

    // Create a checkpoint and snapshot a.txt manually using the library.
    use code_agent_checkpoint::{
        CaptureBeforeToolRequest, CheckpointStore, CreateCheckpointRequest, LocalCheckpointStore,
        WorkspaceRelativePath,
    };
    use code_agent_core::{RunId, SessionId};
    let sid = SessionId::from("aaaa1111bbbb2222".to_string());
    let store = LocalCheckpointStore::new(dir.path().to_path_buf());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ckpt_id = runtime.block_on(async {
        store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: dir.path().to_path_buf(),
                description: None,
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap()
    });
    runtime.block_on(async {
        store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: ckpt_id.clone(),
                session_id: sid,
                run_id: RunId::new(),
                cwd: dir.path().to_path_buf(),
                tool_name: "Edit".into(),
                tool_input: serde_json::json!({"file_path": "a.txt"}),
                paths: vec![WorkspaceRelativePath("a.txt".into())],
            })
            .await
            .unwrap();
    });

    // Mutate the file, then dry-run rewind.
    std::fs::write(dir.path().join("a.txt"), b"changed").unwrap();
    let out = run(
        &[
            "rewind",
            ckpt_id.as_str(),
            "--cwd",
            dir.path().to_str().unwrap(),
            "--dry-run",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "rewind failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would restore: a.txt"), "stdout={stdout}");
    assert!(
        stdout.contains("conversation history will not be rewound"),
        "stdout missing conversation note: {stdout}"
    );
    // File on disk must still be the mutated content.
    let s = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
    assert_eq!(s, "changed");
}

#[test]
// `rewind --yes` actually restores files and notes that conversation
// history was not rewound.
fn rewind_yes_restores_files() {
    let dir = TempDir::new().unwrap();
    let sessions = dir.path().join(".agent").join("sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::write(sessions.join("ccccddddeeeeffff.jsonl"), b"").unwrap();

    std::fs::write(dir.path().join("p.txt"), b"original\n").unwrap();

    use code_agent_checkpoint::{
        CaptureBeforeToolRequest, CheckpointStore, CreateCheckpointRequest, LocalCheckpointStore,
        WorkspaceRelativePath,
    };
    use code_agent_core::{RunId, SessionId};
    let sid = SessionId::from("ccccddddeeeeffff".to_string());
    let store = LocalCheckpointStore::new(dir.path().to_path_buf());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let ckpt_id = runtime.block_on(async {
        let id = store
            .create_checkpoint(CreateCheckpointRequest {
                session_id: sid.clone(),
                run_id: RunId::new(),
                cwd: dir.path().to_path_buf(),
                description: Some("pre-edit".into()),
                user_message_id: None,
                git_state: None,
            })
            .await
            .unwrap();
        store
            .capture_before_tool(CaptureBeforeToolRequest {
                checkpoint_id: id.clone(),
                session_id: sid,
                run_id: RunId::new(),
                cwd: dir.path().to_path_buf(),
                tool_name: "Edit".into(),
                tool_input: serde_json::json!({"file_path": "p.txt"}),
                paths: vec![WorkspaceRelativePath("p.txt".into())],
            })
            .await
            .unwrap();
        id
    });

    std::fs::write(dir.path().join("p.txt"), b"mutated\n").unwrap();

    let out = run(
        &[
            "rewind",
            ckpt_id.as_str(),
            "--cwd",
            dir.path().to_str().unwrap(),
            "--yes",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "rewind failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[rewind] restored p.txt"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("conversation history was not rewound"));
    let s = std::fs::read_to_string(dir.path().join("p.txt")).unwrap();
    assert_eq!(s, "original\n");
}
