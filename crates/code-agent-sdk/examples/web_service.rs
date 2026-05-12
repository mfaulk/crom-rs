//! Sketch of an Axum-style web service that wraps the SDK runtime. This
//! example deliberately omits the HTTP layer so it stays runnable without
//! adding HTTP dependencies; the structure shows where the SDK plugs in.

use std::sync::Arc;

use code_agent_sdk::fake_provider::EchoFakeProvider;
use code_agent_sdk::{
    AgentMode, AgentRequest, AgentRuntime, InMemoryWorkspace, PatchPolicy, ProviderConfig,
    WorkspaceSource,
};

#[derive(serde::Serialize)]
#[allow(dead_code)]
struct CodeGenerationResponse {
    summary: String,
    patch: Option<code_agent_sdk::PatchSet>,
    diagnostics: Vec<code_agent_sdk::Diagnostic>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Build the runtime once and reuse it across requests.
    let runtime = Arc::new(
        AgentRuntime::builder()
            .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider)))
            .build()
            .await?,
    );

    // Simulate a single request.
    let user_request = "describe the project";
    let files = vec![("README.md", b"hello\n".to_vec())];
    let workspace = InMemoryWorkspace::from_pairs(files)?;
    let result = runtime
        .run_once(
            AgentRequest::new(user_request)
                .workspace(WorkspaceSource::in_memory(workspace))
                .mode(AgentMode::GeneratePatch)
                .patch_policy(PatchPolicy::ReturnOnly)
                .tenant_id("tenant-1")
                .user_id("user-42")
                .request_id("req-abc"),
        )
        .await?;
    let response = CodeGenerationResponse {
        summary: result.summary,
        patch: result.patch,
        diagnostics: result.diagnostics,
    };
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}
