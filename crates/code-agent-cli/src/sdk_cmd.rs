//! `code-agent headless-run` — drive a single coding task through the M12
//! SDK runtime.
//!
//! This is the CLI path that proves the SDK runtime is usable from the CLI
//! without re-implementing the agent loop. It deliberately keeps the
//! surface tiny: no TUI, no interactive auth, conservative side-effect
//! defaults, and provider credentials passed via env var or `--api-key`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::Parser;

use code_agent_sdk::fake_provider::EchoFakeProvider;
use code_agent_sdk::{
    AgentMode, AgentRequest, AgentRunStatus, AgentRuntime, AllowAllApprovals, ApprovalPolicy,
    BuiltinToolSet, PatchPolicy, ProviderConfig, SecretString, SideEffectPolicy, ToolPolicy,
    WorkspaceSource,
};

#[derive(Parser, Debug)]
pub struct HeadlessRunArgs {
    /// User prompt to send to the agent.
    pub prompt: String,

    /// Workspace directory (default: cwd).
    #[arg(long, default_value = ".")]
    pub cwd: PathBuf,

    /// Provider to use. `anthropic` requires `--api-key` or `ANTHROPIC_API_KEY`.
    /// `fake` uses a deterministic echo provider — handy for testing the CLI
    /// path without a real account.
    #[arg(long, default_value = "anthropic")]
    pub provider: String,

    /// API key. If omitted, falls back to the env var matching the provider.
    #[arg(long)]
    pub api_key: Option<String>,

    /// Model id override.
    #[arg(long)]
    pub model: Option<String>,

    /// Max turns budget.
    #[arg(long, default_value_t = 8)]
    pub max_turns: u32,

    /// Max duration in seconds (deadline).
    #[arg(long)]
    pub deadline_secs: Option<u64>,

    /// Allow the agent to write to the workspace (default: patch-only output).
    #[arg(long)]
    pub apply_patch: bool,

    /// Allow Bash command execution (default: deny).
    #[arg(long)]
    pub allow_bash: bool,

    /// Auto-approve every approval prompt the SDK raises. Without this the
    /// SDK refuses to mutate by default.
    #[arg(long)]
    pub auto_approve: bool,

    /// Plan-only run.
    #[arg(long)]
    pub plan_only: bool,
}

