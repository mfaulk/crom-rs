use std::sync::Arc;
use std::time::Duration;

use crom_core::{SessionId, Tool, ToolContext, ToolError};
use crom_tools::{BashTool, EditTool, GlobTool, GrepTool, ReadTool, ToolLimits, WriteTool};
use serde_json::json;
use tempfile::TempDir;

fn ctx_for(dir: &TempDir) -> ToolContext {
    ToolContext::new_minimal(dir.path().to_path_buf(), SessionId::new())
}

#[tokio::test]
// Read rejects path traversal escaping the workspace root.
async fn read_rejects_path_traversal() {
    let dir = TempDir::new().unwrap();
    let tool = ReadTool::new(Arc::new(ToolLimits::default()));
    let err = tool
        .execute(json!({"file_path": "../etc/passwd"}), ctx_for(&dir))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::PathEscape(_)), "got: {err:?}");
}

#[cfg(unix)]
#[tokio::test]
// Read rejects symlinks whose target is outside the workspace root.
async fn read_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;
    let outer = TempDir::new().unwrap();
    let secret = outer.path().join("secret.txt");
    std::fs::write(&secret, "shh").unwrap();

    let dir = TempDir::new().unwrap();
    symlink(&secret, dir.path().join("link.txt")).unwrap();

    let tool = ReadTool::new(Arc::new(ToolLimits::default()));
    let err = tool
        .execute(json!({"file_path": "link.txt"}), ctx_for(&dir))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::PathEscape(_)), "got: {err:?}");
}

#[tokio::test]
// Read enforces the configured maximum file size.
async fn read_enforces_max_size() {
    let dir = TempDir::new().unwrap();
    let big = "x".repeat(2048);
    std::fs::write(dir.path().join("big.txt"), &big).unwrap();
    let limits = ToolLimits {
        max_read_bytes: 1024,
        ..ToolLimits::default()
    };
    let tool = ReadTool::new(Arc::new(limits));
    let err = tool
        .execute(json!({"file_path": "big.txt"}), ctx_for(&dir))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::LimitExceeded(_)), "got: {err:?}");
}

#[tokio::test]
// Read returns the file contents for a simple in-workspace text file.
async fn read_succeeds_simple() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let tool = ReadTool::new(Arc::new(ToolLimits::default()));
    let res = tool
        .execute(json!({"file_path": "a.txt"}), ctx_for(&dir))
        .await
        .unwrap();
    assert_eq!(res.flatten_text(), "hello");
}

#[tokio::test]
// Glob respects the configured maximum result count and reports truncation.
async fn glob_respects_max_results() {
    let dir = TempDir::new().unwrap();
    for i in 0..10 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), "x").unwrap();
    }
    let limits = ToolLimits {
        max_glob_results: 3,
        ..ToolLimits::default()
    };
    let tool = GlobTool::new(Arc::new(limits));
    let res = tool
        .execute(json!({"pattern": "*.txt"}), ctx_for(&dir))
        .await
        .unwrap();
    assert!(res.metadata.get("truncated").unwrap().as_bool().unwrap());
}

#[tokio::test]
// Grep respects the configured maximum result count and reports truncation.
async fn grep_respects_max_results() {
    let dir = TempDir::new().unwrap();
    for i in 0..10 {
        std::fs::write(dir.path().join(format!("f{i}.txt")), "needle here\nother\n").unwrap();
    }
    let limits = ToolLimits {
        max_grep_results: 3,
        ..ToolLimits::default()
    };
    let tool = GrepTool::new(Arc::new(limits));
    let res = tool
        .execute(json!({"pattern": "needle"}), ctx_for(&dir))
        .await
        .unwrap();
    assert!(res.metadata.get("truncated").unwrap().as_bool().unwrap());
}

#[tokio::test]
// Edit fails when old_string is missing from the file.
async fn edit_fails_when_old_missing() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "abcdef").unwrap();
    let tool = EditTool::new(Arc::new(ToolLimits::default()));
    let err = tool
        .execute(
            json!({"file_path": "a.txt", "old_string": "ZZZ", "new_string": "."}),
            ctx_for(&dir),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidInput(_)), "got: {err:?}");
}

#[tokio::test]
// Edit fails when old_string occurs more than once in the file.
async fn edit_fails_when_old_ambiguous() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "abc abc abc").unwrap();
    let tool = EditTool::new(Arc::new(ToolLimits::default()));
    let err = tool
        .execute(
            json!({"file_path": "a.txt", "old_string": "abc", "new_string": "X"}),
            ctx_for(&dir),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ToolError::InvalidInput(s) if s.contains("occurs")),
        "got: {err:?}"
    );
}

