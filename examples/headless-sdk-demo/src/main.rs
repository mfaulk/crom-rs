//! Headless SDK demo. Showcases the SDK runtime through subcommands that
//! run entirely against the fake provider — no real model account needed.
//!
//! Usage:
//!   cargo run -p headless-sdk-demo -- simple-patch
//!   cargo run -p headless-sdk-demo -- in-memory
//!   cargo run -p headless-sdk-demo -- plan-act
//!   cargo run -p headless-sdk-demo -- fake-provider
//!   cargo run -p headless-sdk-demo -- approval-callback
//!   cargo run -p headless-sdk-demo -- cancel-run

use std::sync::Arc;
use std::time::Duration;

use crom_sdk::{
    fake_provider::{EchoFakeProvider, ScriptedFakeProvider},
    AgentMode, AgentRequest, AgentRuntime, AllowAllApprovals, ApprovalPolicy, BuiltinToolSet,
    InMemoryWorkspace, PatchPolicy, ProviderConfig, SideEffectPolicy, ToolPolicy, WorkspaceSource,
};
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "simple-patch".into());
    match cmd.as_str() {
        "simple-patch" => simple_patch().await,
        "in-memory" => in_memory().await,
        "plan-act" => plan_act().await,
        "fake-provider" => fake_provider_only().await,
        "approval-callback" => approval_callback().await,
        "cancel-run" => cancel_run().await,
        other => {
            eprintln!(
                "unknown subcommand: {other}\navailable: simple-patch, in-memory, plan-act, fake-provider, approval-callback, cancel-run"
            );
            std::process::exit(2);
        }
    }
}

async fn simple_patch() -> anyhow::Result<()> {
    // Set up an in-memory workspace that mimics a tiny Rust project.
    let workspace = InMemoryWorkspace::from_pairs(vec![
        (
            "Cargo.toml",
            include_bytes!("../fixtures/simple-rust-project/Cargo.toml").to_vec(),
        ),
        (
            "src/lib.rs",
            include_bytes!("../fixtures/simple-rust-project/src/lib.rs").to_vec(),
        ),
    ])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let result = runtime
        .run_once(
            AgentRequest::new("Describe the project")
                .mode(AgentMode::GeneratePatch)
                .patch_policy(PatchPolicy::ReturnOnly),
        )
        .await?;
    println!("status: {:?}", result.status);
    println!("summary: {}", result.summary);
    if let Some(patch) = &result.patch {
        println!("patch summary: {:#?}", patch.summary);
    }
    Ok(())
}

async fn in_memory() -> anyhow::Result<()> {
    let workspace = InMemoryWorkspace::from_pairs(vec![("README.md", b"old\n".to_vec())])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let result = runtime
        .run_once(AgentRequest::new("annotate README"))
        .await?;
    println!("in-memory run completed with status {:?}", result.status);
    Ok(())
}

async fn plan_act() -> anyhow::Result<()> {
    use crom_sdk::SdkPlanRequest;
    let workspace = InMemoryWorkspace::from_pairs(vec![("README.md", b"hi\n".to_vec())])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .build()
        .await?;
    let plan = runtime
        .plan(SdkPlanRequest::new("Add a TOC to README"))
        .await?;
    println!(
        "plan {} has {} steps",
        plan.plan.id, plan.plan.steps.len()
    );
    Ok(())
}

async fn fake_provider_only() -> anyhow::Result<()> {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await?;
    let result = runtime
        .run_once(AgentRequest::new("hello"))
        .await?;
    println!("response: {}", result.summary);
    Ok(())
}

async fn approval_callback() -> anyhow::Result<()> {
    let provider = ScriptedFakeProvider::one_shot_tool_call(
        "Read",
        serde_json::json!({"file_path": "Cargo.toml"}),
    );
    let workspace = InMemoryWorkspace::from_pairs(vec![
        ("Cargo.toml", b"[package]\nname = \"demo\"\n".to_vec()),
    ])?;
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(provider)))
        .workspace(WorkspaceSource::in_memory(workspace))
        .tool_policy(ToolPolicy {
            enable_builtin_tools: BuiltinToolSet::read_only_defaults(),
            side_effect_policy: SideEffectPolicy::read_only(),
            ..Default::default()
        })
        .approval_callback(Arc::new(AllowAllApprovals))
        .build()
        .await?;
    let result = runtime
        .run_once(
            AgentRequest::new("read Cargo.toml")
                .approval_policy(ApprovalPolicy::CallbackRequired),
        )
        .await?;
    println!("approval-callback demo status: {:?}", result.status);
    println!("events_summary: {:#?}", result.events_summary);
    Ok(())
}

async fn cancel_run() -> anyhow::Result<()> {
    let runtime = AgentRuntime::builder()
        .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
        .build()
        .await?;
    let mut handle = runtime
        .run(AgentRequest::new("a slow task").deadline(Duration::from_secs(30)))
        .await?;
    let cancellation = handle.cancellation.clone();
    // Cancel after a few events flow.
    cancellation.cancel();
    // Drain to terminal state.
    while handle.events.next().await.is_some() {}
    let result = handle.final_result().await?;
    println!("cancel-run final status: {:?}", result.status);
    Ok(())
}