pub async fn cmd_headless_run(args: HeadlessRunArgs) -> Result<()> {
    let provider_config = build_provider_config(&args)?;
    let mut builder = AgentRuntime::builder()
        .provider(provider_config)
        .workspace(WorkspaceSource::filesystem(&args.cwd));
    if let Some(m) = &args.model {
        builder = builder.model(m.clone());
    }
    let side_effects = if args.allow_bash {
        SideEffectPolicy::command_execution()
    } else if args.apply_patch {
        SideEffectPolicy::workspace_write()
    } else {
        SideEffectPolicy::read_only()
    };
    let builtins = if args.allow_bash {
        BuiltinToolSet::cli_defaults()
    } else if args.apply_patch {
        BuiltinToolSet::patch_only_defaults()
    } else {
        BuiltinToolSet::read_only_defaults()
    };
    builder = builder.tool_policy(ToolPolicy {
        enable_builtin_tools: builtins,
        side_effect_policy: side_effects,
        ..Default::default()
    });
    if args.auto_approve {
        builder = builder.approval_callback(Arc::new(AllowAllApprovals));
    }
    let runtime = builder
        .build()
        .await
        .map_err(|e| anyhow!("runtime build failed: {}", e.message))?;

    let mut request = AgentRequest::new(&args.prompt).max_turns(args.max_turns);
    if args.plan_only {
        request = request.mode(AgentMode::PlanOnly);
    } else if args.apply_patch {
        request = request
            .mode(AgentMode::ModifyWorkspace)
            .patch_policy(PatchPolicy::ApplyAndReturn);
    } else {
        request = request
            .mode(AgentMode::GeneratePatch)
            .patch_policy(PatchPolicy::ReturnOnly);
    }
    if args.auto_approve {
        request = request.approval_policy(ApprovalPolicy::CallbackRequired);
    }
    if let Some(secs) = args.deadline_secs {
        request = request.deadline(Duration::from_secs(secs));
    }

    let result = runtime
        .run_once(request)
        .await
        .map_err(|e| anyhow!("run failed: {}: {}", display_kind(&e), e.message))?;

    println!("status: {:?}", result.status);
    if !result.summary.is_empty() {
        println!("summary: {}", result.summary);
    }
    println!(
        "events: total={}, tool_calls={}, approvals={}, diagnostics={}",
        result.events_summary.total_events,
        result.events_summary.tool_calls,
        result.events_summary.approvals_requested,
        result.events_summary.diagnostics_count,
    );
    if let Some(patch) = &result.patch {
        if !patch.files.is_empty() {
            println!("--- patch ---");
            println!("{}", patch.unified_diff);
        }
    }
    if !result.diagnostics.is_empty() {
        println!("--- diagnostics ---");
        for d in &result.diagnostics {
            println!("  [{:?}] {}: {}", d.severity, d.code, d.message);
        }
    }
    if !result.validations.is_empty() {
        println!("--- validations ---");
        for v in &result.validations {
            println!(
                "  {} {} (exit={:?}, success={})",
                v.command,
                if v.success { "PASS" } else { "FAIL" },
                v.exit_code,
                v.success
            );
        }
    }
    match result.status {
        AgentRunStatus::Completed => Ok(()),
        AgentRunStatus::Cancelled => Err(anyhow!("run cancelled")),
        AgentRunStatus::TimedOut => Err(anyhow!("run timed out")),
        AgentRunStatus::Failed
        | AgentRunStatus::NeedsApproval
        | AgentRunStatus::NeedsReplan
        | AgentRunStatus::ValidationFailed => {
            Err(anyhow!("non-success status: {:?}", result.status))
        }
    }
}

fn build_provider_config(args: &HeadlessRunArgs) -> Result<ProviderConfig> {
    match args.provider.as_str() {
        "anthropic" => {
            let key = args
                .api_key
                .clone()
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| {
                    anyhow!("provide --api-key or set ANTHROPIC_API_KEY for `--provider anthropic`")
                })?;
            Ok(ProviderConfig::anthropic_api_key(SecretString::new(key)))
        }
        "fake" => Ok(ProviderConfig::fake(Arc::new(EchoFakeProvider))),
        other => Err(anyhow!(
            "unsupported --provider {other}; use anthropic or fake"
        )),
    }
}

fn display_kind(e: &code_agent_sdk::AgentSdkError) -> String {
    format!("{:?}", e.kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cmd_headless_run` with the fake provider returns Ok and produces a result.
    #[tokio::test]
    async fn fake_provider_path_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let args = HeadlessRunArgs {
            prompt: "hello".into(),
            cwd: dir.path().to_path_buf(),
            provider: "fake".into(),
            api_key: None,
            model: None,
            max_turns: 1,
            deadline_secs: Some(5),
            apply_patch: false,
            allow_bash: false,
            auto_approve: false,
            plan_only: false,
        };
        let res = cmd_headless_run(args).await;
        assert!(res.is_ok(), "expected ok: {res:?}");
    }

    /// `--provider anthropic` without an API key returns an error.
    #[tokio::test]
    async fn missing_anthropic_key_errors() {
        // Make sure env var isn't set.
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");
        let dir = tempfile::TempDir::new().unwrap();
        let args = HeadlessRunArgs {
            prompt: "hello".into(),
            cwd: dir.path().to_path_buf(),
            provider: "anthropic".into(),
            api_key: None,
            model: None,
            max_turns: 1,
            deadline_secs: None,
            apply_patch: false,
            allow_bash: false,
            auto_approve: false,
            plan_only: false,
        };
        let res = cmd_headless_run(args).await;
        assert!(res.is_err());
        if let Some(k) = prev {
            std::env::set_var("ANTHROPIC_API_KEY", k);
        }
    }
}
