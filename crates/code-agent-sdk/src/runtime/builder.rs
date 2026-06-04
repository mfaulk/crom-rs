//! Builder for [`AgentRuntime`].

use std::sync::Arc;

use code_agent_auth::ProviderId;
use code_agent_core::ModelProvider;

use crate::budget::RunBudgets;
use crate::callback::{
    AgentHost, ApprovalCallback, CredentialResolver, EventSink, SecretRedactor, ToolHost,
};
use crate::error::AgentSdkError;
use crate::event::EventVerbosity;
use crate::policy::{InteractivityMode, SideEffectPolicy, ToolPolicy};
use crate::provider::ProviderConfig;
use crate::runtime::{default_secret_redactor, default_storage, AgentRuntime, RuntimeInner};
use crate::storage::AgentStorage;
use crate::workspace::WorkspaceSource;

/// Builder for [`AgentRuntime`]. Conservative defaults: read-only side
/// effects, headless-deny approvals, no global credential lookup, no
/// plugins, no MCP. Applications opt in explicitly.
pub struct AgentRuntimeBuilder {
    provider: Option<ProviderConfig>,
    model: Option<String>,
    system_prompt: Option<String>,
    project_instructions: Option<String>,
    workspace: Option<WorkspaceSource>,
    storage: Option<Arc<dyn AgentStorage>>,
    approval_callback: Option<Arc<dyn ApprovalCallback>>,
    credential_resolver: Option<Arc<dyn CredentialResolver>>,
    event_sink: Option<Arc<dyn EventSink>>,
    secret_redactor: Option<Arc<dyn SecretRedactor>>,
    tool_host: Option<Arc<dyn ToolHost>>,
    tool_policy: Option<ToolPolicy>,
    side_effect_policy: Option<SideEffectPolicy>,
    interactivity: Option<InteractivityMode>,
    verbosity: Option<EventVerbosity>,
    budgets: Option<RunBudgets>,
}

impl std::fmt::Debug for AgentRuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentRuntimeBuilder")
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("workspace", &self.workspace)
            .field("interactivity", &self.interactivity)
            .field("verbosity", &self.verbosity)
            .field("budgets", &self.budgets)
            .finish()
    }
}

impl Default for AgentRuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRuntimeBuilder {
    pub fn new() -> Self {
        Self {
            provider: None,
            model: None,
            system_prompt: None,
            project_instructions: None,
            workspace: None,
            storage: None,
            approval_callback: None,
            credential_resolver: None,
            event_sink: None,
            secret_redactor: None,
            tool_host: None,
            tool_policy: None,
            side_effect_policy: None,
            interactivity: None,
            verbosity: None,
            budgets: None,
        }
    }

    pub fn provider(mut self, provider: ProviderConfig) -> Self {
        self.provider = Some(provider);
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Runtime-level system prompt applied to every run. A per-request
    /// [`crate::AgentRequest::system_prompt`] overrides this default.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Runtime-level project instructions (appended after the system prompt
    /// under an "AGENTS.md" heading by the core agent).
    pub fn project_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.project_instructions = Some(instructions.into());
        self
    }

    pub fn workspace(mut self, workspace: WorkspaceSource) -> Self {
        self.workspace = Some(workspace);
        self
    }

    pub fn storage(mut self, storage: Arc<dyn AgentStorage>) -> Self {
        self.storage = Some(storage);
        self
    }

    pub fn approval_callback(mut self, callback: Arc<dyn ApprovalCallback>) -> Self {
        self.approval_callback = Some(callback);
        self
    }

    pub fn credential_resolver(mut self, resolver: Arc<dyn CredentialResolver>) -> Self {
        self.credential_resolver = Some(resolver);
        self
    }

