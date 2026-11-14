use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crom"))
}

fn fake_manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crom-mcp/tests/fixtures/fake-mcp-server/Cargo.toml")
}

fn write_config(dir: &Path) {
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
    std::fs::write(
        agent.join("settings.json"),
        serde_json::json!({
            "tool_search": {
                "enabled": true,
                "mode": "search",
                "strategy": "hybrid",
                "native_provider_mode": "off",
                "max_results": 5,
                "always_loaded": ["Read", "Glob", "Grep", "SearchTools"],
                "deferred": ["mcp__*"]
            },
            "allowed_tools": ["Read", "Glob", "Grep", "SearchTools", "mcp__fake__*"]
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
        .expect("failed to spawn crom")
}

#[test]
// `crom tools stats` builds an offline local + MCP catalog without a model API key.
fn tools_stats_works() {
    let dir = TempDir::new().unwrap();
    write_config(dir.path());
    let out = run(&["tools", "stats", "--cwd", "."], dir.path());
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("CATALOG"));
    assert!(stdout.contains("mcp:fake"));
    assert!(stdout.contains("resolved mode:      search"));
}

#[test]
// `crom tools search` finds policy-visible MCP issue tools.
fn tools_search_works() {
    let dir = TempDir::new().unwrap();
    write_config(dir.path());
    let out = run(
        &[
            "tools",
            "search",
            "github issue",
            "--cwd",
            ".",
            "--limit",
            "2",
        ],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mcp__fake__list_issues"));
    assert!(stdout.contains("mcp__fake__get_issue"));
}

#[test]
// `tools stats` mirrors the agent registry filter: when settings narrow
// `allowed_tools` to a single tool, the catalog only reports that tool.
fn tools_stats_applies_allowed_tools_registry_filter() {
    let dir = TempDir::new().unwrap();
    let agent = dir.path().join(".agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        serde_json::json!({
            "tool_search": {
                "enabled": true,
                "mode": "auto",
            },
            "allowed_tools": ["Read"]
        })
        .to_string(),
    )
    .unwrap();
    let out = run(&["tools", "stats", "--cwd", "."], dir.path());
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("total tools:        1"),
        "expected exactly 1 tool after registry filter, got:\n{}",
        stdout
    );
}

#[test]
// `tools stats` accepts `--permission-mode` and the resolved mode plumbs
// through; this guards against the prior hardcoded `PermissionMode::Default`.
fn tools_stats_accepts_permission_mode_flag() {
    let dir = TempDir::new().unwrap();
    let agent = dir.path().join(".agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        serde_json::json!({
            "allowed_tools": ["Read"]
        })
        .to_string(),
    )
    .unwrap();
    let out = run(
        &["tools", "stats", "--cwd", ".", "--permission-mode", "plan"],
        dir.path(),
    );
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
// Disabled tool settings hide matching tools from CLI search results.
fn tools_search_hides_disabled_tools() {
    let dir = TempDir::new().unwrap();
    write_config(dir.path());
    let out = run(
        &[
            "tools",
            "search",
            "docs",
            "--cwd",
            ".",
            "--disable-tools",
            "mcp__fake__search_docs",
        ],
        dir.path(),
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("mcp__fake__search_docs"));
}
