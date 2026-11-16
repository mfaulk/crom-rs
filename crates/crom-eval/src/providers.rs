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
///
/// Test-only extras: `tools` are registered as custom tools for the run (e.g.
/// tools from a stub MCP server), and a context log receives every model
/// request as JSON so a test can tell what reached the model's context.
#[derive(Clone)]
pub struct ScriptedTurns {
    turns: Arc<Vec<Vec<ModelEvent>>>,
    tools: Vec<Arc<dyn crom_core::Tool>>,
    context_log: Option<ModelContextLog>,
}

/// Every model request of a scripted run, serialized as JSON.
pub type ModelContextLog = Arc<std::sync::Mutex<Vec<String>>>;

impl ScriptedTurns {
    pub fn new(turns: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            turns: Arc::new(turns),
            tools: Vec::new(),
            context_log: None,
        }
    }

    pub fn with_tools(mut self, tools: Vec<Arc<dyn crom_core::Tool>>) -> Self {
        self.tools = tools;
        self
    }

    pub fn with_context_log(mut self, log: ModelContextLog) -> Self {
        self.context_log = Some(log);
        self
    }

    pub fn tools(&self) -> &[Arc<dyn crom_core::Tool>] {
        &self.tools
    }
}

impl PartialEq for ScriptedTurns {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.turns, &other.turns)
    }
}

impl Eq for ScriptedTurns {}

impl std::fmt::Debug for ScriptedTurns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ScriptedTurns({} turns, {} tools)",
            self.turns.len(),
            self.tools.len()
        )
    }
}

/// Appends each request to a [`ModelContextLog`] before replaying the script.
struct RecordingProvider {
    inner: ScriptedFakeProvider,
    log: ModelContextLog,
}

#[async_trait::async_trait]
impl ModelProvider for RecordingProvider {
    async fn stream_turn(
        &self,
        request: crom_core::ModelRequest,
    ) -> Result<
        futures::stream::BoxStream<'static, Result<ModelEvent, crom_core::ProviderError>>,
        crom_core::ProviderError,
    > {
        if let Ok(json) = serde_json::to_string(&request) {
            self.log.lock().unwrap().push(json);
        }
        self.inner.stream_turn(request).await
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

/// Env var naming the live model when `--model` is not given.
pub const MODEL_ENV: &str = "CROM_EVAL_MODEL";

/// Why a provider cannot be used. The task is reported as skipped, never
/// failed, and its `skipped_reason` starts with [`UnavailableCode::as_str`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableCode {
    /// `CROM_EVAL_LIVE=1` is not set.
    LiveDisabled,
    /// The id matches no built-in or settings profile.
    UnknownProfile,
    /// The profile's API-key env var is unset or empty, or it names none.
    CredentialsUnavailable,
    /// No `--model`, no `CROM_EVAL_MODEL`, and no profile `default_model`.
    ModelUnavailable,
}

impl UnavailableCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnavailableCode::LiveDisabled => "live_disabled",
            UnavailableCode::UnknownProfile => "unknown_profile",
            UnavailableCode::CredentialsUnavailable => "credentials_unavailable",
            UnavailableCode::ModelUnavailable => "model_unavailable",
        }
    }
}

/// Returned when a (usually live) provider cannot be used. `reason` is
/// `"<code>: <detail>"` and never contains a secret.
#[derive(Debug, Clone)]
pub struct ProviderUnavailable {
    pub code: UnavailableCode,
    pub reason: String,
}

impl ProviderUnavailable {
    fn new(code: UnavailableCode, detail: impl std::fmt::Display) -> Self {
        Self {
            code,
            reason: format!("{}: {detail}", code.as_str()),
        }
    }
}

