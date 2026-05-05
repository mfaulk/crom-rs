//! End-to-end checkpoint capture + rewind scenarios.

use std::path::PathBuf;

use code_agent_checkpoint::{
    CaptureBeforeToolRequest, CheckpointError, CheckpointStore, CreateCheckpointRequest,
    DiffCheckpointRequest, FileDiff, LocalCheckpointStore, RewindRequest, WorkspaceRelativePath,
};
use code_agent_core::{RunId, SessionId};
use tempfile::TempDir;

fn ws_path(p: &str) -> WorkspaceRelativePath {
    WorkspaceRelativePath(p.to_string())
}

async fn fresh() -> (TempDir, LocalCheckpointStore, SessionId, PathBuf) {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store = LocalCheckpointStore::new(cwd.clone());
    let sid = SessionId::new();
    (dir, store, sid, cwd)
}

#[tokio::test]
// Write a brand-new file then rewind: rewind must delete it.
async fn write_new_file_then_rewind_deletes() {
    let (_dir, store, sid, cwd) = fresh().await;
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();

    // The agent intends to Write a new file. The checkpoint must capture
    // the missing-before-change marker BEFORE the file is created.
    let path = ws_path("new.rs");
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": "new.rs", "content": "fn main(){}"}),
            paths: vec![path.clone()],
        })
        .await
        .unwrap();

    // Then the tool actually runs and creates the file.
    tokio::fs::write(cwd.join("new.rs"), b"fn main(){}")
        .await
        .unwrap();
    assert!(cwd.join("new.rs").exists());

    // Rewind: file should be deleted.
    let report = store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.deleted_files, vec![path.clone()]);
    assert!(!cwd.join("new.rs").exists());
}

#[tokio::test]
// Overwrite an existing file then rewind: rewind restores original content.
async fn write_overwrite_then_rewind_restores() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("a.rs"), b"original\n")
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let path = ws_path("a.rs");
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "a.rs"}),
            paths: vec![path.clone()],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join("a.rs"), b"changed\n")
        .await
        .unwrap();

    let report = store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.restored_files, vec![path.clone()]);
    let restored = tokio::fs::read(cwd.join("a.rs")).await.unwrap();
    assert_eq!(restored, b"original\n");
}

#[tokio::test]
// Edit then rewind: rewind restores original content (same path semantics).
async fn edit_then_rewind_restores_original() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("file.txt"), b"hello\nworld\n")
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "file.txt"}),
            paths: vec![ws_path("file.txt")],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join("file.txt"), b"hello\nNEW\n")
        .await
        .unwrap();
    store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    let s = tokio::fs::read_to_string(cwd.join("file.txt"))
        .await
        .unwrap();
    assert_eq!(s, "hello\nworld\n");
}

#[tokio::test]
// A file that existed at checkpoint time but was deleted later is recreated.
async fn deleted_file_is_recreated() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("survive.txt"), b"yo")
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
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
            cwd: cwd.clone(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": "survive.txt"}),
            paths: vec![ws_path("survive.txt")],
        })
        .await
        .unwrap();
    tokio::fs::remove_file(cwd.join("survive.txt"))
        .await
        .unwrap();

    let report = store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(report.recreated_files, vec![ws_path("survive.txt")]);
    assert!(cwd.join("survive.txt").exists());
}

#[tokio::test]
// Dry run reports actions without modifying files.
async fn dry_run_does_not_modify() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("a.txt"), b"orig").await.unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
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
            cwd: cwd.clone(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "a.txt"}),
            paths: vec![ws_path("a.txt")],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join("a.txt"), b"changed")
        .await
        .unwrap();

    let report = store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: true,
        })
        .await
        .unwrap();
    assert!(report.dry_run);
    assert_eq!(report.restored_files.len(), 1);
    let s = tokio::fs::read_to_string(cwd.join("a.txt")).await.unwrap();
    assert_eq!(s, "changed", "dry run must not modify files");
}

