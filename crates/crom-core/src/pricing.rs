//! Versioned nominal pricing table (M17.1, #282).
//!
//! Prices are data (`crates/crom-core/pricing/prices.toml`), never Rust
//! constants. A cost computed here is a nominal list-price estimate, not a
//! billed amount; any unknown input yields an unknown cost with a reason.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::telemetry::{CostUnknownReason, ModelPricing};
use crate::usage::TokenUsage;

/// The checked-in pricing table.
pub const BUILTIN_PRICING_TOML: &str = include_str!("../pricing/prices.toml");

/// Environment variable naming a pricing override file.
pub const PRICING_FILE_ENV: &str = "CROM_PRICING_FILE";

#[derive(Debug, thiserror::Error)]
pub enum PricingError {
    #[error("reading pricing file {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error("parsing pricing table: {0}")]
    Parse(String),
    #[error("invalid pricing table: {0}")]
    Invalid(String),
}

/// One priced model. Prices are USD per 1M tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    /// Provider profile id (`anthropic`, `openai`, `deepseek`, ...).
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub input: f64,
    pub output: f64,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    pub source_url: String,
    /// `YYYY-MM-DD` the price was read from `source_url`.
    pub retrieved_at: String,
}

impl PriceEntry {
    pub fn model_pricing(&self) -> ModelPricing {
        ModelPricing {
            input_per_million: self.input,
            output_per_million: self.output,
            cache_creation_per_million: self.cache_write,
            cache_read_per_million: self.cache_read,
        }
    }

    fn matches(&self, provider: &str, model: &str) -> bool {
        self.provider == provider
            && (self.model == model || self.aliases.iter().any(|a| a == model))
    }

    fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.model.as_str()).chain(self.aliases.iter().map(String::as_str))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PricingTable {
    /// Table version, conventionally the retrieval date. Recorded in eval
    /// results as `pricing_source_version`.
    pub version: String,
    pub currency: String,
    #[serde(default)]
    pub models: Vec<PriceEntry>,
}

/// A nominal cost, or the reason it is unknown. Exactly one field is set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub cost_usd: Option<f64>,
    pub unknown_reason: Option<CostUnknownReason>,
}

impl CostEstimate {
    fn known(cost: f64) -> Self {
        Self {
            cost_usd: Some(cost),
            unknown_reason: None,
        }
    }

    fn unknown(reason: CostUnknownReason) -> Self {
        Self {
            cost_usd: None,
            unknown_reason: Some(reason),
        }
    }
}

fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

