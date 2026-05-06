use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use globset::Glob;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::Mutex;

use crate::errors::ToolError;
use crate::permission::{PermissionKind, PermissionMode};
use crate::policy::ApprovalMemorySnapshot;
use crate::tool::{Tool, ToolContent, ToolContext, ToolRegistry, ToolResult};
use crate::tool_rule::ToolRule;

pub const SEARCH_TOOLS_NAME: &str = "SearchTools";
const DEFAULT_REGEX_QUERY_LIMIT: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptor {
    pub name: String,
    pub source: ToolSource,
    pub description: String,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    pub permission_kind: PermissionKind,
    pub risk_profile: ToolRiskProfile,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub provider_metadata: Value,
    pub estimated_tokens: usize,
    pub load_policy: ToolLoadPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolSource {
    Local,
    Mcp {
        server_name: String,
        remote_tool_name: String,
        transport: code_agent_mcp::McpTransportKind,
    },
    Custom {
        namespace: String,
    },
}

impl ToolSource {
    pub fn label(&self) -> String {
        match self {
            Self::Local => "local".to_string(),
            Self::Mcp { server_name, .. } => format!("mcp:{server_name}"),
            Self::Custom { namespace } => format!("custom:{namespace}"),
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Mcp { .. } => 1,
            Self::Custom { .. } => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadPolicy {
    Always,
    Deferred,
    Disabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolRiskProfile {
    Low,
    Medium,
    High,
    External,
    MutatingExternal,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadingMode {
    Eager,
    Search,
    Auto {
        threshold_percent_of_context: u8,
        min_tool_count: usize,
    },
}

impl Default for ToolLoadingMode {
    fn default() -> Self {
        Self::Auto {
            threshold_percent_of_context: 10,
            min_tool_count: 10,
        }
    }
}

impl ToolLoadingMode {
    pub fn from_str_lenient(
        s: &str,
        threshold_percent_of_context: u8,
        min_tool_count: usize,
    ) -> Option<Self> {
        match normalized(s).as_str() {
            "eager" => Some(Self::Eager),
            "search" => Some(Self::Search),
            "auto" => Some(Self::Auto {
                threshold_percent_of_context,
                min_tool_count,
            }),
            _ => None,
        }
    }
}

impl std::fmt::Display for ToolLoadingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eager => f.write_str("eager"),
            Self::Search => f.write_str("search"),
            Self::Auto { .. } => f.write_str("auto"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedToolLoadingMode {
    Eager,
    Search,
}

impl std::fmt::Display for ResolvedToolLoadingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Eager => f.write_str("eager"),
            Self::Search => f.write_str("search"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSearchStrategy {
    Regex,
    Bm25,
    #[default]
    Hybrid,
}

impl ToolSearchStrategy {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "regex" => Some(Self::Regex),
            "bm25" => Some(Self::Bm25),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

impl std::fmt::Display for ToolSearchStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Regex => f.write_str("regex"),
            Self::Bm25 => f.write_str("bm25"),
            Self::Hybrid => f.write_str("hybrid"),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeToolSearchMode {
    Off,
    AnthropicRegex,
    AnthropicBm25,
    #[default]
    Auto,
}

impl NativeToolSearchMode {
    pub fn from_str_lenient(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "off" => Some(Self::Off),
            "anthropicregex" => Some(Self::AnthropicRegex),
            "anthropicbm25" => Some(Self::AnthropicBm25),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }
}

impl std::fmt::Display for NativeToolSearchMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => f.write_str("off"),
            Self::AnthropicRegex => f.write_str("anthropic_regex"),
            Self::AnthropicBm25 => f.write_str("anthropic_bm25"),
            Self::Auto => f.write_str("auto"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSearchOptions {
    pub enabled: bool,
    pub mode: ToolLoadingMode,
    pub strategy: ToolSearchStrategy,
    pub native_provider_mode: NativeToolSearchMode,
    pub max_results: usize,
    pub max_loaded_tools: usize,
    pub max_loaded_tool_tokens: usize,
    pub always_loaded: Vec<String>,
    pub deferred: Vec<String>,
    pub disabled: Vec<String>,
    pub include_schema_arguments: bool,
    pub include_tool_category_hints: bool,
    pub persist_discovered_tools: bool,
    pub regex_query_limit: usize,
    pub available_context_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preloaded_discovered_tools: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ToolSearchSettingsConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub native_provider_mode: Option<String>,
    #[serde(default)]
    pub activation_threshold_percent: Option<u8>,
    #[serde(default)]
    pub min_tool_count: Option<usize>,
    #[serde(default)]
    pub max_results: Option<usize>,
    #[serde(default)]
    pub max_loaded_tools: Option<usize>,
    #[serde(default)]
    pub max_loaded_tool_tokens: Option<usize>,
    #[serde(default)]
    pub always_loaded: Option<Vec<String>>,
    #[serde(default)]
    pub deferred: Option<Vec<String>>,
    #[serde(default)]
    pub disabled: Option<Vec<String>>,
    #[serde(default)]
    pub include_schema_arguments: Option<bool>,
    #[serde(default)]
    pub include_tool_category_hints: Option<bool>,
    #[serde(default)]
    pub persist_discovered_tools: Option<bool>,
}

impl Default for ToolSearchOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ToolLoadingMode::default(),
            strategy: ToolSearchStrategy::Hybrid,
            native_provider_mode: NativeToolSearchMode::Auto,
            max_results: 5,
            max_loaded_tools: 20,
            max_loaded_tool_tokens: 12_000,
            always_loaded: vec![
                "Read".into(),
                "Glob".into(),
                "Grep".into(),
                "Edit".into(),
                "Write".into(),
                "Bash".into(),
                "TodoWrite".into(),
                SEARCH_TOOLS_NAME.into(),
            ],
            deferred: vec!["mcp__*".into()],
            disabled: Vec::new(),
            include_schema_arguments: true,
            include_tool_category_hints: true,
            persist_discovered_tools: true,
            regex_query_limit: DEFAULT_REGEX_QUERY_LIMIT,
            available_context_tokens: Some(200_000),
            preloaded_discovered_tools: Vec::new(),
        }
    }
}

impl ToolSearchOptions {
    pub fn apply_settings(&mut self, settings: &ToolSearchSettingsConfig) -> Result<(), String> {
        if let Some(enabled) = settings.enabled {
            self.enabled = enabled;
        }
        if let Some(strategy) = &settings.strategy {
            self.strategy = ToolSearchStrategy::from_str_lenient(strategy)
                .ok_or_else(|| format!("invalid tool_search.strategy: {strategy}"))?;
        }
        if let Some(native) = &settings.native_provider_mode {
            self.native_provider_mode = NativeToolSearchMode::from_str_lenient(native)
                .ok_or_else(|| format!("invalid tool_search.native_provider_mode: {native}"))?;
        }
        if let Some(max_results) = settings.max_results {
            self.max_results = max_results.max(1);
        }
        if let Some(max_loaded_tools) = settings.max_loaded_tools {
            self.max_loaded_tools = max_loaded_tools.max(1);
        }
        if let Some(max_loaded_tool_tokens) = settings.max_loaded_tool_tokens {
            self.max_loaded_tool_tokens = max_loaded_tool_tokens.max(1);
        }
        if let Some(always_loaded) = &settings.always_loaded {
            self.always_loaded = always_loaded.clone();
        }
        if let Some(deferred) = &settings.deferred {
            self.deferred = deferred.clone();
        }
        if let Some(disabled) = &settings.disabled {
            self.disabled = disabled.clone();
        }
        if let Some(include) = settings.include_schema_arguments {
            self.include_schema_arguments = include;
        }
        if let Some(include) = settings.include_tool_category_hints {
            self.include_tool_category_hints = include;
        }
        if let Some(persist) = settings.persist_discovered_tools {
            self.persist_discovered_tools = persist;
        }
        if let Some(mode) = &settings.mode {
            self.set_mode_from_parts(
                mode,
                settings.activation_threshold_percent,
                settings.min_tool_count,
            )?;
        } else if settings.activation_threshold_percent.is_some()
            || settings.min_tool_count.is_some()
        {
            self.mode = ToolLoadingMode::Auto {
                threshold_percent_of_context: settings
                    .activation_threshold_percent
                    .unwrap_or(self.activation_threshold_percent()),
                min_tool_count: settings.min_tool_count.unwrap_or(self.min_tool_count()),
            };
        }
        Ok(())
    }

    pub fn set_mode_from_parts(
        &mut self,
        mode: &str,
        threshold_percent_of_context: Option<u8>,
        min_tool_count: Option<usize>,
    ) -> Result<(), String> {
        let threshold = threshold_percent_of_context.unwrap_or(self.activation_threshold_percent());
        let min_count = min_tool_count.unwrap_or(self.min_tool_count());
        self.mode = ToolLoadingMode::from_str_lenient(mode, threshold, min_count)
            .ok_or_else(|| format!("invalid tool_search.mode: {mode}"))?;
        Ok(())
    }

    pub fn activation_threshold_percent(&self) -> u8 {
        match self.mode {
            ToolLoadingMode::Auto {
                threshold_percent_of_context,
                ..
            } => threshold_percent_of_context,
            _ => 10,
        }
    }

    pub fn min_tool_count(&self) -> usize {
        match self.mode {
            ToolLoadingMode::Auto { min_tool_count, .. } => min_tool_count,
            _ => 10,
        }
    }

    /// Build a `ToolVisibilityContext` for the runtime.
    ///
    /// `allowed_tool_names` is the simple-name allowlist (the `Vec<String>`
    /// equivalent of `AgentOptions::allowed_tools`). It must be passed in
    /// because `dontAsk` mode treats the simple-name list as an implicit
    /// allowlist; deriving names only from `ExactTool` rules would hide
    /// tools that are actually permitted at runtime.
    ///
    /// `mcp_external_effect_policy` is the effective runtime policy from
    /// `McpOptions::external_effect_policy`. Hardcoding a fixed value here
    /// would make `SearchTools` mislabel external MCP tools when the user
    /// has selected a non-default policy.
    pub fn visibility_context(
        &self,
        permission_mode: PermissionMode,
        allowed_tools: Vec<ToolRule>,
        disallowed_tools: Vec<ToolRule>,
        allowed_tool_names: Vec<String>,
        mcp_external_effect_policy: code_agent_mcp::McpExternalEffectPolicy,
    ) -> ToolVisibilityContext {
        let mut combined_names = allowed_tool_names;
        for rule in &allowed_tools {
            if let ToolRule::ExactTool(name) = rule {
                if !combined_names.iter().any(|n| n == name) {
                    combined_names.push(name.clone());
                }
            }
        }
        ToolVisibilityContext {
            permission_mode,
            allowed_tools,
            allowed_tool_names: combined_names,
            disallowed_tools,
            approval_memory_summary: ApprovalMemorySnapshot::default(),
            mcp_external_effect_policy: Some(mcp_external_effect_policy),
            reveal_denied_names: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSearchProviderConfig {
    pub native_mode: NativeToolSearchMode,
    pub local_search_enabled: bool,
    pub strategy: ToolSearchStrategy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoadedToolState {
    pub always_loaded: BTreeSet<String>,
    pub discovered_this_run: BTreeSet<String>,
    pub discovered_this_session: BTreeSet<String>,
    pub disabled: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolLoadReason {
    AlwaysLoaded,
    SearchResult,
    UserConfigured,
    ProviderNativeReference,
    SessionPersistence,
    ManualCliLoad,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSearchQuery {
    pub query: String,
    pub limit: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_filter: Option<ToolSourceFilter>,
    pub include_risky: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ToolSourceFilter {
    Local,
    McpServer(String),
    McpAll,
    CustomNamespace(String),
    Any,
}

impl ToolSourceFilter {
    pub fn parse(s: &str) -> Result<Self, String> {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("any") {
            return Ok(Self::Any);
        }
        if trimmed.eq_ignore_ascii_case("local") {
            return Ok(Self::Local);
        }
        if trimmed.eq_ignore_ascii_case("mcp") || trimmed.eq_ignore_ascii_case("mcp:*") {
            return Ok(Self::McpAll);
        }
        if let Some(server) = trimmed.strip_prefix("mcp:") {
            if server.is_empty() {
                return Err("source filter mcp: requires a server name".into());
            }
            return Ok(Self::McpServer(server.to_string()));
        }
        if let Some(namespace) = trimmed.strip_prefix("custom:") {
            if namespace.is_empty() {
                return Err("source filter custom: requires a namespace".into());
            }
            return Ok(Self::CustomNamespace(namespace.to_string()));
        }
        Err(format!("invalid tool source filter: {s}"))
    }

    fn matches(&self, source: &ToolSource) -> bool {
        match self {
            Self::Any => true,
            Self::Local => matches!(source, ToolSource::Local),
            Self::McpAll => matches!(source, ToolSource::Mcp { .. }),
            Self::McpServer(wanted) => match source {
                ToolSource::Mcp { server_name, .. } => server_name == wanted,
                _ => false,
            },
            Self::CustomNamespace(wanted) => match source {
                ToolSource::Custom { namespace } => namespace == wanted,
                _ => false,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSearchResult {
    pub query: String,
    pub matches: Vec<ToolSearchMatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSearchMatch {
    pub tool_name: String,
    pub score: f32,
    pub reason: String,
    pub descriptor_preview: ToolDescriptorPreview,
    pub visibility: ToolVisibility,
    pub loaded: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSearchMatchSummary {
    pub tool_name: String,
    pub score: f32,
    pub source: ToolSource,
    pub visibility: ToolVisibility,
    pub loaded: bool,
}

impl From<&ToolSearchMatch> for ToolSearchMatchSummary {
    fn from(value: &ToolSearchMatch) -> Self {
        Self {
            tool_name: value.tool_name.clone(),
            score: value.score,
            source: value.descriptor_preview.source.clone(),
            visibility: value.visibility,
            loaded: value.loaded,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDescriptorPreview {
    pub name: String,
    pub source: ToolSource,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub permission_kind: PermissionKind,
    pub risk_profile: ToolRiskProfile,
    pub input_properties: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolVisibility {
    Callable,
    RequiresApproval,
    Hidden,
    Denied,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolVisibilityContext {
    pub permission_mode: PermissionMode,
    pub allowed_tools: Vec<ToolRule>,
    #[serde(default)]
    pub allowed_tool_names: Vec<String>,
    pub disallowed_tools: Vec<ToolRule>,
    pub approval_memory_summary: ApprovalMemorySnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_external_effect_policy: Option<code_agent_mcp::McpExternalEffectPolicy>,
    #[serde(default)]
    pub reveal_denied_names: bool,
}

impl Default for ToolVisibilityContext {
    fn default() -> Self {
        Self {
            permission_mode: PermissionMode::Default,
            allowed_tools: Vec::new(),
            allowed_tool_names: Vec::new(),
            disallowed_tools: Vec::new(),
            approval_memory_summary: ApprovalMemorySnapshot::default(),
            mcp_external_effect_policy: Some(
                code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule,
            ),
            reveal_denied_names: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCatalogSummary {
    pub total_tools: usize,
    pub eager_tools: usize,
    pub deferred_tools: usize,
    pub categories: Vec<ToolCategorySummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCategorySummary {
    pub source: String,
    pub category: String,
    pub tool_count: usize,
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolContextBudget {
    pub estimated_total_tool_tokens: usize,
    pub estimated_loaded_tool_tokens: usize,
    pub estimated_deferred_tool_tokens: usize,
    pub max_eager_tool_tokens: usize,
    pub max_loaded_tools: usize,
    pub max_loaded_tool_tokens: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolLoadingDecision {
    pub configured_mode: ToolLoadingMode,
    pub resolved_mode: ResolvedToolLoadingMode,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolListFilter {
    All,
    Loaded,
    Deferred,
    Disabled,
}

#[derive(Debug, Error)]
pub enum ToolSearchError {
    #[error("query must not be empty")]
    EmptyQuery,
    #[error("query length {actual} exceeds limit {limit}")]
    QueryTooLong { actual: usize, limit: usize },
    #[error("limit must be greater than zero")]
    BadLimit,
    #[error("invalid regex: {0}")]
    InvalidRegex(String),
    #[error("{0}")]
    InvalidInput(String),
}

#[derive(Clone)]
pub struct ToolCatalog {
    descriptors: BTreeMap<String, ToolDescriptor>,
    options: ToolSearchOptions,
    documents: BTreeMap<String, ToolSearchDocument>,
    document_frequency: HashMap<String, usize>,
}

impl ToolCatalog {
    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&ToolDescriptor> {
        self.descriptors.get(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.descriptors.keys().cloned().collect()
    }

    pub fn descriptors(&self) -> impl Iterator<Item = &ToolDescriptor> {
        self.descriptors.values()
    }

    pub fn preview(&self, name: &str) -> Option<ToolDescriptorPreview> {
        self.descriptors.get(name).map(preview_for)
    }

    pub fn list_previews(
        &self,
        filter: ToolListFilter,
        state: &LoadedToolState,
        options: &ToolSearchOptions,
    ) -> Vec<ToolDescriptorPreview> {
        let loaded = self.loaded_tool_names(state, options);
        self.descriptors
            .values()
            .filter(|descriptor| match filter {
                ToolListFilter::All => true,
                ToolListFilter::Loaded => loaded.contains(&descriptor.name),
                ToolListFilter::Deferred => descriptor.load_policy == ToolLoadPolicy::Deferred,
                ToolListFilter::Disabled => descriptor.load_policy == ToolLoadPolicy::Disabled,
            })
            .map(preview_for)
            .collect()
    }

    pub fn resolve_loading_mode(&self, options: &ToolSearchOptions) -> ToolLoadingDecision {
        if !options.enabled {
            return ToolLoadingDecision {
                configured_mode: options.mode.clone(),
                resolved_mode: ResolvedToolLoadingMode::Eager,
                reason: "tool search disabled".into(),
            };
        }
        match &options.mode {
            ToolLoadingMode::Eager => ToolLoadingDecision {
                configured_mode: options.mode.clone(),
                resolved_mode: ResolvedToolLoadingMode::Eager,
                reason: "configured eager".into(),
            },
            ToolLoadingMode::Search => ToolLoadingDecision {
                configured_mode: options.mode.clone(),
                resolved_mode: ResolvedToolLoadingMode::Search,
                reason: "configured search".into(),
            },
            ToolLoadingMode::Auto {
                threshold_percent_of_context,
                min_tool_count,
            } => {
                let permitted_count = self
                    .descriptors
                    .values()
                    .filter(|d| d.load_policy != ToolLoadPolicy::Disabled)
                    .count();
                if permitted_count > *min_tool_count {
                    return ToolLoadingDecision {
                        configured_mode: options.mode.clone(),
                        resolved_mode: ResolvedToolLoadingMode::Search,
                        reason: format!(
                            "tool count {permitted_count} exceeds min_tool_count {min_tool_count}"
                        ),
                    };
                }
                if let Some(context_tokens) = options.available_context_tokens {
                    let total = self.estimated_total_tokens();
                    let threshold =
                        context_tokens.saturating_mul(*threshold_percent_of_context as usize) / 100;
                    if threshold > 0 && total > threshold {
                        return ToolLoadingDecision {
                            configured_mode: options.mode.clone(),
                            resolved_mode: ResolvedToolLoadingMode::Search,
                            reason: format!(
                                "estimated tool tokens {total} exceed {threshold_percent_of_context}% context threshold {threshold}"
                            ),
                        };
                    }
                }
                ToolLoadingDecision {
                    configured_mode: options.mode.clone(),
                    resolved_mode: ResolvedToolLoadingMode::Eager,
                    reason: "catalog fits eager loading budget".into(),
                }
            }
        }
    }

    pub fn estimated_total_tokens(&self) -> usize {
        self.descriptors.values().map(|d| d.estimated_tokens).sum()
    }

    pub fn estimated_tokens_for<'a>(&self, names: impl IntoIterator<Item = &'a String>) -> usize {
        names
            .into_iter()
            .filter_map(|name| self.descriptors.get(name))
            .map(|d| d.estimated_tokens)
            .sum()
    }

    pub fn context_budget(
        &self,
        state: &LoadedToolState,
        options: &ToolSearchOptions,
    ) -> ToolContextBudget {
        let loaded = self.loaded_tool_names(state, options);
        let total = self.estimated_total_tokens();
        let loaded_tokens = self.estimated_tokens_for(&loaded);
        ToolContextBudget {
            estimated_total_tool_tokens: total,
            estimated_loaded_tool_tokens: loaded_tokens,
            estimated_deferred_tool_tokens: total.saturating_sub(loaded_tokens),
            max_eager_tool_tokens: options
                .available_context_tokens
                .unwrap_or(0)
                .saturating_mul(options.activation_threshold_percent() as usize)
                / 100,
            max_loaded_tools: options.max_loaded_tools,
            max_loaded_tool_tokens: options.max_loaded_tool_tokens,
        }
    }

    pub fn summary(&self) -> ToolCatalogSummary {
        let mut grouped: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
        for descriptor in self.descriptors.values() {
            if descriptor.load_policy == ToolLoadPolicy::Disabled {
                continue;
            }
            let source = descriptor.source.label();
            let category = descriptor
                .category
                .clone()
                .unwrap_or_else(|| "uncategorized".into());
            grouped
                .entry((source, category))
                .or_default()
                .push(descriptor.name.clone());
        }
        let categories = grouped
            .into_iter()
            .map(|((source, category), mut names)| {
                names.sort();
                ToolCategorySummary {
                    source,
                    category,
                    tool_count: names.len(),
                    examples: names.into_iter().take(3).collect(),
                }
            })
            .collect();
        ToolCatalogSummary {
            total_tools: self.descriptors.len(),
            eager_tools: self
                .descriptors
                .values()
                .filter(|d| d.load_policy == ToolLoadPolicy::Always)
                .count(),
            deferred_tools: self
                .descriptors
                .values()
                .filter(|d| d.load_policy == ToolLoadPolicy::Deferred)
                .count(),
            categories,
        }
    }

    pub fn compact_summary_text(&self) -> String {
        let summary = self.summary();
        let mut out = String::new();
        out.push_str("Available tool categories:\n");
        for category in summary.categories.iter().take(12) {
            let examples = if category.examples.is_empty() {
                String::new()
            } else {
                format!(": {}", category.examples.join(", "))
            };
            out.push_str(&format!(
                "- {} {} ({} tools{})\n",
                category.source, category.category, category.tool_count, examples
            ));
        }
        out.push_str("Use SearchTools when you need a tool that is not already loaded.");
        out
    }

    pub fn search(
        &self,
        query: ToolSearchQuery,
        strategy: ToolSearchStrategy,
        visibility: &ToolVisibilityContext,
    ) -> Result<ToolSearchResult, ToolSearchError> {
        self.validate_query(&query)?;
        let query_tokens = tokenize(&query.query);
        let mut matches = Vec::new();
        let regex = if strategy == ToolSearchStrategy::Regex {
            Some(
                Regex::new(&query.query)
                    .map_err(|e| ToolSearchError::InvalidRegex(e.to_string()))?,
            )
        } else {
            None
        };

        for descriptor in self.descriptors.values() {
            if descriptor.load_policy == ToolLoadPolicy::Disabled {
                continue;
            }
            if !query.include_risky && is_risky(descriptor.risk_profile) {
                continue;
            }
            if let Some(filter) = &query.source_filter {
                if !filter.matches(&descriptor.source) {
                    continue;
                }
            }
            let visibility_result = self.visibility(descriptor, visibility);
            if matches!(
                visibility_result,
                ToolVisibility::Denied | ToolVisibility::Hidden
            ) {
                continue;
            }
            let Some(document) = self.documents.get(&descriptor.name) else {
                continue;
            };
            let (score, reason) = match strategy {
                ToolSearchStrategy::Regex => {
                    let regex = regex.as_ref().expect("regex compiled above");
                    if regex.is_match(&document.source_text) {
                        (100.0, "regex match".to_string())
                    } else {
                        continue;
                    }
                }
                ToolSearchStrategy::Bm25 => {
                    let score = self.bm25_score(document, &query_tokens);
                    if score <= 0.0 {
                        continue;
                    }
                    (score, "lexical metadata match".to_string())
                }
                ToolSearchStrategy::Hybrid => {
                    let score = self.hybrid_score(document, &query_tokens);
                    if score <= 0.0 {
                        continue;
                    }
                    (score, hybrid_reason(document, &query_tokens))
                }
            };
            matches.push(ToolSearchMatch {
                tool_name: descriptor.name.clone(),
                score,
                reason,
                descriptor_preview: preview_for(descriptor),
                visibility: visibility_result,
                loaded: false,
            });
        }

        matches.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    let a_source = &a.descriptor_preview.source;
                    let b_source = &b.descriptor_preview.source;
                    a_source.priority().cmp(&b_source.priority())
                })
                .then_with(|| a.tool_name.cmp(&b.tool_name))
        });
        matches.truncate(query.limit);
        Ok(ToolSearchResult {
            query: query.query,
            matches,
        })
    }

    pub fn search_and_mark_loaded(
        &self,
        query: ToolSearchQuery,
        strategy: ToolSearchStrategy,
        visibility: &ToolVisibilityContext,
        state: &mut LoadedToolState,
        options: &ToolSearchOptions,
    ) -> Result<(ToolSearchResult, Vec<String>), ToolSearchError> {
        let mut result = self.search(query, strategy, visibility)?;
        let mut loaded_now = Vec::new();
        let mut loaded_names = self.loaded_tool_names(state, options);
        let mut loaded_tokens = self.estimated_tokens_for(&loaded_names);
        for m in &mut result.matches {
            if loaded_names.contains(&m.tool_name) {
                m.loaded = true;
                continue;
            }
            let Some(descriptor) = self.descriptors.get(&m.tool_name) else {
                continue;
            };
            let next_count = loaded_names.len().saturating_add(1);
            let next_tokens = loaded_tokens.saturating_add(descriptor.estimated_tokens);
            if next_count > options.max_loaded_tools || next_tokens > options.max_loaded_tool_tokens
            {
                m.loaded = false;
                continue;
            }
            state.discovered_this_run.insert(m.tool_name.clone());
            if options.persist_discovered_tools {
                state.discovered_this_session.insert(m.tool_name.clone());
            }
            loaded_names.insert(m.tool_name.clone());
            loaded_tokens = next_tokens;
            m.loaded = true;
            loaded_now.push(m.tool_name.clone());
        }
        Ok((result, loaded_now))
    }

    pub fn initial_loaded_state(&self, options: &ToolSearchOptions) -> LoadedToolState {
        let mut state = LoadedToolState::default();
        for descriptor in self.descriptors.values() {
            match descriptor.load_policy {
                ToolLoadPolicy::Always => {
                    state.always_loaded.insert(descriptor.name.clone());
                }
                ToolLoadPolicy::Disabled => {
                    state.disabled.insert(descriptor.name.clone());
                }
                ToolLoadPolicy::Deferred => {}
            }
        }
        for pattern in &options.always_loaded {
            for name in self.matching_names(pattern) {
                state.always_loaded.insert(name);
            }
        }
        for name in &options.preloaded_discovered_tools {
            if self.descriptors.contains_key(name) {
                state.discovered_this_session.insert(name.clone());
            }
        }
        state
    }

    pub fn loaded_tool_names(
        &self,
        state: &LoadedToolState,
        options: &ToolSearchOptions,
    ) -> BTreeSet<String> {
        let decision = self.resolve_loading_mode(options);
        let mut names = match decision.resolved_mode {
            ResolvedToolLoadingMode::Eager => self
                .descriptors
                .values()
                .filter(|d| d.load_policy != ToolLoadPolicy::Disabled)
                .map(|d| d.name.clone())
                .collect::<BTreeSet<_>>(),
            ResolvedToolLoadingMode::Search => {
                let mut out = BTreeSet::new();
                for descriptor in self.descriptors.values() {
                    if descriptor.load_policy == ToolLoadPolicy::Always {
                        out.insert(descriptor.name.clone());
                    }
                }
                out.extend(state.always_loaded.iter().cloned());
                out.extend(state.discovered_this_run.iter().cloned());
                out.extend(state.discovered_this_session.iter().cloned());
                out.extend(options.preloaded_discovered_tools.iter().cloned());
                if self.descriptors.contains_key(SEARCH_TOOLS_NAME) {
                    out.insert(SEARCH_TOOLS_NAME.into());
                }
                out
            }
        };
        names.retain(|name| {
            self.descriptors
                .get(name)
                .map(|d| d.load_policy != ToolLoadPolicy::Disabled)
                .unwrap_or(false)
                && !state.disabled.contains(name)
        });
        self.apply_loaded_budget(names, options)
    }

    pub fn deferred_tool_names(
        &self,
        state: &LoadedToolState,
        options: &ToolSearchOptions,
    ) -> BTreeSet<String> {
        let loaded = self.loaded_tool_names(state, options);
        self.descriptors
            .values()
            .filter(|d| d.load_policy == ToolLoadPolicy::Deferred && !loaded.contains(&d.name))
            .map(|d| d.name.clone())
            .collect()
    }

    fn validate_query(&self, query: &ToolSearchQuery) -> Result<(), ToolSearchError> {
        let trimmed = query.query.trim();
        if trimmed.is_empty() {
            return Err(ToolSearchError::EmptyQuery);
        }
        if query.query.chars().count() > self.options.regex_query_limit {
            return Err(ToolSearchError::QueryTooLong {
                actual: query.query.chars().count(),
                limit: self.options.regex_query_limit,
            });
        }
        if query.limit == 0 {
            return Err(ToolSearchError::BadLimit);
        }
        Ok(())
    }

    fn apply_loaded_budget(
        &self,
        names: BTreeSet<String>,
        options: &ToolSearchOptions,
    ) -> BTreeSet<String> {
        let mut kept = BTreeSet::new();
        let mut tokens = 0usize;
        let mut ordered: Vec<String> = names.into_iter().collect();
        ordered.sort_by(|a, b| {
            let a_always = self
                .descriptors
                .get(a)
                .map(|d| d.load_policy == ToolLoadPolicy::Always)
                .unwrap_or(false)
                || a == SEARCH_TOOLS_NAME;
            let b_always = self
                .descriptors
                .get(b)
                .map(|d| d.load_policy == ToolLoadPolicy::Always)
                .unwrap_or(false)
                || b == SEARCH_TOOLS_NAME;
            b_always.cmp(&a_always).then_with(|| a.cmp(b))
        });
        for name in ordered {
            let Some(descriptor) = self.descriptors.get(&name) else {
                continue;
            };
            let always =
                descriptor.load_policy == ToolLoadPolicy::Always || name == SEARCH_TOOLS_NAME;
            let next_count = kept.len().saturating_add(1);
            let next_tokens = tokens.saturating_add(descriptor.estimated_tokens);
            if always
                || (next_count <= options.max_loaded_tools
                    && next_tokens <= options.max_loaded_tool_tokens)
            {
                tokens = next_tokens;
                kept.insert(name);
            }
        }
        kept
    }

    fn matching_names(&self, pattern: &str) -> Vec<String> {
        if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
            if let Ok(glob) = Glob::new(pattern) {
                let matcher = glob.compile_matcher();
                return self
                    .descriptors
                    .keys()
                    .filter(|name| matcher.is_match(name.as_str()))
                    .cloned()
                    .collect();
            }
            Vec::new()
        } else if self.descriptors.contains_key(pattern) {
            vec![pattern.to_string()]
        } else {
            Vec::new()
        }
    }

    fn visibility(
        &self,
        descriptor: &ToolDescriptor,
        context: &ToolVisibilityContext,
    ) -> ToolVisibility {
        if descriptor.load_policy == ToolLoadPolicy::Disabled {
            return ToolVisibility::Hidden;
        }
        if context
            .disallowed_tools
            .iter()
            .any(|rule| rule.matches(&descriptor.name, &Value::Null))
        {
            return ToolVisibility::Denied;
        }
        let allowed_rule = context
            .allowed_tools
            .iter()
            .any(|rule| rule.matches(&descriptor.name, &Value::Null))
            || context
                .allowed_tool_names
                .iter()
                .any(|name| name == &descriptor.name);
        if allowed_rule {
            return ToolVisibility::Callable;
        }
        match context.permission_mode {
            PermissionMode::Plan => ToolVisibility::Hidden,
            PermissionMode::DontAsk => ToolVisibility::Hidden,
            PermissionMode::Default => match descriptor.permission_kind {
                PermissionKind::ReadOnly => ToolVisibility::Callable,
                PermissionKind::External => match context
                    .mcp_external_effect_policy
                    .unwrap_or(code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule)
                {
                    code_agent_mcp::McpExternalEffectPolicy::AllowWithApproval => {
                        ToolVisibility::RequiresApproval
                    }
                    code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule => {
                        ToolVisibility::Denied
                    }
                    code_agent_mcp::McpExternalEffectPolicy::DenyMutatingNameByDefault => {
                        if mutating_external(&descriptor.name) {
                            ToolVisibility::Denied
                        } else {
                            ToolVisibility::RequiresApproval
                        }
                    }
                },
                // Orchestration is treated like Execution: never auto-allowed.
                PermissionKind::Mutating
                | PermissionKind::Execution
                | PermissionKind::Orchestration => ToolVisibility::RequiresApproval,
            },
            PermissionMode::AcceptEdits => match descriptor.permission_kind {
                PermissionKind::ReadOnly | PermissionKind::Mutating => ToolVisibility::Callable,
                // Orchestration is NOT auto-allowed by AcceptEdits.
                PermissionKind::Execution | PermissionKind::Orchestration => {
                    ToolVisibility::RequiresApproval
                }
                PermissionKind::External => match context
                    .mcp_external_effect_policy
                    .unwrap_or(code_agent_mcp::McpExternalEffectPolicy::RequireExplicitAllowRule)
                {
                    code_agent_mcp::McpExternalEffectPolicy::AllowWithApproval => {
                        ToolVisibility::RequiresApproval
                    }
                    _ => ToolVisibility::Denied,
                },
            },
        }
    }

    fn bm25_score(&self, document: &ToolSearchDocument, query_tokens: &[String]) -> f32 {
        let mut score = 0.0;
        let doc_len = document.all_tokens.len().max(1) as f32;
        let avg_len = self
            .documents
            .values()
            .map(|d| d.all_tokens.len())
            .sum::<usize>()
            .max(1) as f32
            / self.documents.len().max(1) as f32;
        let mut term_counts: HashMap<&str, usize> = HashMap::new();
        for token in &document.all_tokens {
            *term_counts.entry(token.as_str()).or_default() += 1;
        }
        for token in query_tokens {
            let Some(tf) = term_counts.get(token.as_str()).copied() else {
                continue;
            };
            let df = self
                .document_frequency
                .get(token)
                .copied()
                .unwrap_or(0)
                .max(1) as f32;
            let total_docs = self.documents.len().max(1) as f32;
            let idf = ((total_docs - df + 0.5) / (df + 0.5) + 1.0).ln().max(0.1);
            let tf = tf as f32;
            let k1 = 1.2;
            let b = 0.75;
            score += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * doc_len / avg_len));
        }
        score
    }

    fn hybrid_score(&self, document: &ToolSearchDocument, query_tokens: &[String]) -> f32 {
        let mut score = self.bm25_score(document, query_tokens);
        let name_joined = document.name_tokens.join("_");
        let tag_set: BTreeSet<&str> = document.tags.iter().map(String::as_str).collect();
        for token in query_tokens {
            if document.name_tokens.iter().any(|t| t == token) {
                score += 7.5;
            }
            if name_joined.starts_with(token)
                || document.tool_name.to_ascii_lowercase().contains(token)
            {
                score += 3.0;
            }
            if document.aliases.iter().any(|alias| alias.contains(token)) {
                score += 4.0;
            }
            if tag_set.contains(token.as_str()) {
                score += 3.5;
            }
            if document
                .category
                .as_deref()
                .map(|category| category.contains(token))
                .unwrap_or(false)
            {
                score += 2.5;
            }
            if document.schema_tokens.iter().any(|t| t == token) {
                score += 1.5;
            }
        }
        score
    }
}

pub struct ToolCatalogBuilder {
    options: ToolSearchOptions,
    tools: Vec<Arc<dyn Tool>>,
    mcp_transports: BTreeMap<String, code_agent_mcp::McpTransportKind>,
}

impl ToolCatalogBuilder {
    pub fn new(options: ToolSearchOptions) -> Self {
        Self {
            options,
            tools: Vec::new(),
            mcp_transports: BTreeMap::new(),
        }
    }

    pub fn ingest_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn ingest_registry(mut self, registry: &ToolRegistry) -> Self {
        self.tools.extend(registry.iter());
        self
    }

    pub fn mcp_transport(
        mut self,
        server_name: impl Into<String>,
        transport: code_agent_mcp::McpTransportKind,
    ) -> Self {
        self.mcp_transports.insert(server_name.into(), transport);
        self
    }

    pub fn build(self) -> ToolCatalog {
        let mut descriptors = BTreeMap::new();
        for tool in self.tools {
            let descriptor =
                descriptor_from_tool(tool.as_ref(), &self.options, &self.mcp_transports);
            descriptors.insert(descriptor.name.clone(), descriptor);
        }
        let mut documents = BTreeMap::new();
        for descriptor in descriptors.values() {
            documents.insert(
                descriptor.name.clone(),
                ToolSearchDocument::from_descriptor(descriptor),
            );
        }
        let document_frequency = compute_document_frequency(documents.values());
        ToolCatalog {
            descriptors,
            options: self.options,
            documents,
            document_frequency,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolSearchDocument {
    pub tool_name: String,
    pub source_text: String,
    pub name_tokens: Vec<String>,
    pub description_tokens: Vec<String>,
    pub schema_tokens: Vec<String>,
    pub tags: Vec<String>,
    pub aliases: Vec<String>,
    pub category: Option<String>,
    all_tokens: Vec<String>,
}

impl ToolSearchDocument {
    fn from_descriptor(descriptor: &ToolDescriptor) -> Self {
        let source_label = descriptor.source.label();
        let input_text = schema_index_text(&descriptor.input_schema);
        let mut source_text = [
            descriptor.name.as_str(),
            source_label.as_str(),
            descriptor.description.as_str(),
            input_text.as_str(),
            descriptor.tags.join(" ").as_str(),
            descriptor.aliases.join(" ").as_str(),
            descriptor.category.as_deref().unwrap_or(""),
        ]
        .join(" ");
        source_text = source_text.to_ascii_lowercase();
        let name_tokens = tokenize(&descriptor.name);
        let description_tokens = tokenize(&descriptor.description);
        let schema_tokens = tokenize(&input_text);
        let tags: Vec<String> = descriptor
            .tags
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        let aliases: Vec<String> = descriptor
            .aliases
            .iter()
            .map(|t| t.to_ascii_lowercase())
            .collect();
        let category = descriptor.category.as_ref().map(|c| c.to_ascii_lowercase());
        let mut all_tokens = Vec::new();
        all_tokens.extend(name_tokens.iter().cloned());
        all_tokens.extend(description_tokens.iter().cloned());
        all_tokens.extend(schema_tokens.iter().cloned());
        all_tokens.extend(tags.iter().cloned());
        all_tokens.extend(aliases.iter().flat_map(|alias| tokenize(alias)));
        if let Some(category) = &category {
            all_tokens.extend(tokenize(category));
        }
        Self {
            tool_name: descriptor.name.clone(),
            source_text,
            name_tokens,
            description_tokens,
            schema_tokens,
            tags,
            aliases,
            category,
            all_tokens,
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchToolsInput {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    source_filter: Option<String>,
    #[serde(default)]
    include_risky: bool,
}

pub struct ToolSearchRuntime {
    catalog: ToolCatalog,
    state: Mutex<LoadedToolState>,
    options: ToolSearchOptions,
    strategy: ToolSearchStrategy,
    visibility: ToolVisibilityContext,
}

impl ToolSearchRuntime {
    pub fn new(
        catalog: ToolCatalog,
        state: LoadedToolState,
        options: ToolSearchOptions,
        visibility: ToolVisibilityContext,
    ) -> Self {
        let strategy = options.strategy;
        Self {
            catalog,
            state: Mutex::new(state),
            options,
            strategy,
            visibility,
        }
    }

    pub fn catalog(&self) -> &ToolCatalog {
        &self.catalog
    }

    pub async fn state_snapshot(&self) -> LoadedToolState {
        self.state.lock().await.clone()
    }

    pub async fn loaded_tool_names(&self) -> BTreeSet<String> {
        let state = self.state.lock().await;
        self.catalog.loaded_tool_names(&state, &self.options)
    }

    pub async fn deferred_tool_names(&self) -> BTreeSet<String> {
        let state = self.state.lock().await;
        self.catalog.deferred_tool_names(&state, &self.options)
    }

    pub async fn search_tools(
        &self,
        query: ToolSearchQuery,
    ) -> Result<(ToolSearchResult, Vec<String>, u64), ToolSearchError> {
        let started = Instant::now();
        let mut state = self.state.lock().await;
        let (result, loaded) = self.catalog.search_and_mark_loaded(
            query,
            self.strategy,
            &self.visibility,
            &mut state,
            &self.options,
        )?;
        let duration_ms = started.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
        Ok((result, loaded, duration_ms))
    }
}

pub struct SearchToolsTool {
    runtime: Arc<ToolSearchRuntime>,
}

impl SearchToolsTool {
    pub fn new(runtime: Arc<ToolSearchRuntime>) -> Self {
        Self { runtime }
    }
}

pub struct SearchToolsDefinitionTool {
    max_results: usize,
}

impl SearchToolsDefinitionTool {
    pub fn new(max_results: usize) -> Self {
        Self { max_results }
    }
}

#[async_trait]
impl Tool for SearchToolsDefinitionTool {
    fn name(&self) -> &str {
        SEARCH_TOOLS_NAME
    }

    fn description(&self) -> &str {
        "Search the available tool catalog and load relevant tools for the next model turn."
    }

    fn input_schema(&self) -> Value {
        search_tools_schema(self.max_results)
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    async fn execute(&self, _input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        Err(ToolError::Other(
            "SearchTools runtime is not attached".into(),
        ))
    }
}

#[async_trait]
impl Tool for SearchToolsTool {
    fn name(&self) -> &str {
        SEARCH_TOOLS_NAME
    }

    fn description(&self) -> &str {
        "Search the available tool catalog and load relevant tools for the next model turn."
    }

    fn input_schema(&self) -> Value {
        search_tools_schema(self.runtime.options.max_results)
    }

    fn permission_kind(&self) -> PermissionKind {
        PermissionKind::ReadOnly
    }

    async fn execute(&self, input: Value, _ctx: ToolContext) -> Result<ToolResult, ToolError> {
        let parsed: SearchToolsInput = serde_json::from_value(input.clone())
            .map_err(|e| ToolError::Other(format!("SearchTools input: {e}")))?;
        let source_filter = parsed
            .source_filter
            .as_deref()
            .map(ToolSourceFilter::parse)
            .transpose()
            .map_err(ToolError::Other)?;
        let limit = parsed
            .limit
            .unwrap_or(self.runtime.options.max_results)
            .min(self.runtime.options.max_results)
            .max(1);
        let query = ToolSearchQuery {
            query: parsed.query,
            limit,
            source_filter,
            include_risky: parsed.include_risky,
        };
        let (result, loaded, duration_ms) = self
            .runtime
            .search_tools(query)
            .await
            .map_err(|e| ToolError::Other(e.to_string()))?;
        let text = render_search_result_text(&result, &loaded);
        Ok(ToolResult {
            content: vec![ToolContent::Text(text)],
            is_error: false,
            metadata: json!({
                "tool_search": {
                    "query": cap_log_string(&result.query, 200),
                    "matches": result.matches,
                    "loaded": loaded,
                    "duration_ms": duration_ms
                }
            }),
        })
    }
}

fn search_tools_schema(max_results: usize) -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["query"],
        "properties": {
            "query": {
                "type": "string",
                "description": "Tool search query, such as 'github issue repository bug'."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": max_results,
                "description": "Maximum number of matching tools to return."
            },
            "source_filter": {
                "type": "string",
                "description": "Optional source filter: local, mcp, mcp:<server>, custom:<namespace>, or any."
            },
            "include_risky": {
                "type": "boolean",
                "description": "Whether to include high-risk or mutating external tools."
            }
        }
    })
}

fn descriptor_from_tool(
    tool: &dyn Tool,
    options: &ToolSearchOptions,
    mcp_transports: &BTreeMap<String, code_agent_mcp::McpTransportKind>,
) -> ToolDescriptor {
    let name = tool.name().to_string();
    let description = cap_descriptor_text(tool.description());
    let source = infer_source(&name, mcp_transports);
    let input_schema = redact_secret_values(tool.input_schema());
    let permission_kind = tool.permission_kind();
    let risk_profile = infer_risk_profile(&name, permission_kind);
    let aliases = aliases_for(&name, &source);
    let tags = tags_for(&name, &source, permission_kind, risk_profile);
    let category = infer_category(&name, &description, &source);
    let provider_metadata = redact_secret_values(provider_metadata_for(&name, &source));
    let estimated_tokens =
        estimate_tool_tokens(&name, &description, &input_schema, &provider_metadata);
    let load_policy = classify_load_policy(&name, &source, options);
    ToolDescriptor {
        name,
        source,
        description,
        input_schema,
        output_schema: None,
        permission_kind,
        risk_profile,
        tags,
        aliases,
        category,
        provider_metadata,
        estimated_tokens,
        load_policy,
    }
}

fn infer_source(
    name: &str,
    mcp_transports: &BTreeMap<String, code_agent_mcp::McpTransportKind>,
) -> ToolSource {
    if let Some(parts) = code_agent_mcp::parse_agent_tool_name(name) {
        let transport = mcp_transports
            .get(&parts.server_name)
            .copied()
            .unwrap_or(code_agent_mcp::McpTransportKind::Stdio);
        return ToolSource::Mcp {
            server_name: parts.server_name,
            remote_tool_name: parts.remote_tool_name,
            transport,
        };
    }
    ToolSource::Local
}

fn classify_load_policy(
    name: &str,
    source: &ToolSource,
    options: &ToolSearchOptions,
) -> ToolLoadPolicy {
    if pattern_list_matches(&options.disabled, name) {
        return ToolLoadPolicy::Disabled;
    }
    if pattern_list_matches(&options.always_loaded, name) {
        return ToolLoadPolicy::Always;
    }
    if pattern_list_matches(&options.deferred, name) {
        return ToolLoadPolicy::Deferred;
    }
    match source {
        ToolSource::Local => ToolLoadPolicy::Always,
        ToolSource::Mcp { .. } | ToolSource::Custom { .. } => ToolLoadPolicy::Deferred,
    }
}

fn pattern_list_matches(patterns: &[String], name: &str) -> bool {
    patterns
        .iter()
        .any(|pattern| pattern_matches(pattern, name))
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    if pattern.contains('*') || pattern.contains('?') || pattern.contains('[') {
        Glob::new(pattern)
            .map(|glob| glob.compile_matcher().is_match(name))
            .unwrap_or(false)
    } else {
        pattern == name
    }
}

fn provider_metadata_for(name: &str, source: &ToolSource) -> Value {
    match source {
        ToolSource::Local => Value::Null,
        ToolSource::Mcp {
            server_name,
            remote_tool_name,
            transport,
        } => json!({
            "mcp": {
                "agent_tool_name": name,
                "server_name": server_name,
                "remote_tool_name": remote_tool_name,
                "transport": transport,
                "checkpoint_coverage": "not_applicable"
            }
        }),
        ToolSource::Custom { namespace } => json!({"custom": {"namespace": namespace}}),
    }
}

fn infer_risk_profile(name: &str, permission_kind: PermissionKind) -> ToolRiskProfile {
    match permission_kind {
        PermissionKind::ReadOnly => ToolRiskProfile::Low,
        PermissionKind::Mutating => ToolRiskProfile::Medium,
        // Orchestration spawns child runs that may themselves do high-risk
        // work; classify it as `High` to surface the autonomy implications.
        PermissionKind::Execution | PermissionKind::Orchestration => ToolRiskProfile::High,
        PermissionKind::External => {
            if mutating_external(name) {
                ToolRiskProfile::MutatingExternal
            } else {
                ToolRiskProfile::External
            }
        }
    }
}

fn mutating_external(name: &str) -> bool {
    code_agent_mcp::parse_agent_tool_name(name)
        .map(|parts| code_agent_mcp::mutating_tool_name(&parts.remote_tool_name))
        .unwrap_or(false)
}

fn aliases_for(name: &str, source: &ToolSource) -> Vec<String> {
    let mut aliases = vec![name.to_string()];
    if let ToolSource::Mcp {
        server_name,
        remote_tool_name,
        ..
    } = source
    {
        aliases.push(remote_tool_name.clone());
        aliases.push(format!("{server_name}_{remote_tool_name}"));
        aliases.push(format!("{server_name}:{remote_tool_name}"));
    }
    aliases.sort();
    aliases.dedup();
    aliases
}

fn tags_for(
    name: &str,
    source: &ToolSource,
    permission_kind: PermissionKind,
    risk_profile: ToolRiskProfile,
) -> Vec<String> {
    let mut tags = BTreeSet::new();
    for token in tokenize(name) {
        tags.insert(token);
    }
    match source {
        ToolSource::Local => {
            tags.insert("local".into());
            tags.insert("workspace".into());
        }
        ToolSource::Mcp { server_name, .. } => {
            tags.insert("mcp".into());
            tags.insert(server_name.to_ascii_lowercase());
        }
        ToolSource::Custom { namespace } => {
            tags.insert("custom".into());
            tags.insert(namespace.to_ascii_lowercase());
        }
    }
    tags.insert(format!("{permission_kind:?}").to_ascii_lowercase());
    tags.insert(format!("{risk_profile:?}").to_ascii_lowercase());
    tags.into_iter().collect()
}

fn infer_category(name: &str, description: &str, source: &ToolSource) -> Option<String> {
    let mut text = name.to_ascii_lowercase();
    text.push(' ');
    text.push_str(&description.to_ascii_lowercase());
    match source {
        ToolSource::Local => {
            if text.contains("read") || text.contains("grep") || text.contains("glob") {
                Some("workspace_search".into())
            } else if text.contains("edit") || text.contains("write") {
                Some("workspace_edit".into())
            } else if text.contains("bash") {
                Some("execution".into())
            } else {
                Some("workspace".into())
            }
        }
        ToolSource::Mcp { .. } | ToolSource::Custom { .. } => {
            if text.contains("issue") {
                Some("issues".into())
            } else if text.contains("pull") || text.contains("pr") {
                Some("pull_requests".into())
            } else if text.contains("doc") {
                Some("documentation".into())
            } else if text.contains("sentry") || text.contains("error") || text.contains("event") {
                Some("errors".into())
            } else if text.contains("log") || text.contains("splunk") {
                Some("logs".into())
            } else if text.contains("dashboard") || text.contains("grafana") {
                Some("dashboards".into())
            } else if text.contains("message") || text.contains("slack") {
                Some("messages".into())
            } else {
                tokenize(name).into_iter().next()
            }
        }
    }
}

fn estimate_tool_tokens(
    name: &str,
    description: &str,
    input_schema: &Value,
    metadata: &Value,
) -> usize {
    let schema_len = serde_json::to_string(input_schema)
        .unwrap_or_default()
        .len();
    let metadata_len = if metadata.is_null() {
        0
    } else {
        serde_json::to_string(metadata).unwrap_or_default().len()
    };
    ((name.len() + description.len() + schema_len + metadata_len) / 4)
        .saturating_add(24)
        .max(32)
}

fn preview_for(descriptor: &ToolDescriptor) -> ToolDescriptorPreview {
    ToolDescriptorPreview {
        name: descriptor.name.clone(),
        source: descriptor.source.clone(),
        description: cap_descriptor_text(&descriptor.description),
        category: descriptor.category.clone(),
        tags: descriptor.tags.iter().take(8).cloned().collect(),
        permission_kind: descriptor.permission_kind,
        risk_profile: descriptor.risk_profile,
        input_properties: input_properties(&descriptor.input_schema)
            .into_iter()
            .take(12)
            .collect(),
    }
}

fn input_properties(schema: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(props) = schema.get("properties").and_then(Value::as_object) {
        for key in props.keys() {
            out.push(key.clone());
        }
    }
    out.sort();
    out
}

fn schema_index_text(schema: &Value) -> String {
    fn walk(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    out.push(key.clone());
                    if key == "description" || key == "title" {
                        if let Some(s) = value.as_str() {
                            out.push(s.to_string());
                        }
                    }
                    walk(value, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, out);
                }
            }
            Value::String(s) => out.push(s.clone()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(schema, &mut out);
    out.join(" ")
}

fn compute_document_frequency<'a>(
    documents: impl Iterator<Item = &'a ToolSearchDocument>,
) -> HashMap<String, usize> {
    let mut df = HashMap::new();
    for document in documents {
        let unique: BTreeSet<&String> = document.all_tokens.iter().collect();
        for token in unique {
            *df.entry(token.clone()).or_default() += 1;
        }
    }
    df
}

fn hybrid_reason(document: &ToolSearchDocument, query_tokens: &[String]) -> String {
    if query_tokens
        .iter()
        .any(|token| document.name_tokens.iter().any(|t| t == token))
    {
        "tool name match".into()
    } else if query_tokens
        .iter()
        .any(|token| document.tags.iter().any(|t| t == token))
    {
        "tag match".into()
    } else if query_tokens
        .iter()
        .any(|token| document.schema_tokens.iter().any(|t| t == token))
    {
        "input schema match".into()
    } else {
        "metadata match".into()
    }
}

fn tokenize(input: &str) -> Vec<String> {
    input
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect()
}

fn normalized(input: &str) -> String {
    input
        .chars()
        .filter(|c| *c != '_' && *c != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn is_risky(risk: ToolRiskProfile) -> bool {
    matches!(
        risk,
        ToolRiskProfile::High | ToolRiskProfile::MutatingExternal | ToolRiskProfile::Unknown
    )
}

fn cap_descriptor_text(s: &str) -> String {
    cap_string(s.replace('\n', " "), 500)
}

fn cap_log_string(s: &str, limit: usize) -> String {
    cap_string(s.replace('\n', " "), limit)
}

fn cap_string(mut s: String, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s;
    }
    let mut end = 0;
    for (idx, _) in s.char_indices().take(limit.saturating_sub(1)) {
        end = idx;
    }
    if end > 0 {
        s.truncate(end);
    }
    s.push_str("...");
    s
}

fn redact_secret_values(value: Value) -> Value {
    fn walk(value: Value, secret_context: bool) -> Value {
        match value {
            Value::Object(map) => {
                let mut out = serde_json::Map::new();
                for (key, value) in map {
                    let secret = secret_context || secret_like_key(&key);
                    out.insert(key, walk(value, secret));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(
                items
                    .into_iter()
                    .map(|item| walk(item, secret_context))
                    .collect(),
            ),
            Value::String(_) | Value::Number(_) | Value::Bool(_) if secret_context => {
                Value::String("[REDACTED]".into())
            }
            other => other,
        }
    }
    walk(value, false)
}

fn secret_like_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("secret")
        || key.contains("token")
        || key.contains("password")
        || key.contains("authorization")
        || key.contains("api_key")
        || key == "apikey"
        || key == "key"
}

fn render_search_result_text(result: &ToolSearchResult, loaded: &[String]) -> String {
    let mut out = format!(
        "Tool search query {:?} returned {} match(es).",
        result.query,
        result.matches.len()
    );
    if !loaded.is_empty() {
        out.push_str("\nLoaded tools:");
        for name in loaded {
            out.push_str(&format!("\n- {name}"));
        }
    }
    if !result.matches.is_empty() {
        out.push_str("\nMatches:");
        for m in &result.matches {
            out.push_str(&format!(
                "\n- {} ({:.2}, {:?}, loaded={}) - {}",
                m.tool_name, m.score, m.visibility, m.loaded, m.reason
            ));
        }
    }
    out
}

pub fn tool_schema_from_tool(tool: &dyn Tool) -> crate::provider::ToolSchema {
    crate::provider::ToolSchema {
        name: tool.name().to_string(),
        description: tool.description().to_string(),
        input_schema: tool.input_schema(),
    }
}
