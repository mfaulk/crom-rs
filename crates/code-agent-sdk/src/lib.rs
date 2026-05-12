//! Embeddable headless coding-agent SDK.
//!
//! `code-agent-sdk` exposes a stable, typed Rust API that applications can
//! use to generate code, produce patches, apply changes, run validation,
//! and stream progress events without depending on the CLI, TUI, terminal
//! prompts, CLI flags, global credentials, or filesystem settings files.
//!
//! # Defaults
//!
//! The default SDK configuration is conservative:
//!
//! - read-only side-effect policy (no Bash, no Edit/Write)
//! - `InteractivityMode::HeadlessDeny` (no approval prompts)
//! - no global credential lookup
//! - no plugin/MCP/skill loading
//! - in-memory or no-op storage
//! - patch-only output (`PatchPolicy::ReturnOnly`)
//!
//! Applications opt in to side effects, callbacks, custom tools, and
//! storage explicitly through [`AgentRuntimeBuilder`].
//!
//! # Quick start
//!
//! ```no_run
//! # #[cfg(feature = "fake-provider")]
//! # async fn run() -> Result<(), code_agent_sdk::AgentSdkError> {
//! use code_agent_sdk::{
//!     AgentRequest, AgentRuntime, ProviderConfig, WorkspaceSource,
//!     fake_provider::EchoFakeProvider,
//! };
//! use std::sync::Arc;
//!
//! let runtime = AgentRuntime::builder()
//!     .provider(ProviderConfig::fake(Arc::new(EchoFakeProvider::default())))
//!     .build()
//!     .await?;
//! let result = runtime
//!     .run_once(AgentRequest::new("describe the workspace"))
//!     .await?;
//! println!("summary = {}", result.summary);
//! # Ok(())
//! # }
//! ```

// `missing_debug_implementations` is intentionally not warned for public types
// that wrap trait objects (`Arc<dyn Workspace>` etc.) — most have manual or
// derived Debug where it adds signal, otherwise a Debug impl would have to
// fabricate output that does not match the underlying object.

pub mod budget;
pub mod callback;
pub mod error;
pub mod event;
pub mod patch;
pub mod plan;
pub mod policy;
pub mod provider;
pub mod request;
pub mod result;
pub mod runtime;
pub mod storage;
pub mod workspace;

#[cfg(feature = "fake-provider")]
pub mod fake_provider;

pub use code_agent_core::options::SessionId;
pub use code_agent_core::run_id::RunId;
pub use code_agent_core::TokenUsage;

pub use budget::{CancellationHandle, RunBudgets};
pub use callback::{
    AgentHost, AllowAllApprovals, ApprovalCallback, ApprovalDecision, ApprovalRequest,
    CredentialRequest, CredentialResolver, DefaultSecretRedactor, DenyAllApprovals, EventSink,
    HostToolRequest, NoopSecretRedactor, SecretRedactor, ToolHost,
};
pub use error::{
    AgentSdkError, AgentSdkErrorKind, Diagnostic, DiagnosticLocation, DiagnosticSeverity,
};
pub use event::{EventVerbosity, SdkEvent};
pub use patch::{
    CodeArtifacts, CommandSuggestion, FilePatch, GeneratedFile, PatchSet, PatchSummary,
};
pub use plan::{ActRequest, PlanRequest as SdkPlanRequest, PlanResult};
pub use policy::{
    BuiltinToolSet, InteractivityMode, SideEffectLevel, SideEffectPolicy, ToolLoadingMode,
    ToolPolicy,
};
pub use provider::{ProviderConfig, ProviderSelection, SecretString};
pub use request::{
    AgentMode, AgentRequest, ApprovalPolicy, PatchPolicy, PlanPolicy, RequestMetadata,
    SessionPolicy, ValidationPolicy,
};
pub use result::{AgentRunResult, AgentRunStatus, EventSummary, ValidationReport};
pub use runtime::{AgentRun, AgentRuntime, AgentRuntimeBuilder};
pub use storage::{
    AgentStorage, FileStorage, InMemoryStorage, NoopStorage, SessionSnapshot, StorageError,
};
pub use workspace::{
    ApplyPatchReport, DiffBase, FileContent, FileQuery, FilesystemWorkspace, InMemoryWorkspace,
    OverlayWorkspace, PatchOnlyWorkspace, ReadOnlyWorkspace, SearchMatch, SearchQuery, Workspace,
    WorkspaceCapabilities, WorkspaceError, WorkspacePath, WorkspaceSource,
};

// Re-export commonly needed core/auth types so applications can avoid pulling
// the underlying crates into their Cargo.toml when they only want the public
// SDK surface.
pub use code_agent_auth::ProviderId;
pub use code_agent_core::permission::{PermissionKind, PermissionMode};
pub use code_agent_core::plan::{PlanDocument, PlanId, PlanStatus, PlanningMode, PlanningOptions};
pub use code_agent_core::{ModelProvider, Tool, ToolContext, ToolRegistry, ToolResult};
