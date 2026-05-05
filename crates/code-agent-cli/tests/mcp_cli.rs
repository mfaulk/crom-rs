use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_code-agent"))
}

fn fake_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../code-agent-mcp/tests/fixtures/fake-mcp-server/Cargo.toml")
}

fn write_mcp_config(dir: &Path) {
    let agent = dir.join(".agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "fake": {
                    "type": "stdio",
                    "command": "cargo",
                    "args": [
                        "run",
                        "--quiet",
                        "--manifest-path",
                        fake_manifest().display().to_string()
                    ],
                    "startup_timeout_seconds": 5,
                    "request_timeout_seconds": 5
                }
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .current_dir(cwd)
        .env_remove("ANTHROPIC_API_KEY")
        .output()
        .expect("failed to spawn code-agent")
}

#[test]
// `code-agent mcp status` connects to the fake server and prints its status.
fn mcp_status_works() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let out = run(&["mcp", "status", "--cwd", "."], dir.path());
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("fake"));
    assert!(stdout.contains("Connected") || stdout.contains("connected"));
}

#[test]
// `code-agent mcp tools <server>` lists deterministic fake server tools.
fn mcp_tools_works() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let out = run(&["mcp", "tools", "fake", "--cwd", "."], dir.path());
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("list_issues"));
    assert!(stdout.contains("get_issue"));
}

#[test]
// `code-agent mcp call` invokes a fake tool directly without an API key.
fn mcp_call_works() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let out = run(
        &[
            "mcp",
            "call",
            "fake",
            "list_issues",
            "--args-json",
            "{}",
            "--yes",
            "--cwd",
            ".",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Parser trims too late"));
    assert!(stdout.contains("not covered by workspace checkpointing"));
}

#[test]
// Resource and prompt inspection commands work against the fake server.
fn mcp_resources_and_prompts_work() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let resources = run(&["mcp", "resources", "fake", "--cwd", "."], dir.path());
    assert!(resources.status.success());
    assert!(String::from_utf8_lossy(&resources.stdout).contains("fake://docs/parser"));

    let read = run(
        &[
            "mcp",
            "resource",
            "read",
            "fake",
            "fake://docs/parser",
            "--cwd",
            ".",
        ],
        dir.path(),
    );
    assert!(read.status.success());
    assert!(String::from_utf8_lossy(&read.stdout).contains("Parser docs stay offline"));

    let prompts = run(&["mcp", "prompts", "fake", "--cwd", "."], dir.path());
    assert!(prompts.status.success());
    assert!(String::from_utf8_lossy(&prompts.stdout).contains("summarize_issue"));
}
