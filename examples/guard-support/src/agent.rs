use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use async_trait::async_trait;
use crom_core::{
    ContentBlock, ModelEvent, ModelProvider, ModelRequest, ProviderError, ToolCall,
    ToolResultContent, ToolRule,
};
use crom_sdk::fake_provider::ScriptedFakeProvider;
use crom_sdk::{
    AgentRequest, AgentRunStatus, AgentRuntime, FileStorage, GuardConnection, ProviderConfig,
    RunBudgets, SdkEvent, SecretString, SessionPolicy, SideEffectPolicy, ToolPolicy,
    WorkspaceSource,
};
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};

use crate::admin::Secret;
use crate::report::read_ledger;
use crate::trusted_app::TrustedApp;

/// One scripted model turn: a single tool call.
#[derive(Debug, Clone)]
pub struct ScriptedCall {
    pub id: String,
    pub tool: String,
    pub input: Value,
}

/// A tool result as the model received it.
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub is_error: bool,
    pub text: String,
}

impl ToolOutput {
    /// The call failed: a tool error, or a command that exited non-zero
    /// (Bash reports its exit status as `[exit: N]`).
    pub fn failed(&self) -> bool {
        self.is_error || (self.text.contains("[exit: ") && !self.text.contains("[exit: 0]"))
    }
}

/// Ledger length and supervisor-approval count when a model turn started.
#[derive(Debug, Clone, Copy, Default)]
pub struct Mark {
    pub ledger: usize,
    pub approvals: usize,
}

/// Scripted model: one tool call per turn, then a final message. Before each
/// turn it records a [`Mark`], so each call's ledger effects and approvals
/// can be attributed to it, and it keeps every request (the model's whole
/// context) for the leak checks.
struct MarkingProvider {
    inner: ScriptedFakeProvider,
    ledger: PathBuf,
    app: Arc<TrustedApp>,
    marks: Arc<Mutex<Vec<Mark>>>,
    requests: Arc<Mutex<Vec<Value>>>,
    results: Arc<Mutex<BTreeMap<String, ToolOutput>>>,
}

#[async_trait]
impl ModelProvider for MarkingProvider {
    async fn stream_turn(
        &self,
        request: ModelRequest,
    ) -> Result<BoxStream<'static, Result<ModelEvent, ProviderError>>, ProviderError> {
        for block in request.messages.iter().flat_map(|m| &m.content) {
            if let ContentBlock::ToolResult(r) = block {
                let text = r
                    .content
                    .iter()
                    .map(|ToolResultContent::Text { text }| text.as_str())
                    .collect();
                let output = ToolOutput {
                    is_error: r.is_error,
                    text,
                };
                self.results
                    .lock()
                    .unwrap()
                    .insert(r.tool_use_id.clone(), output);
            }
        }
        let logged = serde_json::to_value(&request).unwrap_or(Value::Null);
        self.requests.lock().unwrap().push(logged);
        let ledger = read_ledger(&self.ledger).map(|l| l.len()).unwrap_or(0);
        self.marks.lock().unwrap().push(Mark {
            ledger,
            approvals: self.app.approval_count(),
        });
        self.inner.stream_turn(request).await
    }
}

fn scripted_turns(calls: &[ScriptedCall]) -> Vec<Vec<ModelEvent>> {
    let mut turns: Vec<Vec<ModelEvent>> = calls
        .iter()
        .map(|c| {
            vec![
                ModelEvent::ToolCall(ToolCall {
                    id: c.id.clone(),
                    name: c.tool.clone(),
                    input: c.input.clone(),
                }),
                ModelEvent::MessageStop {
                    stop_reason: Some("tool_use".into()),
                    usage: None,
                },
            ]
        })
        .collect();
    turns.push(vec![
        ModelEvent::TextDelta {
            text: "Ticket handled.".into(),
        },
        ModelEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        },
    ]);
    turns
}

pub enum Model {
    Scripted(Vec<ScriptedCall>),
    Live {
        provider: Box<ProviderConfig>,
        model: String,
        pricing: crom_core::ModelPricing,
        max_cost_usd: f64,
    },
}

pub struct AgentRun {
    pub status: AgentRunStatus,
    pub summary: String,
    pub events: Vec<SdkEvent>,
    pub marks: Vec<Mark>,
    /// Tool results by call id (scripted runs only).
    pub results: BTreeMap<String, ToolOutput>,
    pub usage: Option<crom_core::TokenUsage>,
}

pub struct RunSpec<'a> {
    pub label: &'a str,
    pub task: &'a str,
    pub guard_url: &'a str,
    pub handle: &'a Secret,
    pub app: &'a Arc<TrustedApp>,
    /// Sandboxed Bash for the agent (the scripted bypass attempts).
    pub bash: bool,
    pub workspace: &'a Path,
    pub ledger: &'a Path,
    /// Artifact directory: events, model requests, sessions.
    pub out: &'a Path,
    pub max_turns: u32,
}

/// The agent's tool policy: Guard as the only MCP server, sandbox
/// `required`, and optionally sandboxed Bash.
pub fn policy(connection: GuardConnection, bash: bool) -> ToolPolicy {
    let mut policy = ToolPolicy::guard(connection);
    if bash {
        policy.enable_builtin_tools.bash = true;
        policy.side_effect_policy = SideEffectPolicy::command_execution();
        policy
            .allowed_tools
            .push(ToolRule::ExactTool("Bash".into()));
    }
    policy
}

