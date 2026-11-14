//! Unified model registry across providers.

use serde::{Deserialize, Serialize};

use crate::types::{BillingMode, ProviderId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub provider: ProviderId,
    pub model_id: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supports_tools: bool,
    #[serde(default)]
    pub supports_streaming: bool,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub supports_reasoning: bool,
    #[serde(default)]
    pub supports_native_tool_search: bool,
    pub availability: ModelAvailability,
    #[serde(default)]
    pub billing_mode: BillingMode,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ModelAvailability {
    Available,
    RequiresApiKey,
    RequiresSubscription,
    RequiresPlan(String),
    Unavailable { reason: String },
    Unknown,
}

impl ModelAvailability {
    pub fn label(&self) -> String {
        match self {
            Self::Available => "available".into(),
            Self::RequiresApiKey => "requires API key".into(),
            Self::RequiresSubscription => "requires subscription".into(),
            Self::RequiresPlan(p) => format!("requires plan: {p}"),
            Self::Unavailable { reason } => format!("unavailable: {reason}"),
            Self::Unknown => "unknown".into(),
        }
    }
}

/// Cross-provider model registry. Keeps a flat list of [`ModelInfo`] and
/// supports filtering by provider.
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    pub models: Vec<ModelInfo>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn extend(&mut self, models: impl IntoIterator<Item = ModelInfo>) {
        self.models.extend(models);
    }

    pub fn for_provider(&self, provider: &ProviderId) -> Vec<&ModelInfo> {
        self.models
            .iter()
            .filter(|m| &m.provider == provider)
            .collect()
    }

    pub fn find(&self, provider: &ProviderId, model_id: &str) -> Option<&ModelInfo> {
        self.models
            .iter()
            .find(|m| &m.provider == provider && m.model_id == model_id)
    }
}

/// Built-in starter set: one representative model per provider so the
/// `model list` CLI has something useful to show in offline tests/demos.
pub fn starter_registry() -> ModelRegistry {
    let mut reg = ModelRegistry::new();
    reg.extend([
        ModelInfo {
            provider: ProviderId::Anthropic,
            model_id: "claude-sonnet-4-6".into(),
            display_name: "Claude Sonnet 4.6".into(),
            context_window: Some(200_000),
            supports_tools: true,
            supports_streaming: true,
            supports_images: true,
            supports_reasoning: true,
            supports_native_tool_search: true,
            availability: ModelAvailability::RequiresApiKey,
            billing_mode: BillingMode::ApiUsageBased,
        },
        ModelInfo {
            provider: ProviderId::OpenAi,
            model_id: "gpt-4.1".into(),
            display_name: "GPT-4.1".into(),
            context_window: Some(128_000),
            supports_tools: true,
            supports_streaming: true,
            supports_images: true,
            supports_reasoning: false,
            supports_native_tool_search: false,
            availability: ModelAvailability::RequiresApiKey,
            billing_mode: BillingMode::ApiUsageBased,
        },
        ModelInfo {
            provider: ProviderId::Codex,
            model_id: "gpt-codex".into(),
            display_name: "GPT Codex".into(),
            context_window: Some(128_000),
            supports_tools: true,
            supports_streaming: true,
            supports_images: false,
            supports_reasoning: true,
            supports_native_tool_search: false,
            availability: ModelAvailability::RequiresSubscription,
            billing_mode: BillingMode::SubscriptionIncluded,
        },
        ModelInfo {
            provider: ProviderId::Gemini,
            model_id: "gemini-1.5-pro".into(),
            display_name: "Gemini 1.5 Pro".into(),
            context_window: Some(1_000_000),
            supports_tools: true,
            supports_streaming: true,
            supports_images: true,
            supports_reasoning: false,
            supports_native_tool_search: false,
            availability: ModelAvailability::RequiresApiKey,
            billing_mode: BillingMode::ApiUsageBased,
        },
        ModelInfo {
            provider: ProviderId::Fake,
            model_id: "fake-coding-model".into(),
            display_name: "Fake Coding Model".into(),
            context_window: Some(8192),
            supports_tools: true,
            supports_streaming: true,
            supports_images: false,
            supports_reasoning: false,
            supports_native_tool_search: false,
            availability: ModelAvailability::Available,
            billing_mode: BillingMode::ApiUsageBased,
        },
    ]);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // Starter registry exposes one model per built-in provider.
    fn starter_registry_includes_each_provider() {
        let reg = starter_registry();
        for p in [
            ProviderId::Anthropic,
            ProviderId::OpenAi,
            ProviderId::Codex,
            ProviderId::Gemini,
            ProviderId::Fake,
        ] {
            assert!(reg.find(&p, &reg.for_provider(&p)[0].model_id).is_some());
        }
    }

    #[test]
    // Codex starter model is marked subscription-backed; Anthropic/OpenAI/Gemini are API-usage-based.
    fn billing_modes_are_correctly_attributed() {
        let reg = starter_registry();
        let codex = reg.find(&ProviderId::Codex, "gpt-codex").unwrap();
        assert!(codex.billing_mode.is_subscription_backed());
        let anthropic = reg
            .find(&ProviderId::Anthropic, "claude-sonnet-4-6")
            .unwrap();
        assert_eq!(anthropic.billing_mode, BillingMode::ApiUsageBased);
    }

    #[test]
    // Availability labels render to non-empty human strings.
    fn availability_labels_render() {
        assert_eq!(ModelAvailability::Available.label(), "available");
        assert!(ModelAvailability::Unavailable { reason: "x".into() }
            .label()
            .contains("unavailable"));
    }
}