impl PricingTable {
    /// The checked-in table. Panics only if the compiled-in file is invalid,
    /// which the unit tests rule out.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN_PRICING_TOML).expect("built-in pricing table is valid")
    }

    pub fn parse(text: &str) -> Result<Self, PricingError> {
        let table: PricingTable =
            toml::from_str(text).map_err(|e| PricingError::Parse(e.to_string()))?;
        table.validate()?;
        Ok(table)
    }

    pub fn load_file(path: &Path) -> Result<Self, PricingError> {
        let text = std::fs::read_to_string(path).map_err(|source| PricingError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text)
    }

    /// The built-in table, merged with the file named by
    /// [`PRICING_FILE_ENV`] when that variable is set.
    pub fn load_default() -> Result<Self, PricingError> {
        let builtin = Self::builtin();
        match std::env::var_os(PRICING_FILE_ENV) {
            Some(p) if !p.is_empty() => Ok(builtin.with_override(Self::load_file(Path::new(&p))?)),
            _ => Ok(builtin),
        }
    }

    /// Merge `over` on top of `self`: an override entry replaces every base
    /// entry for the same provider that shares any model name or alias. The
    /// merged version is `<override>+<base>`.
    pub fn with_override(mut self, over: PricingTable) -> Self {
        for entry in &over.models {
            self.models.retain(|base| {
                base.provider != entry.provider
                    || !base.names().any(|n| entry.names().any(|m| m == n))
            });
        }
        self.models.extend(over.models);
        self.version = format!("{}+{}", over.version, self.version);
        self
    }

    fn validate(&self) -> Result<(), PricingError> {
        let invalid = |m: String| Err(PricingError::Invalid(m));
        if self.version.trim().is_empty() {
            return invalid("`version` is empty".into());
        }
        if self.currency != "USD" {
            return invalid(format!("currency must be USD, got {:?}", self.currency));
        }
        let mut seen: Vec<(&str, &str)> = Vec::new();
        for e in &self.models {
            let id = format!("{}/{}", e.provider, e.model);
            let prices = [Some(e.input), Some(e.output), e.cache_write, e.cache_read];
            if prices.iter().flatten().any(|p| !p.is_finite() || *p < 0.0) {
                return invalid(format!("{id}: prices must be finite and >= 0"));
            }
            if !is_iso_date(&e.retrieved_at) {
                return invalid(format!("{id}: retrieved_at must be YYYY-MM-DD"));
            }
            if !e.source_url.starts_with("https://") {
                return invalid(format!("{id}: source_url must be an https URL"));
            }
            for name in e.names() {
                if seen.contains(&(e.provider.as_str(), name)) {
                    return invalid(format!("duplicate model {}/{name}", e.provider));
                }
                seen.push((e.provider.as_str(), name));
            }
        }
        Ok(())
    }

    pub fn lookup(&self, provider: &str, model: &str) -> Option<&PriceEntry> {
        self.models.iter().find(|e| e.matches(provider, model))
    }

    /// Nominal cost of `usage` for `provider`/`model`. `usage == None`
    /// (no round trip, or one with no usage) is unknown, never zero.
    pub fn estimate(
        &self,
        provider: &str,
        model: &str,
        usage: Option<&TokenUsage>,
    ) -> CostEstimate {
        let Some(entry) = self.lookup(provider, model) else {
            return CostEstimate::unknown(CostUnknownReason::NoPricingEntry);
        };
        let Some(usage) = usage else {
            return CostEstimate::unknown(CostUnknownReason::UsageUnknown);
        };
        match entry.model_pricing().estimate_explained(usage) {
            Ok(c) => CostEstimate::known(c),
            Err(r) => CostEstimate::unknown(r),
        }
    }
}