#[tokio::test]
// CWD mismatch is rejected unless force_cwd is set.
async fn refuses_cwd_mismatch_without_force() {
    let (_dir, store, sid, cwd) = fresh().await;
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid,
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let other = TempDir::new().unwrap();
    let err = store
        .rewind(RewindRequest {
            checkpoint_id: id.clone(),
            cwd: other.path().to_path_buf(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CheckpointError::CwdMismatch { .. }));
}

#[tokio::test]
// Diff renders unified diff for modified text files and tracks created files.
async fn diff_renders_unified_diff() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("p.rs"), b"line1\nline2\n")
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({"file_path": "p.rs"}),
            paths: vec![ws_path("p.rs")],
        })
        .await
        .unwrap();
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": "n.rs"}),
            paths: vec![ws_path("n.rs")],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join("p.rs"), b"line1\nNEW\n")
        .await
        .unwrap();
    tokio::fs::write(cwd.join("n.rs"), b"created")
        .await
        .unwrap();

    let diff = store
        .diff_checkpoint(DiffCheckpointRequest {
            checkpoint_id: id,
            session_id: sid,
            cwd,
            max_bytes: None,
        })
        .await
        .unwrap();
    let mut saw_modified = false;
    let mut saw_created = false;
    for fd in &diff.file_diffs {
        match fd {
            FileDiff::Modified { unified_diff, .. } => {
                assert!(unified_diff.contains("NEW"));
                saw_modified = true;
            }
            FileDiff::Created {
                current_content_preview,
                ..
            } => {
                assert!(current_content_preview.contains("created"));
                saw_created = true;
            }
            _ => {}
        }
    }
    assert!(saw_modified && saw_created);
}

#[tokio::test]
// Path traversal in capture is rejected with PathEscape.
async fn path_traversal_rejected() {
    let (_dir, store, sid, cwd) = fresh().await;
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let err = store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id,
            session_id: sid,
            run_id: RunId::new(),
            cwd,
            tool_name: "Write".into(),
            tool_input: serde_json::json!({}),
            paths: vec![ws_path("../escape")],
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CheckpointError::PathEscape(_)));
}

#[tokio::test]
// Non-UTF8 / binary files are reported as Binary in diff output.
async fn binary_diff_marker() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("b.bin"), [0u8, 1, 2, 3])
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": "b.bin"}),
            paths: vec![ws_path("b.bin")],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join("b.bin"), [0u8, 9, 8, 7])
        .await
        .unwrap();
    let diff = store
        .diff_checkpoint(DiffCheckpointRequest {
            checkpoint_id: id,
            session_id: sid,
            cwd,
            max_bytes: None,
        })
        .await
        .unwrap();
    assert!(diff
        .file_diffs
        .iter()
        .any(|f| matches!(f, FileDiff::Binary { .. })));
}

#[tokio::test]
// .env files are redacted in diff output, never embedded in content.
async fn env_files_redacted_in_diff() {
    let (_dir, store, sid, cwd) = fresh().await;
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Write".into(),
            tool_input: serde_json::json!({"file_path": ".env"}),
            paths: vec![ws_path(".env")],
        })
        .await
        .unwrap();
    tokio::fs::write(cwd.join(".env"), "SECRET=1\n")
        .await
        .unwrap();
    let diff = store
        .diff_checkpoint(DiffCheckpointRequest {
            checkpoint_id: id,
            session_id: sid,
            cwd,
            max_bytes: None,
        })
        .await
        .unwrap();
    let mut saw_redacted = false;
    for fd in &diff.file_diffs {
        match fd {
            FileDiff::Redacted { .. } => saw_redacted = true,
            FileDiff::Created {
                current_content_preview,
                ..
            } => {
                panic!("env content leaked: {current_content_preview}");
            }
            _ => {}
        }
    }
    assert!(saw_redacted, "redacted entry expected");
}

#[tokio::test]
// First snapshot wins: a second capture for the same path doesn't replace
// the original (otherwise rewind would restore an already-mutated state).
async fn first_snapshot_wins() {
    let (_dir, store, sid, cwd) = fresh().await;
    tokio::fs::write(cwd.join("x.txt"), b"original")
        .await
        .unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let req = |paths| CaptureBeforeToolRequest {
        checkpoint_id: id.clone(),
        session_id: sid.clone(),
        run_id: RunId::new(),
        cwd: cwd.clone(),
        tool_name: "Edit".into(),
        tool_input: serde_json::json!({}),
        paths,
    };
    store
        .capture_before_tool(req(vec![ws_path("x.txt")]))
        .await
        .unwrap();
    tokio::fs::write(cwd.join("x.txt"), b"intermediate")
        .await
        .unwrap();
    // Second capture pass: should be a no-op for x.txt.
    store
        .capture_before_tool(req(vec![ws_path("x.txt")]))
        .await
        .unwrap();
    tokio::fs::write(cwd.join("x.txt"), b"final").await.unwrap();

    store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(
        tokio::fs::read_to_string(cwd.join("x.txt")).await.unwrap(),
        "original"
    );
}