#[tokio::test]
// Edit succeeds when old_string occurs exactly once and writes the new content.
async fn edit_succeeds_single_occurrence() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world").unwrap();
    let tool = EditTool::new(Arc::new(ToolLimits::default()));
    let res = tool
        .execute(
            json!({"file_path": "a.txt", "old_string": "world", "new_string": "rust"}),
            ctx_for(&dir),
        )
        .await
        .unwrap();
    assert!(!res.is_error);
    let updated = std::fs::read_to_string(dir.path().join("a.txt")).unwrap();
    assert_eq!(updated, "hello rust");
}

#[tokio::test]
// Edit caps a large diff summary in both content and metadata at max_output_bytes (plus marker).
async fn edit_caps_large_diff_summary() {
    let dir = TempDir::new().unwrap();
    let limits = ToolLimits {
        max_output_bytes: 1024,
        ..ToolLimits::default()
    };
    // Replace a large block with a different large block so every line changes,
    // producing a diff summary far larger than the cap (many +/- lines).
    let old_block: String = (0..20_000).map(|i| format!("old line {i}\n")).collect();
    let new_block: String = (0..20_000).map(|i| format!("new line {i}\n")).collect();
    let original = format!("header\n{old_block}footer\n");
    std::fs::write(dir.path().join("big.txt"), &original).unwrap();
    let tool = EditTool::new(Arc::new(limits));
    let res = tool
        .execute(
            json!({"file_path": "big.txt", "old_string": old_block, "new_string": new_block}),
            ctx_for(&dir),
        )
        .await
        .unwrap();
    assert!(!res.is_error);
    let cap = 1024;
    let marker_slack = 128;
    let diff = res.metadata.get("diff").unwrap().as_str().unwrap();
    assert!(
        diff.len() <= cap + marker_slack,
        "metadata diff len {} exceeds cap",
        diff.len()
    );
    let crom_core::ToolContent::Text(text) = &res.content[0];
    assert!(
        text.len() <= cap + marker_slack + 64,
        "content text len {} exceeds cap",
        text.len()
    );
}

#[tokio::test]
// Bash kills a long-running command after the configured timeout.
async fn bash_timeout_kills_command() {
    let dir = TempDir::new().unwrap();
    let limits = ToolLimits {
        bash_timeout: Duration::from_millis(200),
        ..ToolLimits::default()
    };
    let tool = BashTool::new(Arc::new(limits));
    let res = tool
        .execute(json!({"command": "sleep 5"}), ctx_for(&dir))
        .await
        .unwrap();
    assert!(res.is_error);
    let timed_out = res.metadata.get("timed_out").unwrap().as_bool().unwrap();
    assert!(timed_out);
}

#[tokio::test]
// Bash treats a non-zero exit as ordinary output, not a tool failure: a
// failing `cargo test` (or any command exiting non-zero) must surface its
// exit code in metadata and text without flipping `is_error`, which the
// API uses to signal that the tool itself failed.
async fn bash_nonzero_exit_is_not_tool_error() {
    let dir = TempDir::new().unwrap();
    let tool = BashTool::new(Arc::new(ToolLimits::default()));
    let res = tool
        .execute(json!({"command": "exit 7"}), ctx_for(&dir))
        .await
        .unwrap();
    assert!(
        !res.is_error,
        "non-zero exit should not be reported as a tool error"
    );
    assert_eq!(
        res.metadata.get("exit_code").and_then(|v| v.as_i64()),
        Some(7)
    );
    assert!(!res.metadata.get("timed_out").unwrap().as_bool().unwrap());
    assert!(res.flatten_text().contains("[exit: 7]"));
}

#[tokio::test]
// Bash truncates output exceeding the configured cap.
async fn bash_output_capped() {
    let dir = TempDir::new().unwrap();
    let limits = ToolLimits {
        max_output_bytes: 64,
        ..ToolLimits::default()
    };
    let tool = BashTool::new(Arc::new(limits));
    let res = tool
        .execute(
            json!({"command": "yes hello | head -c 4096"}),
            ctx_for(&dir),
        )
        .await
        .unwrap();
    let text = res.flatten_text();
    assert!(text.contains("truncated"), "text was: {text}");
}

#[tokio::test]
// Write creates a new file inside the workspace and round-trips its content.
async fn write_creates_file() {
    let dir = TempDir::new().unwrap();
    let tool = WriteTool::new(Arc::new(ToolLimits::default()));
    let res = tool
        .execute(
            json!({"file_path": "sub/new.txt", "content": "hi"}),
            ctx_for(&dir),
        )
        .await
        .unwrap();
    assert!(!res.is_error);
    let text = std::fs::read_to_string(dir.path().join("sub/new.txt")).unwrap();
    assert_eq!(text, "hi");
}

