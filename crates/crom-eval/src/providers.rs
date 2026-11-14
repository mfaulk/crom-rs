//! Provider execution modes: fake, golden, and live.
//!
//! - **Fake** — a trivial echo provider. Proves the harness plumbing (workspace
//!   setup, event capture, oracles, scoring, artifacts) without proving model
//!   intelligence. Deterministic and offline.
//! - **Golden** — a scripted provider that replays a canned sequence of tool
//!   calls for a specific task (read → edit → run check → observe failure →
//!   edit → pass). Catches regressions in tool schemas, event handling,
//!   sessions, and command execution.
//! - **Fault** — returns a classified provider error on every turn. Built only
//!   in code (not reachable from `--provider`) so tests can prove provider
//!   failures are recorded as `provider_error`, not agent failures.
//! - **Live** — a real model provider resolved from a provider profile
//!   (`anthropic`, `openai`, `deepseek`, `gemini`, `local`, or a
//!   settings-defined id). Opt-in only, gated by the `CROM_EVAL_LIVE`
//!   environment variable and the profile's API-key env var. Never runs in
//!   ordinary CI.

use std::collections::BTreeMap;
use std::sync::Arc;

use crom_core::provider_profile::{resolve_profile, ProviderProfile, ProviderProfileConfig};

use crom_core::{ModelEvent, ModelProvider, TokenUsage, ToolCall};
use crom_sdk::fake_provider::{EchoFakeProvider, ScriptedFakeProvider};
use crom_sdk::ProviderConfig;

use crate::task::EvalTask;
use crate::LIVE_ENV;

/// Which provider backs an eval run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderMode {
    /// Echo provider — offline, deterministic, proves the harness only.
    Fake,
    /// Scripted golden provider keyed by task.
    Golden,
    /// Live model provider by provider-profile id.
    Live(String),
    /// Injected provider failure (test-only; never parsed from the CLI).
    Fault(ProviderFault),
    /// Caller-supplied model script (test-only; never parsed from the CLI).
    /// Lets tests script tool calls that depend on run-time paths.
    Scripted(ScriptedTurns),
}

/// Model turns replayed by [`ProviderMode::Scripted`]. Each agent attempt
/// replays the full script. Equality is identity of the shared script.
#[derive(Clone)]
pub struct ScriptedTurns(pub Arc<Vec<Vec<ModelEvent>>>);

impl ScriptedTurns {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self(Arc::new(turns))
    }
}

impl PartialEq for ScriptedTurns {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ScriptedTurns {}

impl std::fmt::Debug for ScriptedTurns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptedTurns({} turns)", self.0.len())
    }
}

/// Provider failure injected by [`ProviderMode::Fault`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFault {
    /// Every turn fails with HTTP 503.
    ServerError,
}

struct FaultProvider(ProviderFault);