impl Default for PricingTable {
    fn default() -> Self {
        Self::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(i: u64, o: u64, cw: Option<u64>, cr: Option<u64>) -> TokenUsage {
        TokenUsage {
            input_tokens: Some(i),
            output_tokens: Some(o),
            cache_creation_input_tokens: cw,
            cache_read_input_tokens: cr,
            reasoning_tokens: None,
        }
    }

    const SMALL: &str = r#"
version = "2026-01-01"
currency = "USD"
[[models]]
provider = "p"
model = "m"
aliases = ["m-alias"]
input = 1.0
output = 2.0
source_url = "https://example.com/pricing"
retrieved_at = "2026-01-01"
"#;

    // The checked-in table parses, is dated, and every entry carries a source.
    #[test]
    fn builtin_table_is_valid_and_dated() {
        let t = PricingTable::builtin();
        assert!(is_iso_date(&t.version), "version {:?}", t.version);
        assert!(!t.models.is_empty());
        for e in &t.models {
            assert!(e.source_url.starts_with("https://"));
            assert!(is_iso_date(&e.retrieved_at));
        }
    }

    // Every built-in provider profile default model that is priced resolves by exact id.
    #[test]
    fn builtin_lookup_by_provider_and_model() {
        let t = PricingTable::builtin();
        assert!(t.lookup("anthropic", "claude-sonnet-4-6").is_some());
        assert!(t.lookup("openai", "gpt-4.1").is_some());
        assert!(t.lookup("openai", "claude-sonnet-4-6").is_none());
        assert!(t.lookup("deepseek", "deepseek-v4-flash").is_some(), "alias");
    }

    // A known price and fully known usage produce a nominal cost.
    #[test]
    fn estimate_known_cost() {
        let t = PricingTable::parse(SMALL).unwrap();
        let est = t.estimate("p", "m-alias", Some(&usage(1_000_000, 500_000, None, None)));
        assert_eq!(est.cost_usd, Some(2.0));
        assert_eq!(est.unknown_reason, None);
    }

    // Missing price or missing usage gives None with a reason, never zero.
    #[test]
    fn estimate_unknown_has_reason() {
        let t = PricingTable::parse(SMALL).unwrap();
        let u = usage(1, 1, None, None);
        let no_price = t.estimate("p", "other", Some(&u));
        assert_eq!(no_price.cost_usd, None);
        assert_eq!(
            no_price.unknown_reason,
            Some(CostUnknownReason::NoPricingEntry)
        );
        let no_usage = t.estimate("p", "m", None);
        assert_eq!(
            no_usage.unknown_reason,
            Some(CostUnknownReason::UsageUnknown)
        );
        let zero_usage = t.estimate("p", "m", Some(&usage(0, 0, None, None)));
        assert_eq!(
            zero_usage.cost_usd,
            Some(0.0),
            "reported zero is a real zero"
        );
    }

    // A priced cache quantity that is unreported makes the cost unknown.
    #[test]
    fn estimate_unreported_cache_is_unknown() {
        let t = PricingTable::builtin();
        let est = t.estimate(
            "anthropic",
            "claude-sonnet-4-6",
            Some(&usage(10, 10, Some(0), None)),
        );
        assert_eq!(
            est.unknown_reason,
            Some(CostUnknownReason::CacheTokensUnknown)
        );
        let est = t.estimate(
            "anthropic",
            "claude-sonnet-4-6",
            Some(&usage(
                1_000_000,
                1_000_000,
                Some(1_000_000),
                Some(1_000_000),
            )),
        );
        let c = est.cost_usd.unwrap();
        assert!((c - (3.0 + 15.0 + 3.75 + 0.30)).abs() < 1e-9, "{c}");
    }

    // An override replaces matching entries, adds new ones, and composes the version.
    #[test]
    fn override_replaces_and_versions() {
        let base = PricingTable::parse(SMALL).unwrap();
        let over = PricingTable::parse(
            &SMALL
                .replace("2026-01-01\"\ncurrency", "local-1\"\ncurrency")
                .replace(
                    "model = \"m\"\naliases = [\"m-alias\"]",
                    "model = \"m-alias\"",
                )
                .replace("input = 1.0", "input = 5.0"),
        )
        .unwrap();
        let merged = base.with_override(over);
        assert_eq!(merged.version, "local-1+2026-01-01");
        assert_eq!(merged.models.len(), 1);
        assert_eq!(merged.lookup("p", "m-alias").unwrap().input, 5.0);
        assert!(merged.lookup("p", "m").is_none(), "replaced entry is gone");
    }

    // Validation rejects non-USD, negative prices, undated entries, and duplicates.
    #[test]
    fn validation_rejects_bad_tables() {
        for bad in [
            SMALL.replace("USD", "EUR"),
            SMALL.replace("input = 1.0", "input = -1.0"),
            SMALL.replace(
                "retrieved_at = \"2026-01-01\"",
                "retrieved_at = \"recently\"",
            ),
            SMALL.replace("https://example.com", "http://example.com"),
            SMALL.replace("aliases = [\"m-alias\"]", "aliases = [\"m\"]"),
            SMALL.replace("input = 1.0", "input = 1.0\nbogus = 1"),
        ] {
            assert!(PricingTable::parse(&bad).is_err(), "accepted:\n{bad}");
        }
    }

    // CROM_PRICING_FILE merges a user file over the built-in table.
    #[test]
    fn load_default_honors_env_override() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prices.toml");
        std::fs::write(&path, SMALL).unwrap();
        let t =
            temp_env::with_var(PRICING_FILE_ENV, Some(&path), PricingTable::load_default).unwrap();
        assert!(t.lookup("p", "m").is_some());
        assert!(t.lookup("openai", "gpt-4.1").is_some());
        assert!(t.version.starts_with("2026-01-01+"));
    }
}