#[cfg(unix)]
#[tokio::test]
// Write through a dangling symlink whose target lies outside the workspace
// is rejected as a sandbox escape, even though the symlink path itself does
// not exist on disk.
async fn write_rejects_dangling_symlink_escape() {
    use std::os::unix::fs::symlink;
    let outer = TempDir::new().unwrap();
    let target = outer.path().join("escape.txt");
    // Target intentionally absent so the symlink dangles; this is the case
    // that previously slipped past the sandbox because Path::exists()
    // follows symlinks and reports `false` for a dangling one.
    let dir = TempDir::new().unwrap();
    symlink(&target, dir.path().join("link.txt")).unwrap();

    let tool = WriteTool::new(Arc::new(ToolLimits::default()));
    let err = tool
        .execute(
            json!({"file_path": "link.txt", "content": "x"}),
            ctx_for(&dir),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::PathEscape(_)), "got: {err:?}");
    // The escape file must not have been created.
    assert!(!target.exists());
}

#[tokio::test]
// Bash caller-supplied timeout_secs is capped to BashTool's configured maximum.
async fn bash_timeout_secs_is_capped() {
    let dir = TempDir::new().unwrap();
    let limits = ToolLimits {
        bash_timeout: Duration::from_millis(200),
        ..ToolLimits::default()
    };
    let tool = BashTool::new(Arc::new(limits));
    // Caller asks for 60 seconds; the configured ceiling is 200ms, so the
    // command must be killed well before its own 5s sleep finishes.
    let started = std::time::Instant::now();
    let res = tool
        .execute(
            json!({"command": "sleep 5", "timeout_secs": 60}),
            ctx_for(&dir),
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(res.is_error);
    assert!(res.metadata.get("timed_out").unwrap().as_bool().unwrap());
    assert!(
        elapsed < Duration::from_secs(2),
        "expected cap to kick in quickly, took {elapsed:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
// Bash kills the entire process group on timeout, so grandchild processes
// the script forked are not left running as orphans.
async fn bash_timeout_reaps_grandchildren() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("grandchild.pid");
    let limits = ToolLimits {
        bash_timeout: Duration::from_millis(200),
        ..ToolLimits::default()
    };
    let tool = BashTool::new(Arc::new(limits));
    // Spawn a backgrounded `sleep` from inside the bash script and record
    // its pid. After the timeout fires we'll check the recorded pid is gone.
    let cmd = format!("sleep 30 & echo $! > {} ; wait", marker.display());
    let _ = tool
        .execute(json!({"command": cmd}), ctx_for(&dir))
        .await
        .unwrap();
    // The kill signal propagates asynchronously to the process group. Rather
    // than sleeping a fixed interval and hoping it landed, poll until the
    // recorded grandchild pid is gone (`kill(pid, 0)` returns ESRCH), bounded
    // by an overall deadline so a real leak still fails loudly.
    let pid_str = std::fs::read_to_string(&marker).unwrap_or_default();
    let pid: i32 = pid_str.trim().parse().expect("grandchild pid recorded");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        // kill(pid, 0) returns ESRCH (==> != 0) once the process is gone.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "grandchild pid {pid} still alive after Bash timeout"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
// End-to-end regression for #142: a command that interpolates a real
// secret value (no env-var-name needle in the surrounding text) must
// have the value scrubbed from the `command_redacted` metadata field
// before it is persisted to audit / telemetry surfaces.
//
// This exercises the production code path
// `BashTool::execute -> redact -> metadata["command_redacted"]` rather
// than the private `redact` helper directly, so a future regression
// where `execute` stops calling `redact` would be caught here.
async fn bash_metadata_command_redacted_scrubs_no_needle_secret() {
    let dir = TempDir::new().unwrap();
    let tool = BashTool::new(Arc::new(ToolLimits::default()));
    // `echo` is a builtin; the command runs cleanly. The Bearer token
    // shape is the canonical #142 example: it contains none of the
    // env-var-name needles (`KEY`, `SECRET`, `TOKEN`, `PASSWORD`,
    // `PASSWD`, `ACCESS`, `PRIVATE`) anywhere in the surrounding text,
    // so only the token-shape redactor in `redact_secrets_in_str` can
    // save us.
    let secret = "sk-ant-api03-AAAAAAAAAAAAAAAAAAAA";
    let cmd = format!("echo {} >/dev/null", secret);
    let res = tool
        .execute(json!({"command": cmd}), ctx_for(&dir))
        .await
        .unwrap();
    let redacted = res
        .metadata
        .get("command_redacted")
        .and_then(|v| v.as_str())
        .expect("command_redacted metadata must be present");
    assert!(
        !redacted.contains(secret),
        "raw secret leaked through command_redacted: {redacted}"
    );
    assert!(
        redacted.contains("[REDACTED]"),
        "expected [REDACTED] marker in: {redacted}"
    );
}
