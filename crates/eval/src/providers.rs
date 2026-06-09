//! Provider execution modes: fake, golden, and live.
//!
//! - **Fake** — a trivial echo provider. Proves the harness plumbing (workspace
//!   setup, event capture, oracles, scoring, artifacts) without proving model
//!   intelligence. Deterministic and offline.
//! - **Golden** — a scripted provider that replays a canned sequence of tool
//!   calls for a specific task (read → edit → run check → observe failure →
//!   edit → pass). Catches regressions in tool schemas, event handling,
//!   sessions, and command execution.
//! - **Live** — a real model provider (e.g. Anthropic). Opt-in only, gated by
//!   the `CODE_AGENT_EVAL_LIVE` environment variable and provider credentials.
//!   Never runs in ordinary CI.

use std::sync::Arc;

use code_agent_core::{ModelEvent, ModelProvider, TokenUsage, ToolCall};
use code_agent_sdk::fake_provider::{EchoFakeProvider, ScriptedFakeProvider};
use code_agent_sdk::ProviderConfig;

use crate::task::EvalTask;
use crate::LIVE_ENV;

/// Which provider backs an eval run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderMode {
    /// Echo provider — offline, deterministic, proves the harness only.
    Fake,
    /// Scripted golden provider keyed by task.
    Golden,
    /// Live model provider by name (currently `anthropic`).
    Live(String),
}

impl ProviderMode {
    /// Parse a `--provider` CLI value.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "fake" => ProviderMode::Fake,
            "golden" => ProviderMode::Golden,
            other => ProviderMode::Live(other.to_string()),
        }
    }

    pub fn label(&self) -> String {
        match self {
            ProviderMode::Fake => "fake".to_string(),
            ProviderMode::Golden => "golden".to_string(),
            ProviderMode::Live(name) => name.clone(),
        }
    }

    pub fn is_live(&self) -> bool {
        matches!(self, ProviderMode::Live(_))
    }
}

/// A resolved provider ready to hand to the SDK runtime.
pub struct ProviderSetup {
    pub config: ProviderConfig,
    pub model: String,
    pub label: String,
}

/// Returned when a (usually live) provider cannot be used — missing opt-in flag
/// or missing credentials. The task is reported as skipped, never failed.
#[derive(Debug, Clone)]
pub struct ProviderUnavailable {
    pub reason: String,
}

/// Build a provider for `mode` and `task`.
pub fn build(mode: &ProviderMode, task: &EvalTask) -> Result<ProviderSetup, ProviderUnavailable> {
    match mode {
        ProviderMode::Fake => Ok(ProviderSetup {
            config: ProviderConfig::fake(Arc::new(EchoFakeProvider)),
            model: "fake-echo".to_string(),
            label: "fake".to_string(),
        }),
        ProviderMode::Golden => {
            let provider: Arc<dyn ModelProvider> = match golden_script(task.golden_script_name()) {
                Some(script) => Arc::new(ScriptedFakeProvider::new(script)),
                None => Arc::new(EchoFakeProvider),
            };
            Ok(ProviderSetup {
                config: ProviderConfig::fake(provider),
                model: "golden".to_string(),
                label: "golden".to_string(),
            })
        }
        ProviderMode::Live(name) => build_live(name),
    }
}

fn build_live(name: &str) -> Result<ProviderSetup, ProviderUnavailable> {
    if std::env::var(LIVE_ENV).ok().as_deref() != Some("1") {
        return Err(ProviderUnavailable {
            reason: format!("live provider '{name}' requires {LIVE_ENV}=1 (opt-in); skipping"),
        });
    }
    match name {
        "anthropic" => {
            let key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty());
            match key {
                Some(key) => Ok(ProviderSetup {
                    config: ProviderConfig::anthropic_api_key(code_agent_sdk::SecretString::new(
                        key,
                    )),
                    model: std::env::var("CODE_AGENT_EVAL_MODEL")
                        .unwrap_or_else(|_| "claude-sonnet-4-6".to_string()),
                    label: "anthropic".to_string(),
                }),
                None => Err(ProviderUnavailable {
                    reason: "ANTHROPIC_API_KEY is not set; skipping live anthropic run".to_string(),
                }),
            }
        }
        other => Err(ProviderUnavailable {
            reason: format!("unknown live provider '{other}'"),
        }),
    }
}

// --- Golden script construction helpers ---------------------------------

fn tool_call_turn(id: &str, name: &str, input: serde_json::Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".to_string()),
            usage: Some(TokenUsage::default()),
        },
    ]
}

fn final_text_turn(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta {
            text: text.to_string(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(TokenUsage::default()),
        },
    ]
}