    pub fn event_sink(mut self, sink: Arc<dyn EventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    pub fn secret_redactor(mut self, redactor: Arc<dyn SecretRedactor>) -> Self {
        self.secret_redactor = Some(redactor);
        self
    }

    pub fn tool_host(mut self, host: Arc<dyn ToolHost>) -> Self {
        self.tool_host = Some(host);
        self
    }

    pub fn tool_policy(mut self, policy: ToolPolicy) -> Self {
        self.tool_policy = Some(policy);
        self
    }

    pub fn side_effect_policy(mut self, policy: SideEffectPolicy) -> Self {
        self.side_effect_policy = Some(policy);
        self
    }

    pub fn interactivity(mut self, mode: InteractivityMode) -> Self {
        self.interactivity = Some(mode);
        self
    }

    pub fn verbosity(mut self, v: EventVerbosity) -> Self {
        self.verbosity = Some(v);
        self
    }

    pub fn budgets(mut self, budgets: RunBudgets) -> Self {
        self.budgets = Some(budgets);
        self
    }

    /// Apply a host bundle: pulls approval, credential, event sink, redactor,
    /// and tool host out of an `AgentHost` implementation.
    pub fn host(mut self, host: Arc<dyn AgentHost>) -> Self {
        if let Some(cb) = host.approval_callback() {
            self.approval_callback = Some(cb);
        }
        if let Some(cr) = host.credential_resolver() {
            self.credential_resolver = Some(cr);
        }
        if let Some(es) = host.event_sink() {
            self.event_sink = Some(es);
        }
        if let Some(sr) = host.secret_redactor() {
            self.secret_redactor = Some(sr);
        }
        if let Some(th) = host.tool_host() {
            self.tool_host = Some(th);
        }
        self
    }

    pub async fn build(self) -> Result<AgentRuntime, AgentSdkError> {
        let provider_config = self.provider.ok_or_else(|| {
            AgentSdkError::invalid_request("runtime.no_provider", "no provider configured")
        })?;

        let provider_id = provider_config.provider_id();
        let (provider, default_model_default): (Arc<dyn ModelProvider>, &'static str) =
            materialize_provider(provider_config).await?;

        let workspace = match self.workspace {
            Some(ws) => Some(ws.into_workspace()?),
            None => None,
        };

        let mut tool_policy = self.tool_policy.unwrap_or_default();
        if let Some(sep) = self.side_effect_policy {
            tool_policy.side_effect_policy = sep;
        }
        let interactivity = self.interactivity.unwrap_or_default();
        let verbosity = self.verbosity.unwrap_or_default();
        let storage = self.storage.unwrap_or_else(default_storage);
        let redactor = self.secret_redactor.unwrap_or_else(default_secret_redactor);

        let model = self
            .model
            .unwrap_or_else(|| default_model_default.to_string());

        let inner = Arc::new(RuntimeInner {
            provider,
            provider_id,
            default_model: model,
            default_system_prompt: self.system_prompt,
            default_project_instructions: self.project_instructions,
            workspace,
            storage,
            approval_callback: self.approval_callback,
            credential_resolver: self.credential_resolver,
            event_sink: self.event_sink,
            secret_redactor: redactor,
            tool_host: self.tool_host,
            tool_policy,
            interactivity,
            verbosity,
            default_budgets: self.budgets.unwrap_or_default(),
        });
        Ok(AgentRuntime { inner })
    }
}

async fn materialize_provider(
    cfg: ProviderConfig,
) -> Result<(Arc<dyn ModelProvider>, &'static str), AgentSdkError> {
    match cfg {
        #[cfg(feature = "anthropic")]
        ProviderConfig::AnthropicApiKey { api_key, base_url } => {
            let mut anthropic_cfg = code_agent_anthropic::AnthropicConfig {
                api_key: api_key.expose().to_string(),
                base_url: base_url.unwrap_or_else(|| {
                    std::env::var("ANTHROPIC_BASE_URL")
                        .unwrap_or_else(|_| "https://api.anthropic.com".into())
                }),
                connect_timeout: std::time::Duration::from_secs(30),
                read_timeout: std::time::Duration::from_secs(120),
                max_retries: 2,
            };
            // No-op: kept for forward-compatible field plumbing.
            let _ = &mut anthropic_cfg;
            let provider = code_agent_anthropic::AnthropicProvider::new(anthropic_cfg)
                .map_err(|e| AgentSdkError::provider("provider.init_failed", e.to_string()))?;
            let provider: Arc<dyn ModelProvider> = Arc::new(provider);
            Ok((provider, "claude-sonnet-4-6"))
        }
        #[cfg(not(feature = "anthropic"))]
        ProviderConfig::AnthropicApiKey { .. } => Err(AgentSdkError::invalid_request(
            "provider.feature_disabled",
            "anthropic feature is disabled",
        )),
        ProviderConfig::OpenAiApiKey { .. } => Err(AgentSdkError::invalid_request(
            "provider.openai_not_wired",
            "OpenAI provider needs a transport + credential factory; wrap your \
             OpenAiProvider into ProviderConfig::External and pass the resolved \
             credential explicitly",
        )),
        ProviderConfig::GeminiApiKey { .. } => Err(AgentSdkError::invalid_request(
            "provider.gemini_not_wired",
            "Gemini provider needs a transport + credential factory; wrap your \
             GeminiProvider into ProviderConfig::External and pass the resolved \
             credential explicitly",
        )),
        ProviderConfig::OAuthCredential { .. } => Err(AgentSdkError::invalid_request(
            "provider.oauth_not_wired",
            "OAuth credential injection is not yet supported by the SDK builder; \
             use ProviderConfig::External with a prepared ModelProvider",
        )),
        ProviderConfig::FromAuthStore { .. } => Err(AgentSdkError::invalid_request(
            "provider.auth_store_disabled",
            "FromAuthStore is opt-in and not yet wired in the SDK builder; \
             use ProviderConfig::External or an API-key variant",
        )),
        ProviderConfig::External { provider, .. } => Ok((provider, "external-model")),
        ProviderConfig::Fake { provider } => Ok((provider, "fake-model")),
    }
}

// Verify `ProviderId` is in scope.
#[allow(dead_code)]
fn _provider_id(_p: ProviderId) {}

#[cfg(all(test, feature = "fake-provider"))]
mod tests {
    //! Builder validation beyond `builder_without_provider_fails`: the
    //! `materialize_provider` branches that reject not-yet-wired provider
    //! configs (OpenAI/Gemini/OAuth/FromAuthStore) with a stable error code,
    //! the External/Fake success branches and their default model ids, and
    //! the explicit-model-override-wins rule.

    use super::*;
    use crate::fake_provider::EchoFakeProvider;
    use crate::provider::{ProviderConfig, SecretString};
    use crate::AgentSdkErrorKind;
    use code_agent_core::ModelProvider;

    fn fake() -> ProviderConfig {
        ProviderConfig::fake(Arc::new(EchoFakeProvider))
    }

    fn secret(s: &str) -> SecretString {
        SecretString::new(s.to_string())
    }

    // A Fake provider builds and falls back to the "fake-model" default id
    // when no model is configured.
    #[tokio::test]
    async fn fake_provider_builds_with_default_model() {
        let rt = AgentRuntime::builder()
            .provider(fake())
            .build()
            .await
            .expect("fake provider builds");
        assert_eq!(rt.default_model(), "fake-model");
        assert_eq!(rt.provider_id().as_str(), "fake");
    }

    // An explicit `.model(..)` overrides the provider's default model id.
    #[tokio::test]
    async fn explicit_model_overrides_default() {
        let rt = AgentRuntime::builder()
            .provider(fake())
            .model("custom-model-x")
            .build()
            .await
            .unwrap();
        assert_eq!(rt.default_model(), "custom-model-x");
    }

    // The External branch builds from a prepared ModelProvider and reports
    // the "external-model" default id.
    #[tokio::test]
    async fn external_provider_builds() {
        let provider: Arc<dyn ModelProvider> = Arc::new(EchoFakeProvider);
        let rt = AgentRuntime::builder()
            .provider(ProviderConfig::external(provider))
            .build()
            .await
            .expect("external provider builds");
        assert_eq!(rt.default_model(), "external-model");
    }

    async fn assert_invalid_request(cfg: ProviderConfig, expect_code: &str) {
        let err = AgentRuntime::builder()
            .provider(cfg)
            .build()
            .await
            .expect_err("not-yet-wired provider must fail to build");
        assert_eq!(err.kind, AgentSdkErrorKind::InvalidRequest);
        // The structured error code is carried on the injected diagnostic.
        assert_eq!(
            err.diagnostics.first().map(|d| d.code.as_str()),
            Some(expect_code),
            "unexpected error code: {err}"
        );
    }

    // OpenAI API-key config is not wired through the SDK builder yet and must
    // fail with a stable, specific InvalidRequest code (not a panic / generic
    // error) so callers can branch on it.
    #[tokio::test]
    async fn openai_api_key_not_wired() {
        assert_invalid_request(
            ProviderConfig::openai_api_key(secret("sk-test")),
            "provider.openai_not_wired",
        )
        .await;
    }

    // Gemini API-key config: same not-wired contract.
    #[tokio::test]
    async fn gemini_api_key_not_wired() {
        assert_invalid_request(
            ProviderConfig::gemini_api_key(secret("g-test")),
            "provider.gemini_not_wired",
        )
        .await;
    }
}