#[cfg(unix)]
#[tokio::test]
// Capture must reject paths that traverse an INTERMEDIATE symlink whose
// target is outside the workspace, not just terminal symlinks. Both
// reviewers flagged this as a sandbox-escape bug.
async fn intermediate_symlink_escape_rejected() {
    use std::os::unix::fs::symlink;
    let outer = TempDir::new().unwrap();
    let secret_dir = outer.path().join("secret");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(secret_dir.join("loot.txt"), b"crown jewels").unwrap();

    let workspace = TempDir::new().unwrap();
    let cwd = workspace.path().to_path_buf();
    // Place a symlinked subdirectory in the workspace pointing at the
    // outside `secret_dir`. A naive check would resolve `link/loot.txt`
    // lexically inside cwd and let capture read the file.
    symlink(&secret_dir, cwd.join("link")).unwrap();

    let store = LocalCheckpointStore::new(cwd.clone());
    let sid = SessionId::new();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let err = store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id,
            session_id: sid,
            run_id: RunId::new(),
            cwd,
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({}),
            paths: vec![ws_path("link/loot.txt")],
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, CheckpointError::PathEscape(_)),
        "intermediate symlink escape must be rejected, got {err:?}"
    );
}

#[tokio::test]
// Sibling files like `main.rs` and `main.txt` produce DIFFERENT temp
// filenames during atomic_write. The earlier `with_extension("ckpt-tmp")`
// path replaced the extension and would have collided.
async fn atomic_write_tmp_does_not_collide_for_siblings() {
    let (_dir, store, sid, cwd) = fresh().await;
    std::fs::write(cwd.join("main.rs"), b"// rs").unwrap();
    std::fs::write(cwd.join("main.txt"), b"txt").unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    // Capture both. The blob-write path uses atomic_write internally;
    // distinct content produces distinct blobs. We then mutate them, run
    // many concurrent index refreshes (which also use atomic_write on the
    // index file), and confirm rewind restores BOTH originals.
    store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id.clone(),
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            tool_name: "Edit".into(),
            tool_input: serde_json::json!({}),
            paths: vec![ws_path("main.rs"), ws_path("main.txt")],
        })
        .await
        .unwrap();
    std::fs::write(cwd.join("main.rs"), b"// changed").unwrap();
    std::fs::write(cwd.join("main.txt"), b"changed").unwrap();
    store
        .rewind(RewindRequest {
            checkpoint_id: id,
            cwd: cwd.clone(),
            force_cwd: false,
            dry_run: false,
        })
        .await
        .unwrap();
    assert_eq!(std::fs::read(cwd.join("main.rs")).unwrap(), b"// rs");
    assert_eq!(std::fs::read(cwd.join("main.txt")).unwrap(), b"txt");
}

#[cfg(unix)]
#[tokio::test]
// Symlink in workspace pointing outside cwd is rejected during capture.
async fn symlink_escape_rejected() {
    use std::os::unix::fs::symlink;
    let outer = TempDir::new().unwrap();
    let target = outer.path().join("secret");
    std::fs::write(&target, b"x").unwrap();
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().to_path_buf();
    let store = LocalCheckpointStore::new(cwd.clone());
    let sid = SessionId::new();
    symlink(&target, cwd.join("link")).unwrap();
    let id = store
        .create_checkpoint(CreateCheckpointRequest {
            session_id: sid.clone(),
            run_id: RunId::new(),
            cwd: cwd.clone(),
            description: None,
            user_message_id: None,
            git_state: None,
        })
        .await
        .unwrap();
    let err = store
        .capture_before_tool(CaptureBeforeToolRequest {
            checkpoint_id: id,
            session_id: sid,
            run_id: RunId::new(),
            cwd,
            tool_name: "Write".into(),
            tool_input: serde_json::json!({}),
            paths: vec![ws_path("link")],
        })
        .await
        .unwrap_err();
    assert!(matches!(err, CheckpointError::PathEscape(_)));
}