/// Look up the canned model script for a golden task id.
///
/// Scripts are deliberately fixed: they replay the same tool calls regardless
/// of the (deterministic) workspace, so the workspace responds for real while
/// the model's "intelligence" is removed from the loop.
pub fn golden_script(name: &str) -> Option<Vec<Vec<ModelEvent>>> {
    match name {
        // Read a config file, attempt a check that fails because the value is
        // wrong, edit the value, re-run the check (now passing), then finish.
        // Exercises read → failed command → focused edit → passing command.
        "golden-edit-recover" => Some(vec![
            tool_call_turn("g1", "Read", serde_json::json!({ "file_path": "data.txt" })),
            tool_call_turn(
                "g2",
                "Bash",
                serde_json::json!({ "command": "grep -q 'value=42' data.txt" }),
            ),
            tool_call_turn(
                "g3",
                "Edit",
                serde_json::json!({
                    "file_path": "data.txt",
                    "old_string": "value=0",
                    "new_string": "value=42"
                }),
            ),
            tool_call_turn(
                "g4",
                "Bash",
                serde_json::json!({ "command": "grep -q 'value=42' data.txt" }),
            ),
            final_text_turn("Updated data.txt so value=42; the check now passes."),
        ]),

        // Rust variant: inspect the source, run the failing test, fix the bug,
        // re-run, finish. Requires cargo and is exercised via the `golden`
        // suite rather than the default cargo-test lane.
        "golden-rust-recover" => Some(vec![
            tool_call_turn(
                "r1",
                "Read",
                serde_json::json!({ "file_path": "src/lib.rs" }),
            ),
            tool_call_turn(
                "r2",
                "Bash",
                serde_json::json!({ "command": "cargo test --all-targets" }),
            ),
            tool_call_turn(
                "r3",
                "Edit",
                serde_json::json!({
                    "file_path": "src/lib.rs",
                    "old_string": "a - b",
                    "new_string": "a + b"
                }),
            ),
            tool_call_turn(
                "r4",
                "Bash",
                serde_json::json!({ "command": "cargo test --all-targets" }),
            ),
            final_text_turn("Fixed add() to use addition; tests pass."),
        ]),

        // M16 repair-smoke: within-run recovery. Read, run the failing check,
        // make the correct edit, re-run the check (passes), finish. The agent
        // validates after editing, so `validation_after_last_edit` is true.
        "repair-grep-recover" => Some(vec![
            tool_call_turn(
                "rr1",
                "Read",
                serde_json::json!({ "file_path": "data.txt" }),
            ),
            tool_call_turn(
                "rr2",
                "Bash",
                serde_json::json!({ "command": "grep -q 'value=42' data.txt" }),
            ),
            tool_call_turn(
                "rr3",
                "Edit",
                serde_json::json!({
                    "file_path": "data.txt",
                    "old_string": "value=0",
                    "new_string": "value=42"
                }),
            ),
            tool_call_turn(
                "rr4",
                "Bash",
                serde_json::json!({ "command": "grep -q 'value=42' data.txt" }),
            ),
            final_text_turn("Set value=42; the check now passes."),
        ]),

        // M16 repair-smoke: repeated-failure trap. The script makes a
        // non-fixing edit and stops. The oracle fails; on the next attempt the
        // identical script replays (the edit no longer matches the workspace)
        // and the oracle fails with the *same* fingerprint, which the repair
        // loop detects and stops on.
        "repair-grep-trap" => Some(vec![
            tool_call_turn(
                "rt1",
                "Read",
                serde_json::json!({ "file_path": "data.txt" }),
            ),
            tool_call_turn(
                "rt2",
                "Edit",
                serde_json::json!({
                    "file_path": "data.txt",
                    "old_string": "value=0",
                    "new_string": "value=1"
                }),
            ),
            final_text_turn("Changed the value."),
        ]),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!(ProviderMode::parse("fake"), ProviderMode::Fake);
        assert_eq!(ProviderMode::parse("golden"), ProviderMode::Golden);
        assert_eq!(
            ProviderMode::parse("anthropic"),
            ProviderMode::Live("anthropic".to_string())
        );
        assert!(ProviderMode::parse("anthropic").is_live());
    }

    #[test]
    fn golden_scripts_exist() {
        assert!(golden_script("golden-edit-recover").is_some());
        assert!(golden_script("golden-rust-recover").is_some());
        assert!(golden_script("does-not-exist").is_none());
    }

    #[test]
    fn live_requires_opt_in() {
        // Without the opt-in env var, live providers are unavailable. We avoid
        // mutating process env (racy across tests) by checking the real state:
        // ordinary test runs do not set CODE_AGENT_EVAL_LIVE=1.
        if std::env::var(LIVE_ENV).ok().as_deref() != Some("1") {
            match build_live("anthropic") {
                Err(err) => assert!(err.reason.contains(LIVE_ENV)),
                Ok(_) => panic!("live provider should be unavailable without opt-in"),
            }
        }
    }
}