/// Resolve the provider profile for a live eval run. Same resolver as the
/// CLI and SDK, so a profile id means the same endpoint everywhere.
pub fn resolve_live_profile(
    name: &str,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
) -> Result<ProviderProfile, ProviderUnavailable> {
    resolve_profile(name, profiles)
        .map_err(|e| ProviderUnavailable::new(UnavailableCode::UnknownProfile, e))
}

/// Model for a live run: `--model` (`model_override`), then
/// `CROM_EVAL_MODEL`, then the profile's `default_model` — the only place a
/// default model is defined.
pub fn live_model(profile: &ProviderProfile, model_override: Option<&str>) -> Option<String> {
    live_model_with(profile, model_override, &|k| crom_core::legacy::env_var(k))
}

fn live_model_with(
    profile: &ProviderProfile,
    model_override: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    model_override
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .or_else(|| env(MODEL_ENV).filter(|m| !m.is_empty()))
        .or_else(|| profile.default_model.clone())
}

/// Build a provider for `mode` and `task`. `profiles` are settings-level
/// provider-profile overrides (see `docs/providers.md`).
pub fn build(
    mode: &ProviderMode,
    task: &EvalTask,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
    model_override: Option<&str>,
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
        ProviderMode::Scripted(script) => Ok(ProviderSetup {
            config: ProviderConfig::fake({
                let inner = ScriptedFakeProvider::new(script.turns.as_ref().clone());
                match &script.context_log {
                    Some(log) => Arc::new(RecordingProvider {
                        inner,
                        log: log.clone(),
                    }) as Arc<dyn ModelProvider>,
                    None => Arc::new(inner),
                }
            }),
            model: "scripted".to_string(),
            label: "scripted".to_string(),
        }),
        ProviderMode::Live(name) => build_live(name, profiles, model_override, &process_env),
    }
}

/// `CROM_EVAL_*` lookups honor the legacy `CODE_AGENT_*` fallback; API-key
/// variables are read as named by the profile.
fn process_env(key: &str) -> Option<String> {
    if key == LIVE_ENV || key == MODEL_ENV {
        crom_core::legacy::env_var(key)
    } else {
        std::env::var(key).ok()
    }
}

