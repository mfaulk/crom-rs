use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Once;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_code-agent"))
}

fn fake_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../code-agent-mcp/tests/fixtures/fake-mcp-server/Cargo.toml")
}

static BUILD_FAKE_SERVER: Once = Once::new();

/// Pre-builds the fake MCP server fixture exactly once per test binary.
///
/// The fixture is its own (excluded) workspace, so the `cargo run` the server
/// config uses would otherwise trigger a build on first spawn. Under
/// full-suite parallelism many tests spawn it simultaneously, and racing
/// `cargo build` invocations contend on the cargo lock and on CPU/memory —
/// the spawned server child was intermittently SIGKILLed before it became
/// ready (#35). Building once up front turns each later `cargo run` into a
/// near-instant no-op spawn, removing that contention.
fn ensure_fake_server_built() {
    BUILD_FAKE_SERVER.call_once(|| {
        let status = Command::new("cargo")
            .args([
                "build",
                "--quiet",
                "--manifest-path",
                &fake_manifest().display().to_string(),
            ])
            .status()
            .expect("failed to spawn cargo build for fake-mcp-server fixture");
        assert!(
            status.success(),
            "pre-building the fake-mcp-server fixture failed"
        );
    });
}

/// Runs an MCP CLI command, retrying a bounded number of times if the spawned
/// MCP server child fails to come up. A genuine spawn failure under resource
/// pressure is diagnosed (captured stderr is surfaced) rather than surfacing
/// as a bare assertion failure on the first attempt.
fn run_mcp(args: &[&str], cwd: &Path) -> std::process::Output {
    ensure_fake_server_built();
    let mut last: Option<std::process::Output> = None;
    for attempt in 0..3 {
        let out = run(args, cwd);
        if out.status.success() {
            return out;
        }
        last = Some(out);
        // Brief backoff before re-spawning under transient load.
        std::thread::sleep(std::time::Duration::from_millis(100 * (attempt + 1)));
    }
    let out = last.expect("at least one attempt ran");
    panic!(
        "MCP CLI command {args:?} failed after retries; last stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    let out = run_mcp(&["mcp", "status", "--cwd", "."], dir.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("fake"));
    assert!(stdout.contains("Connected") || stdout.contains("connected"));
}

#[test]
// `code-agent mcp tools <server>` lists deterministic fake server tools.
fn mcp_tools_works() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let out = run_mcp(&["mcp", "tools", "fake", "--cwd", "."], dir.path());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("list_issues"));
    assert!(stdout.contains("get_issue"));
}

#[test]
// `code-agent mcp call` invokes a fake tool directly without an API key.
fn mcp_call_works() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let out = run_mcp(
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
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Parser trims too late"));
    assert!(stdout.contains("not covered by workspace checkpointing"));
}

#[test]
// Resource and prompt inspection commands work against the fake server.
fn mcp_resources_and_prompts_work() {
    let dir = TempDir::new().unwrap();
    write_mcp_config(dir.path());
    let resources = run_mcp(&["mcp", "resources", "fake", "--cwd", "."], dir.path());
    assert!(String::from_utf8_lossy(&resources.stdout).contains("fake://docs/parser"));

    let read = run_mcp(
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
    assert!(String::from_utf8_lossy(&read.stdout).contains("Parser docs stay offline"));

    let prompts = run_mcp(&["mcp", "prompts", "fake", "--cwd", "."], dir.path());
    assert!(String::from_utf8_lossy(&prompts.stdout).contains("summarize_issue"));
}