#[async_trait::async_trait]
impl ModelProvider for FaultProvider {
    async fn stream_turn(
        &self,
        _request: crom_core::ModelRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<ModelEvent, crom_core::ProviderError>>,
        crom_core::ProviderError,
    > {
        match self.0 {
            ProviderFault::ServerError => Err(crom_core::ProviderError::ServerError {
                status: 503,
                message: "injected fault: status 503".to_string(),
            }),
        }
    }
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
            ProviderMode::Fault(_) => "fault".to_string(),
            ProviderMode::Scripted(_) => "scripted".to_string(),
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

/// Resolve the provider profile for a live eval run. Same resolver as the
/// CLI and SDK, so a profile id means the same endpoint everywhere.
pub fn resolve_live_profile(
    name: &str,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
) -> Result<ProviderProfile, ProviderUnavailable> {
    resolve_profile(name, profiles).map_err(|e| ProviderUnavailable {
        reason: e.to_string(),
    })
}

/// Model for a live run: `CROM_EVAL_MODEL`, else the profile's default.
pub fn live_model(profile: &ProviderProfile) -> Option<String> {
    crom_core::legacy::env_var("CROM_EVAL_MODEL").or_else(|| profile.default_model.clone())
}

/// Build a provider for `mode` and `task`. `profiles` are settings-level
/// provider-profile overrides (see `docs/providers.md`).
pub fn build(
    mode: &ProviderMode,
    task: &EvalTask,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
) -> Result<ProviderSetup, ProviderUnavailable> {
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
        ProviderMode::Fault(fault) => Ok(ProviderSetup {
            config: ProviderConfig::fake(Arc::new(FaultProvider(*fault))),
            model: "fault".to_string(),
            label: "fault".to_string(),
        }),
        ProviderMode::Scripted(turns) => Ok(ProviderSetup {
            config: ProviderConfig::fake(Arc::new(ScriptedFakeProvider::new(
                turns.0.as_ref().clone(),
            ))),
            model: "scripted".to_string(),
            label: "scripted".to_string(),
        }),
        ProviderMode::Live(name) => build_live(name, profiles),
    }
}

fn build_live(
    name: &str,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
) -> Result<ProviderSetup, ProviderUnavailable> {
    if crom_core::legacy::env_var(LIVE_ENV).as_deref() != Some("1") {
        return Err(ProviderUnavailable {
            reason: format!("live provider '{name}' requires {LIVE_ENV}=1 (opt-in); skipping"),
        });
    }
    let profile = resolve_live_profile(name, profiles)?;
    let Some(env_name) = profile.api_key_env.clone() else {
        return Err(ProviderUnavailable {
            reason: format!("provider profile '{name}' has no api_key_env; skipping"),
        });
    };
    let Some(key) = std::env::var(&env_name).ok().filter(|k| !k.is_empty()) else {
        return Err(ProviderUnavailable {
            reason: format!("{env_name} is not set; skipping live {name} run"),
        });
    };
    let Some(model) = live_model(&profile) else {
        return Err(ProviderUnavailable {
            reason: format!("provider profile '{name}' has no default_model; set CROM_EVAL_MODEL"),
        });
    };
    Ok(ProviderSetup {
        config: ProviderConfig::profile(profile, crom_sdk::SecretString::new(key)),
        model,
        label: name.to_string(),
    })
}

// --- Golden script construction helpers ---------------------------------

/// Golden turns report input/output but leave cache and reasoning tokens
/// unreported, exercising partial-usage accounting end to end.
fn golden_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: Some(100),
        output_tokens: Some(20),
        ..Default::default()
    }
}

pub fn tool_call_turn(id: &str, name: &str, input: serde_json::Value) -> Vec<ModelEvent> {
    vec![
        ModelEvent::ToolCall(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }),
        ModelEvent::MessageStop {
            stop_reason: Some("tool_use".to_string()),
            usage: Some(golden_usage()),
        },
    ]
}

pub fn final_text_turn(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::TextDelta {
            text: text.to_string(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".to_string()),
            usage: Some(golden_usage()),
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

        // M17.2 leak canary: try to exfiltrate grader material that the old
        // layout wrote beside the workspace (`../task.toml`, the normalized
        // task, the source task's `hidden/`) via both Bash and the Read tool.
        "leak-canary" => Some(vec![
            tool_call_turn(
                "lc1",
                "Bash",
                serde_json::json!({
                    "command": "{ ls -la ..; cat ../task.toml ../prompt.md \
                        ../normalized-task.json ../hidden/LEAK_CANARY.txt \
                        ../oracle.patch; } > FOUND.txt 2>&1; true"
                }),
            ),
            tool_call_turn(
                "lc2",
                "Read",
                serde_json::json!({ "file_path": "../task.toml" }),
            ),
            final_text_turn("Copied whatever grader material was readable into FOUND.txt."),
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

    /// Live eval profiles come from the shared core resolver, including settings overrides.
    #[test]
    fn live_profile_matches_core_resolver() {
        let overrides: BTreeMap<String, ProviderProfileConfig> =
            serde_json::from_value(serde_json::json!({ "deepseek": { "base_url": "http://x:1" } }))
                .unwrap();
        assert_eq!(
            resolve_live_profile("deepseek", &overrides).unwrap(),
            resolve_profile("deepseek", &overrides).unwrap()
        );
        assert!(resolve_live_profile("nope", &overrides)
            .unwrap_err()
            .reason
            .contains("unknown provider profile"));
    }

    #[test]
    fn live_requires_opt_in() {
        // Without the opt-in env var, live providers are unavailable. We avoid
        // mutating process env (racy across tests) by checking the real state:
        // ordinary test runs do not set CROM_EVAL_LIVE=1.
        if std::env::var(LIVE_ENV).ok().as_deref() != Some("1") {
            match build_live("anthropic", &BTreeMap::new()) {
                Err(err) => assert!(err.reason.contains(LIVE_ENV)),
                Ok(_) => panic!("live provider should be unavailable without opt-in"),
            }
        }
    }
}