/// Build a live provider. `env` is injected so tests need not mutate the
/// process environment.
fn build_live(
    name: &str,
    profiles: &BTreeMap<String, ProviderProfileConfig>,
    model_override: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<ProviderSetup, ProviderUnavailable> {
    use UnavailableCode as C;
    if env(LIVE_ENV).as_deref() != Some("1") {
        return Err(ProviderUnavailable::new(
            C::LiveDisabled,
            format!("live provider '{name}' requires {LIVE_ENV}=1 (opt-in); skipping"),
        ));
    }
    let profile = resolve_live_profile(name, profiles)?;
    let Some(env_name) = profile.api_key_env.clone() else {
        return Err(ProviderUnavailable::new(
            C::CredentialsUnavailable,
            format!("provider profile '{name}' has no api_key_env; skipping"),
        ));
    };
    let Some(key) = env(&env_name).filter(|k| !k.is_empty()) else {
        return Err(ProviderUnavailable::new(
            C::CredentialsUnavailable,
            format!("{env_name} is not set; skipping live {name} run"),
        ));
    };
    let Some(model) = live_model_with(&profile, model_override, env) else {
        return Err(ProviderUnavailable::new(
            C::ModelUnavailable,
            format!(
                "provider profile '{name}' has no default_model; pass --model or set {MODEL_ENV}"
            ),
        ));
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

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    fn unavailable(name: &str, model: Option<&str>, env: &[(&str, &str)]) -> ProviderUnavailable {
        match build_live(name, &BTreeMap::new(), model, &env_of(env)) {
            Err(u) => u,
            Ok(_) => panic!("{name} should be unavailable"),
        }
    }

    /// Without CROM_EVAL_LIVE=1 a live provider is skipped as `live_disabled`.
    #[test]
    fn live_requires_opt_in() {
        let u = unavailable("anthropic", None, &[("ANTHROPIC_API_KEY", "k")]);
        assert_eq!(u.code, UnavailableCode::LiveDisabled);
        assert!(u.reason.starts_with("live_disabled: "));
        assert!(u.reason.contains(LIVE_ENV));
    }

    /// Opted in but missing the profile's key env var skips every built-in profile as `credentials_unavailable`.
    #[test]
    fn missing_key_is_credentials_unavailable() {
        for (name, var) in [
            ("anthropic", "ANTHROPIC_API_KEY"),
            ("openai", "OPENAI_API_KEY"),
            ("deepseek", "DEEPSEEK_API_KEY"),
            ("gemini", "GEMINI_API_KEY"),
        ] {
            let u = unavailable(name, None, &[(LIVE_ENV, "1")]);
            assert_eq!(u.code, UnavailableCode::CredentialsUnavailable, "{name}");
            assert!(u.reason.starts_with("credentials_unavailable: "), "{u:?}");
            assert!(u.reason.contains(var), "{u:?}");
            let u = unavailable(name, None, &[(LIVE_ENV, "1"), (var, "")]);
            assert_eq!(u.code, UnavailableCode::CredentialsUnavailable, "{name}");
        }
    }

    /// An unknown profile id is reported as `unknown_profile`.
    #[test]
    fn unknown_profile_code() {
        let u = unavailable("nope", None, &[(LIVE_ENV, "1")]);
        assert_eq!(u.code, UnavailableCode::UnknownProfile);
    }

    /// A profile without a default model and no --model/CROM_EVAL_MODEL is `model_unavailable`.
    #[test]
    fn local_without_model_is_model_unavailable() {
        let u = unavailable(
            "local",
            None,
            &[(LIVE_ENV, "1"), ("LOCAL_OPENAI_API_KEY", "x")],
        );
        assert_eq!(u.code, UnavailableCode::ModelUnavailable);
        assert!(u.reason.contains("--model"));
    }

    /// With opt-in and a key, every built-in profile builds with the profile's default model.
    #[test]
    fn builtin_profiles_build_with_key() {
        for (name, var, default) in [
            ("anthropic", "ANTHROPIC_API_KEY", "claude-sonnet-4-6"),
            ("openai", "OPENAI_API_KEY", "gpt-4.1"),
            ("deepseek", "DEEPSEEK_API_KEY", "deepseek-chat"),
            ("gemini", "GEMINI_API_KEY", "gemini-2.5-pro"),
        ] {
            let setup = build_live(
                name,
                &BTreeMap::new(),
                None,
                &env_of(&[(LIVE_ENV, "1"), (var, "k")]),
            )
            .unwrap_or_else(|u| panic!("{name}: {u:?}"));
            assert_eq!(setup.model, default);
            assert_eq!(setup.label, name);
        }
    }

    /// Model precedence is --model, then CROM_EVAL_MODEL, then the profile default.
    #[test]
    fn model_precedence() {
        let p = resolve_profile("deepseek", &BTreeMap::new()).unwrap();
        let none = env_of(&[]);
        let env = env_of(&[(MODEL_ENV, "from-env")]);
        assert_eq!(
            live_model_with(&p, Some("flag"), &env).as_deref(),
            Some("flag")
        );
        assert_eq!(live_model_with(&p, None, &env).as_deref(), Some("from-env"));
        assert_eq!(
            live_model_with(&p, None, &none).as_deref(),
            Some("deepseek-chat")
        );
        assert_eq!(
            live_model_with(&p, Some(""), &none).as_deref(),
            Some("deepseek-chat")
        );
        let setup = build_live(
            "deepseek",
            &BTreeMap::new(),
            Some("deepseek-reasoner"),
            &env_of(&[(LIVE_ENV, "1"), ("DEEPSEEK_API_KEY", "k")]),
        )
        .unwrap();
        assert_eq!(setup.model, "deepseek-reasoner");
    }
}