/// The agent's [`policy`] as the Crom settings file `crom-guard check`
/// reads. The in-process SDK agent reads no settings file, so the preflight
/// checks this rendering of the policy it actually runs with.
pub fn crom_settings(guard_url: &str, bash: bool) -> Result<serde_json::Value> {
    // The handle is never written: the settings interpolate it, as
    // agent/crom-settings.json does.
    let header = format!("Bearer ${{{HANDLE_VAR}}}");
    let placeholder = SecretString::new(format!("${{{HANDLE_VAR}}}"));
    let p = policy(GuardConnection::new(guard_url, placeholder), bash);
    let guard = p.guard.as_ref().context("policy has no Guard connection")?;
    let mut allowed = Vec::new();
    for rule in &p.allowed_tools {
        allowed.push(match rule {
            ToolRule::McpServerGlob { server, tool_glob } => format!("mcp__{server}__{tool_glob}"),
            ToolRule::ExactTool(name) => name.clone(),
            other => anyhow::bail!("no settings form for allow rule {other:?}"),
        });
    }
    Ok(serde_json::json!({
        "sandbox": {
            "mode": p.sandbox.mode.as_str(),
            "network": p.sandbox.network,
            "read_paths": p.sandbox.read_paths,
            "env_allow": p.sandbox.env_allow,
        },
        "mcp": {
            "enabled": true,
            "servers": {
                crom_sdk::GUARD_SERVER_NAME: {
                    "type": "http",
                    "url": guard.url,
                    "headers": {"Authorization": header},
                    "guard": true,
                }
            }
        },
        // The SDK loads no plugins.
        "plugins": {"allow_plugin_mcp": false},
        "allowed_tools": allowed,
    }))
}

const HANDLE_VAR: &str = "CROM_GUARD_HANDLE";

/// Runs one Crom session for the ticket. The handle reaches only Crom's
/// MCP client; the model never sees it.
pub async fn run(spec: RunSpec<'_>, model: Model) -> Result<AgentRun> {
    let connection = GuardConnection::new(
        spec.guard_url,
        SecretString::new(spec.handle.expose().to_owned()),
    )
    .with_approver(spec.app.clone());
    let policy = policy(connection, spec.bash);
    let sessions = spec.out.join("crom-sessions");
    std::fs::create_dir_all(&sessions)?;
    let marks = Arc::new(Mutex::new(Vec::new()));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let results = Arc::new(Mutex::new(BTreeMap::new()));
    let mut builder = AgentRuntime::builder()
        .tool_policy(policy)
        .workspace(WorkspaceSource::Filesystem(spec.workspace.to_path_buf()))
        .storage(Arc::new(FileStorage::new(&sessions)?));
    let mut request = AgentRequest::new(spec.task)
        .session_policy(SessionPolicy::Persisted)
        .max_turns(spec.max_turns)
        .label("ticket", spec.label);
    match model {
        Model::Scripted(calls) => {
            builder = builder.provider(ProviderConfig::fake(Arc::new(MarkingProvider {
                inner: ScriptedFakeProvider::new(scripted_turns(&calls)),
                ledger: spec.ledger.to_path_buf(),
                app: spec.app.clone(),
                marks: marks.clone(),
                requests: requests.clone(),
                results: results.clone(),
            })));
        }
        Model::Live {
            provider,
            model,
            pricing,
            max_cost_usd,
        } => {
            builder = builder.provider(*provider).model(model).pricing(pricing);
            request = request.budgets(RunBudgets::unlimited().with_max_cost_usd(max_cost_usd));
        }
    }
    let runtime = builder.build().await?;
    let mut run = runtime.run(request).await?;
    let mut events = Vec::new();
    while let Some(ev) = run.events.next().await {
        events.push(ev?);
    }
    let result = run.final_result().await?;

    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spec.out.join("crom-events.jsonl"))?;
    for ev in &events {
        writeln!(log, "{}", json!({"run": spec.label, "event": ev}))?;
    }
    let mut req_log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(spec.out.join("model-requests.jsonl"))?;
    for r in requests.lock().unwrap().iter() {
        writeln!(req_log, "{}", json!({"run": spec.label, "request": r}))?;
    }
    let mut summary = result.summary.clone();
    for d in &result.diagnostics {
        summary.push('\n');
        summary.push_str(&d.message);
    }
    let marks = marks.lock().unwrap().clone();
    let results = results.lock().unwrap().clone();
    Ok(AgentRun {
        status: result.status,
        summary,
        events,
        marks,
        results,
        usage: result.usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked settings render the agent's policy: sandbox required without network, Guard as the only MCP server, and the matching allow rules, with no handle value.
    #[test]
    fn crom_settings_render_the_agent_policy() {
        let s = crom_settings("http://127.0.0.1:9999/mcp", true).unwrap();
        assert_eq!(s["sandbox"]["mode"], "required");
        assert_eq!(s["sandbox"]["network"], false);
        let servers = s["mcp"]["servers"].as_object().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers["guard"]["url"], "http://127.0.0.1:9999/mcp");
        assert_eq!(servers["guard"]["guard"], true);
        assert_eq!(s["allowed_tools"], json!(["mcp__guard__*", "Bash"]));
        assert_eq!(
            servers["guard"]["headers"]["Authorization"],
            format!("Bearer ${{{HANDLE_VAR}}}")
        );
    }

    /// Without Bash the rendering matches the reference settings in agent/crom-settings.json, apart from the empty sandbox lists.
    #[test]
    fn crom_settings_match_the_reference_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("agent/crom-settings.json");
        let reference: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut s = crom_settings("http://127.0.0.1:8470/mcp", false).unwrap();
        let sandbox = s["sandbox"].as_object_mut().unwrap();
        assert_eq!(sandbox.remove("read_paths"), Some(json!([])));
        assert_eq!(sandbox.remove("env_allow"), Some(json!([])));
        assert_eq!(s, reference);
    }
}
