//! P4 environment scrubbing. Kept in its own test binary because it mutates
//! the process environment.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use crom_core::{SandboxConfig, SessionId, Tool, ToolContext};
use crom_tools::{BashTool, ToolLimits};
use serde_json::json;

// P4: provider keys and other non-allowlisted host variables are absent from the sandboxed child.
#[tokio::test]
async fn provider_keys_are_absent_in_child_env() {
    if let Err(reason) = crom_tools::sandbox::probe(&SandboxConfig::required()) {
        if std::env::var_os("CROM_REQUIRE_SANDBOX_TESTS").is_some() {
            panic!("sandbox required by CROM_REQUIRE_SANDBOX_TESTS but unavailable: {reason}");
        }
        eprintln!("SKIP: OS sandbox unavailable on this host: {reason}");
        return;
    }
    // This test binary holds a single test, so no other thread reads the
    // environment concurrently.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-fake-should-not-leak");
    std::env::set_var("OPENAI_API_KEY", "sk-fake-should-not-leak");
    std::env::set_var("CROM_TEST_UNLISTED", "unlisted-should-not-leak");
    std::env::set_var("CROM_TEST_LISTED", "listed-value");
    let ws = tempfile::TempDir::new().unwrap();
    let tool = BashTool::new(Arc::new(ToolLimits {
        sandbox: SandboxConfig {
            env_allow: vec!["CROM_TEST_LISTED".into()],
            ..SandboxConfig::required()
        },
        ..ToolLimits::default()
    }));
    let r = tool
        .execute(
            json!({"command": "env"}),
            ToolContext::new_minimal(ws.path().to_path_buf(), SessionId::new()),
        )
        .await
        .unwrap();
    let crom_core::ToolContent::Text(out) = &r.content[0];
    assert!(out.contains("[exit: 0]"), "{out}");
    assert!(!out.contains("should-not-leak"), "{out}");
    assert!(!out.contains("ANTHROPIC_API_KEY"), "{out}");
    assert!(out.contains("CROM_TEST_LISTED=listed-value"), "{out}");
    let home = out
        .lines()
        .find_map(|l| l.strip_prefix("HOME="))
        .expect("HOME set");
    assert_ne!(
        Some(home),
        std::env::var("HOME").ok().as_deref(),
        "HOME must not point at the host home"
    );
}
